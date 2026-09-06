//! One-time browser geometry setup before publishing a proof workspace.
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
            "explicit proof viewport requires a daemon-created Linux display",
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
            .map_err(|_| error("proof viewport CDP send failed"))?;
        for _ in 0..64 {
            let message = self
                .socket
                .next()
                .await
                .ok_or_else(|| error("proof viewport CDP closed"))?
                .map_err(|_| error("proof viewport CDP receive failed"))?;
            match message {
                Message::Text(text) => {
                    self.total = self.total.saturating_add(text.len());
                    if self.total > 4 * 1024 * 1024 {
                        return Err(error("proof viewport CDP exceeded total byte limit"));
                    }
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|_| error("invalid proof viewport CDP JSON"))?;
                    if value.get("id").is_none() {
                        continue;
                    }
                    if value["id"] != self.next || value.get("error").is_some() {
                        return Err(error("proof viewport CDP request failed or mismatched"));
                    }
                    return value
                        .get("result")
                        .cloned()
                        .ok_or_else(|| error("proof viewport CDP result missing"));
                }
                Message::Ping(bytes) => self
                    .socket
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| error("proof viewport CDP pong failed"))?,
                _ => return Err(error("unexpected proof viewport CDP frame")),
            }
        }
        Err(error("proof viewport CDP message limit exceeded"))
    }
}
pub(super) async fn configure(
    port: u16,
    url: &str,
    target: &str,
    viewport: Viewport,
) -> Result<(), BrowserWorkspaceError> {
    if !super::exact_loopback_websocket_url(url, port, &format!("/devtools/page/{target}")) {
        return Err(error("proof viewport endpoint is not the exact owned page"));
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        configure_inner(url, target, viewport),
    )
    .await
    .map_err(|_| error("proof viewport setup exceeded 10 seconds"))?
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
        .map_err(|_| error("proof viewport CDP connect failed"))?;
    let mut client = Client {
        socket,
        next: 0,
        total: 0,
    };
    let window = client
        .call("Browser.getWindowForTarget", json!({"targetId":target}))
        .await?;
    let id = window["windowId"]
        .as_i64()
        .filter(|n| *n >= 0)
        .ok_or_else(|| error("proof viewport window missing"))?;
    client
        .call(
            "Browser.setWindowBounds",
            json!({"windowId":id,"bounds":{"windowState":"normal"}}),
        )
        .await?;
    let mut width = i64::from(want.width);
    let mut height = i64::from(want.height) + 120;
    for _ in 0..8 {
        if !(256..=4096).contains(&width) || !(144..=2304).contains(&height) {
            return Err(error("proof viewport outer window exceeded bounds"));
        }
        client
            .call(
                "Browser.setWindowBounds",
                json!({"windowId":id,"bounds":{"left":0,"top":0,"width":width,"height":height}}),
            )
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let metrics = client.call("Page.getLayoutMetrics", json!({})).await?;
        let css = &metrics["cssLayoutViewport"];
        let device = &metrics["layoutViewport"];
        let actual_width = css["clientWidth"]
            .as_i64()
            .ok_or_else(|| error("proof viewport CSS width missing"))?;
        let actual_height = css["clientHeight"]
            .as_i64()
            .ok_or_else(|| error("proof viewport CSS height missing"))?;
        if actual_width == i64::from(want.width) && actual_height == i64::from(want.height) {
            if device["clientWidth"] != css["clientWidth"]
                || device["clientHeight"] != css["clientHeight"]
            {
                return Err(error("proof viewport requires device scale one"));
            }
            return Ok(());
        }
        width += i64::from(want.width) - actual_width;
        height += i64::from(want.height) - actual_height;
    }
    Err(error(
        "browser did not reach the exact requested proof viewport",
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
}
