//! Navigation of the original managed page, not a new tab, desktop shortcut,
//! arbitrary endpoint, or script evaluator. Shared exact-process/page checks
//! are the same ones used by keyboard delivery.
use super::*;
#[cfg(any(target_os = "macos", test))]
use serde_json::{json, Value};

pub(super) fn validate_url(raw: &str) -> Result<(), String> {
    if raw.is_empty() || raw.len() > 4096 || raw.trim() != raw {
        return Err("navigation URL must contain 1..4096 bytes without outer whitespace".into());
    }
    launch_policy::navigation(Some(raw)).map_err(str::to_string)?;
    if raw == "about:blank" {
        return Ok(());
    }
    let url = url::Url::parse(raw).map_err(|_| "invalid navigation URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(
            "navigation URL must be http, https or about:blank, without credentials".into(),
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub(super) struct Receipt {
    pub ok: bool,
    pub commands_attempted: u8,
    pub commands_acknowledged: u8,
    pub navigation_committed: bool,
    pub effects_verified: bool,
    pub effects_unconfirmed: bool,
    pub error: Option<String>,
    pub mechanism: &'static str,
}
impl Receipt {
    fn new() -> Self {
        Self {
            ok: false,
            commands_attempted: 0,
            commands_acknowledged: 0,
            navigation_committed: false,
            effects_verified: false,
            effects_unconfirmed: false,
            error: None,
            mechanism: "managed_page_navigation",
        }
    }
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn top_frame(tree: &Value) -> Result<&Value, String> {
    let frame = &tree["frameTree"]["frame"];
    if frame.get("parentId").is_some()
        || frame["id"]
            .as_str()
            .is_none_or(|id| id.is_empty() || id.len() > 512)
    {
        return Err("original page top frame unavailable".into());
    }
    Ok(frame)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn acknowledge(reply: &Value, frame_id: &str) -> Result<Option<String>, String> {
    if reply["frameId"].as_str() != Some(frame_id)
        || reply.get("errorText").is_some()
        || reply
            .get("isDownload")
            .is_some_and(|v| v.as_bool() != Some(false))
    {
        return Err("navigation failed, downloaded, or changed its frame".into());
    }
    match reply.get("loaderId") {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .map(|id| Some(id.to_string()))
            .ok_or("invalid navigation loader receipt".into()),
    }
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn committed(
    tree: &Value,
    frame_id: &str,
    loader: Option<&str>,
    requested: &str,
) -> Result<bool, String> {
    let frame = top_frame(tree)?;
    if frame["id"].as_str() != Some(frame_id) {
        return Err("navigation replaced the original top frame".into());
    }
    let raw = frame["url"]
        .as_str()
        .ok_or("navigation document URL unavailable")?;
    validate_url(raw)?;
    if let Some(loader) = loader {
        // Redirects are fine, but only the loader returned by our ONE navigate
        // can confirm a cross-document commit. Never accept an older document.
        return Ok(frame["loaderId"].as_str() == Some(loader));
    }
    let fragment = match frame.get("urlFragment") {
        None => "",
        Some(v) => v
            .as_str()
            .filter(|s| s.is_empty() || s.starts_with('#'))
            .ok_or("invalid document fragment")?,
    };
    let actual = if fragment.is_empty() {
        raw.to_string()
    } else {
        format!("{raw}{fragment}")
    };
    // Same-document navigations omit loaderId. Match the normalized URL,
    // including its fragment, rather than accepting any pre-existing frame.
    Ok(url::Url::parse(&actual).ok() == url::Url::parse(requested).ok())
}

#[cfg(any(target_os = "macos", test))]
async fn navigate_once(
    client: &mut managed_keyboard::Client,
    url: &str,
    frame: &str,
    receipt: &mut Receipt,
) -> Result<Option<String>, String> {
    receipt.commands_attempted = 1;
    receipt.effects_unconfirmed = true;
    let reply = client
        .call(
            "Page.navigate",
            json!({"url":url,"frameId":frame,"transitionType":"typed"}),
        )
        .await?;
    receipt.commands_acknowledged = 1;
    acknowledge(&reply, frame)
}

pub(super) async fn execute(
    id: &str,
    url: &str,
    bus: &EventBus,
    authority: &crate::macos_monitor::Authority,
) -> Receipt {
    let mut receipt = Receipt::new();
    let result: Result<(), String> = async {
        validate_url(url)?;
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (id, bus, authority);
            Err("managed navigation requires macOS".into())
        }
        #[cfg(target_os = "macos")]
        {
            // The task worker owns its admission guard through this entire
            // future, including caller cancellation. No navigation is replayed.
            let _lane = bus
                .macos_monitors
                .workspace_lane
                .clone()
                .try_lock_owned()
                .map_err(|_| "browser busy; no navigation queued")?;
            let workspace = managed_keyboard::owned_workspace(id).await?;
            managed_keyboard::validate_native(&workspace, bus, authority).await?;
            let mut page = managed_keyboard::connect_owned_page(&workspace).await?;
            let tree = page.call("Page.getFrameTree", json!({})).await?;
            let frame = top_frame(&tree)?["id"]
                .as_str()
                .expect("validated")
                .to_string();
            let before = managed_keyboard::foreground().await?;
            managed_keyboard::validate_native(&workspace, bus, authority).await?;
            managed_keyboard::validate_original_page(&mut page, &workspace).await?;
            authority.check().await?;
            let loader = navigate_once(&mut page, url, &frame, &mut receipt).await?;
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let tree = page.call("Page.getFrameTree", json!({})).await?;
                    if committed(&tree, &frame, loader.as_deref(), url)? {
                        break Ok::<_, String>(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .map_err(|_| "navigation commit not confirmed; do not replay")??;
            receipt.navigation_committed = true;
            managed_keyboard::validate_original_page(&mut page, &workspace).await?;
            managed_keyboard::validate_native(&workspace, bus, authority).await?;
            if managed_keyboard::foreground().await? != before {
                return Err("foreground changed during navigation; attribution unknown".into());
            }
            // This confirms a protocol/document commit, not useful site effects
            // or network quiescence. The agent must observe the resulting page.
            Ok(())
        }
    }
    .await;
    receipt.ok = result.is_ok();
    if let Err(error) = result {
        eprintln!("[task-browser] navigation: {error}");
        receipt.error =
            Some("navigation refused or unconfirmed; observe before further input".into());
    }
    receipt
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tree(id: &str, loader: &str, url: &str) -> Value {
        json!({"frameTree":{"frame":{"id":id,"loaderId":loader,"url":url}}})
    }
    #[test]
    fn navigation_urls_never_select_scripts_files_handlers_or_credentials() {
        for raw in [
            "javascript:alert(1)",
            "file:///tmp/private",
            "data:text/html,test",
            "app://start",
            "https://user:pass@example.test",
            "https://user@example.test",
            "about:config",
            " https://example.test",
            "https://example.test\n",
            "",
        ] {
            assert!(validate_url(raw).is_err(), "{raw}");
        }
        for raw in [
            "https://example.test/path?search=one#section",
            "http://127.0.0.1:1234/test",
            "about:blank",
        ] {
            assert!(validate_url(raw).is_ok(), "{raw}");
        }
    }
    #[test]
    fn navigation_ack_requires_exact_frame_and_never_counts_download_as_success() {
        assert_eq!(
            acknowledge(&json!({"frameId":"f","loaderId":"new"}), "f").unwrap(),
            Some("new".into())
        );
        assert_eq!(acknowledge(&json!({"frameId":"f"}), "f").unwrap(), None);
        for reply in [
            json!({}),
            json!({"frameId":"other"}),
            json!({"frameId":"f","errorText":"blocked"}),
            json!({"frameId":"f","isDownload":true}),
            json!({"frameId":"f","isDownload":"false"}),
            json!({"frameId":"f","loaderId":""}),
        ] {
            assert!(acknowledge(&reply, "f").is_err());
        }
    }
    #[test]
    fn commit_requires_new_loader_not_merely_the_same_tab() {
        assert!(!committed(
            &tree("f", "old", "https://example.test"),
            "f",
            Some("new"),
            "https://example.test"
        )
        .unwrap());
        assert!(committed(
            &tree("f", "new", "https://redirect.test/destination"),
            "f",
            Some("new"),
            "https://example.test"
        )
        .unwrap());
        assert!(committed(
            &tree("foreign", "new", "https://example.test"),
            "f",
            Some("new"),
            "https://example.test"
        )
        .is_err());
        assert!(committed(
            &tree("f", "new", "file:///private"),
            "f",
            Some("new"),
            "https://example.test"
        )
        .is_err());
    }
    #[test]
    fn same_document_commit_requires_matching_fragment() {
        let mut value = tree("f", "old", "https://example.test/");
        assert!(!committed(&value, "f", None, "https://example.test/#new").unwrap());
        value["frameTree"]["frame"]["urlFragment"] = json!("#new");
        assert!(committed(&value, "f", None, "https://example.test/#new").unwrap());
    }
    #[tokio::test]
    async fn navigation_sends_once_to_exact_top_frame() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let call: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(call["method"], "Page.navigate");
            assert_eq!(
                call["params"],
                json!({"url":"https://example.test/","frameId":"top","transitionType":"typed"})
            );
            ws.send(Message::Text(
                json!({"id":call["id"],"result":{"frameId":"top","loaderId":"new"}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            // After acknowledgment the caller closes: no retry, no key or
            // activation command, and no implicit second navigation.
            match ws.next().await {
                None | Some(Err(_)) => (),
                Some(Ok(Message::Close(_))) => (),
                other => panic!("extra command: {other:?}"),
            }
        });
        let mut client = managed_keyboard::Client::connect(
            port,
            &format!("ws://127.0.0.1:{port}/devtools/page/page"),
            "page",
        )
        .await
        .unwrap();
        let mut receipt = Receipt::new();
        assert_eq!(
            navigate_once(&mut client, "https://example.test/", "top", &mut receipt)
                .await
                .unwrap(),
            Some("new".into())
        );
        assert_eq!(receipt.commands_attempted, 1);
        assert_eq!(receipt.commands_acknowledged, 1);
        assert!(!receipt.effects_verified);
        assert!(!receipt.navigation_committed);
        drop(client);
        peer.await.unwrap();
    }
    #[tokio::test]
    async fn disconnected_navigation_is_uncertain_without_a_retry() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let call: Value =
                serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(call["method"], "Page.navigate");
            ws.send(Message::Close(None)).await.unwrap();
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => (),
                other => panic!("navigation was retried: {other:?}"),
            }
        });
        let mut client = managed_keyboard::Client::connect(
            port,
            &format!("ws://127.0.0.1:{port}/devtools/page/page"),
            "page",
        )
        .await
        .unwrap();
        let mut receipt = Receipt::new();
        assert!(
            navigate_once(&mut client, "https://example.test/", "top", &mut receipt)
                .await
                .is_err()
        );
        assert_eq!(receipt.commands_attempted, 1);
        assert_eq!(receipt.commands_acknowledged, 0);
        assert!(receipt.effects_unconfirmed);
        assert!(!receipt.navigation_committed);
        drop(client);
        peer.await.unwrap();
    }
}
