//! One-time browser geometry setup before publishing a browser workspace.
//! No page script evaluation, DOM mutation, emulation, or user input is exposed.
use super::BrowserWorkspaceError;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    tungstenite::{protocol::WebSocketConfig, Message},
    MaybeTlsStream, WebSocketStream,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Viewport {
    pub width: u32,
    pub height: u32,
}
fn error(message: impl Into<String>) -> BrowserWorkspaceError {
    BrowserWorkspaceError::Launch(message.into())
}
pub(super) fn parse(
    raw: Option<&str>,
    bound: bool,
) -> Result<Option<Viewport>, BrowserWorkspaceError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if !cfg!(target_os = "linux") || !bound {
        return Err(error(
            "explicit browser viewport requires a daemon-created Linux display",
        ));
    }
    let Some((w, h)) = raw.split_once('x') else {
        return Err(error("viewport must be WIDTHxHEIGHT"));
    };
    let width = w
        .parse::<u32>()
        .map_err(|_| error("invalid viewport width"))?;
    let height = h
        .parse::<u32>()
        .map_err(|_| error("invalid viewport height"))?;
    if raw != format!("{width}x{height}")
        || !(256..=3840).contains(&width)
        || !(144..=2048).contains(&height)
    {
        return Err(error(
            "viewport dimensions exceed their canonical bounded range",
        ));
    }
    Ok(Some(Viewport { width, height }))
}
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
struct Client {
    socket: Socket,
    next: u64,
    total: usize,
}
impl Client {
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, BrowserWorkspaceError> {
        self.next += 1;
        self.socket
            .send(Message::Text(
                json!({"id":self.next,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| error("browser viewport CDP send failed"))?;
        for _ in 0..64 {
            let message = self
                .socket
                .next()
                .await
                .ok_or_else(|| error("browser viewport CDP closed"))?
                .map_err(|_| error("browser viewport CDP receive failed"))?;
            match message {
                Message::Text(text) => {
                    self.total = self.total.saturating_add(text.len());
                    if self.total > 4 * 1024 * 1024 {
                        return Err(error("browser viewport CDP exceeded total byte limit"));
                    }
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|_| error("invalid browser viewport CDP JSON"))?;
                    if value.get("id").is_none() {
                        continue;
                    }
                    if value["id"] != self.next || value.get("error").is_some() {
                        return Err(error("browser viewport CDP request failed or mismatched"));
                    }
                    return value
                        .get("result")
                        .cloned()
                        .ok_or_else(|| error("browser viewport CDP result missing"));
                }
                Message::Ping(bytes) => self
                    .socket
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| error("browser viewport CDP pong failed"))?,
                _ => return Err(error("unexpected browser viewport CDP frame")),
            }
        }
        Err(error("browser viewport CDP message limit exceeded"))
    }
}
pub(super) async fn configure(
    port: u16,
    url: &str,
    target: &str,
    viewport: Viewport,
) -> Result<(), BrowserWorkspaceError> {
    if !super::exact_loopback_websocket_url(url, port, &format!("/devtools/page/{target}")) {
        return Err(error(
            "browser viewport endpoint is not the exact owned page",
        ));
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        configure_inner(url, target, viewport),
    )
    .await
    .map_err(|_| error("browser viewport setup exceeded 10 seconds"))?
}
async fn configure_inner(
    url: &str,
    target: &str,
    want: Viewport,
) -> Result<(), BrowserWorkspaceError> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(1024 * 1024))
        .max_frame_size(Some(1024 * 1024));
    let (socket, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), true)
        .await
        .map_err(|_| error("browser viewport CDP connect failed"))?;
    let mut client = Client {
        socket,
        next: 0,
        total: 0,
    };
    // A pinned extension can open its first-run tab while CDP is becoming
    // ready. Background tabs retain stale layout metrics when the OS window
    // resizes. Activate only this already-authenticated page during pre-publication
    // geometry setup; do not emulate a viewport or mutate page content.
    client.call("Page.bringToFront", json!({})).await?;
    let window = client
        .call("Browser.getWindowForTarget", json!({"targetId":target}))
        .await?;
    let id = window["windowId"]
        .as_i64()
        .filter(|n| *n >= 0)
        .ok_or_else(|| error("browser viewport window missing"))?;
    client
        .call(
            "Browser.setWindowBounds",
            json!({"windowId":id,"bounds":{"windowState":"normal"}}),
        )
        .await?;
    let mut width = i64::from(want.width);
    let mut height = i64::from(want.height) + 120;
    let mut samples = Vec::new();
    for _ in 0..20 {
        if !(256..=4096).contains(&width) || !(144..=2304).contains(&height) {
            return Err(error(format!(
                "browser viewport outer window exceeded bounds: {samples:?}"
            )));
        }
        client
            .call(
                "Browser.setWindowBounds",
                json!({"windowId":id,"bounds":{"left":0,"top":0,"width":width,"height":height}}),
            )
            .await?;
        client.call("Page.bringToFront", json!({})).await?;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let metrics = client.call("Page.getLayoutMetrics", json!({})).await?;
        let css = &metrics["cssLayoutViewport"];
        let device = &metrics["layoutViewport"];
        let actual_width = css["clientWidth"]
            .as_i64()
            .ok_or_else(|| error("browser viewport CSS width missing"))?;
        let actual_height = css["clientHeight"]
            .as_i64()
            .ok_or_else(|| error("browser viewport CSS height missing"))?;
        let bounds = client
            .call("Browser.getWindowBounds", json!({"windowId":id}))
            .await?;
        let outer_width = bounds["bounds"]["width"]
            .as_i64()
            .ok_or_else(|| error("browser outer width missing"))?;
        let outer_height = bounds["bounds"]["height"]
            .as_i64()
            .ok_or_else(|| error("browser outer height missing"))?;
        if !(256..=4096).contains(&outer_width) || !(144..=2304).contains(&outer_height) {
            return Err(error("browser window reported invalid outer bounds"));
        }
        samples.push((
            width,
            height,
            actual_width,
            actual_height,
            outer_width,
            outer_height,
        ));
        if actual_width <= 0 || actual_height <= 0 {
            continue;
        }
        if actual_width == i64::from(want.width) && actual_height == i64::from(want.height) {
            if device["clientWidth"] != css["clientWidth"]
                || device["clientHeight"] != css["clientHeight"]
            {
                return Err(error("browser viewport requires device scale one"));
            }
            return Ok(());
        }
        // Resize acknowledgement and tab layout are asynchronous. Use the
        // observed outer geometry, not the prior requested bounds: accumulating
        // deltas from a background tab can otherwise grow the window endlessly.
        width = outer_width + i64::from(want.width) - actual_width;
        height = outer_height + i64::from(want.height) - actual_height;
    }
    Err(error(
        "browser did not reach the exact requested browser viewport",
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_viewport_is_closed_bounded_and_never_targets_user_display() {
        assert_eq!(parse(None, false).unwrap(), None);
        for raw in [
            "0x768",
            "1024x0",
            "0400x600",
            "+400x600",
            "4096x3000",
            "400x600x1",
            "400x600 ",
        ] {
            assert!(parse(Some(raw), true).is_err());
        }
        assert!(parse(Some("1024x768"), false).is_err());
        #[cfg(target_os = "linux")]
        assert_eq!(
            parse(Some("1024x768"), true).unwrap(),
            Some(Viewport {
                width: 1024,
                height: 768
            })
        );
    }
    #[tokio::test]
    async fn activates_only_the_bound_page_before_measuring_native_geometry() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut activations = 0;
            let mut methods = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let v: Value = serde_json::from_str(&text).unwrap();
                let method = v["method"].as_str().unwrap();
                methods.push(method.to_owned());
                let result = match method {
                    "Page.bringToFront" => {
                        activations += 1;
                        json!({})
                    }
                    "Browser.getWindowForTarget" => {
                        assert!(activations > 0);
                        assert_eq!(v["params"]["targetId"], "owned-target");
                        json!({"windowId":7})
                    }
                    "Browser.getWindowBounds" => {
                        assert_eq!(v["params"]["windowId"], 7);
                        json!({"bounds":{"width":1024,"height":888}})
                    }
                    "Browser.setWindowBounds" => {
                        assert_eq!(v["params"]["windowId"], 7);
                        json!({})
                    }
                    "Page.getLayoutMetrics" => {
                        assert!(activations >= 2);
                        json!({"layoutViewport":{"clientWidth":1024,"clientHeight":768},
                            "cssLayoutViewport":{"clientWidth":1024,"clientHeight":768}})
                    }
                    other => panic!("unexpected geometry operation: {other}"),
                };
                ws.send(Message::Text(
                    json!({"id":v["id"],"result":result}).to_string().into(),
                ))
                .await
                .unwrap();
                if method == "Browser.getWindowBounds" {
                    break;
                }
            }
            assert_eq!(
                methods.first().map(String::as_str),
                Some("Page.bringToFront")
            );
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            configure_inner(
                &format!("ws://127.0.0.1:{port}/devtools/page/owned-target"),
                "owned-target",
                Viewport {
                    width: 1024,
                    height: 768,
                },
            )
            .await
            .unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
}
