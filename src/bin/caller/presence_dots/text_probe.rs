//! Explicit owner-elected existing-dot text acceptance. One fixed message,
//! no resume/stop/desktop/call/tool/activation lane, and never an automatic retry.

use super::{
    http::{HttpReport, ProbeOutcome, ProfileClient},
    probe_store::{Journal, Store},
    profile,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{future::Future, path::Path, time::Duration};

const USAGE: &str = "Usage: intendant presence-dots existing-dot <status|observe-probe|send-probe --allow-existing-dot> [--json]";

#[derive(Debug, Serialize)]
struct Report {
    state: &'static str,
    attempt_recorded: bool,
    post_attempted_now: bool,
    delivery_unconfirmed: bool,
    receipt_recorded: bool,
    probe_message_observed: bool,
    probe_reply_observed: bool,
    identity_live_validated: bool,
    primary_unchanged: Option<bool>,
    provider_status: Option<u16>,
    read_window_may_be_incomplete: bool,
    presence_backend_enabled: bool,
    message_send_validated: bool,
    dots_voice_validated: bool,
    cloud_desktop_validated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_failure: Option<HttpReport>,
}

impl Report {
    fn new(state: &'static str) -> Self {
        Self {
            state,
            attempt_recorded: false,
            post_attempted_now: false,
            delivery_unconfirmed: false,
            receipt_recorded: false,
            probe_message_observed: false,
            probe_reply_observed: false,
            identity_live_validated: false,
            primary_unchanged: None,
            provider_status: None,
            read_window_may_be_incomplete: false,
            presence_backend_enabled: false,
            message_send_validated: false,
            dots_voice_validated: false,
            cloud_desktop_validated: false,
            http_failure: None,
        }
    }

    fn recorded(state: &'static str, journal: &Journal) -> Self {
        Self {
            attempt_recorded: true,
            delivery_unconfirmed: !journal.message_observed,
            receipt_recorded: journal.receipt.is_some(),
            probe_message_observed: journal.message_observed,
            probe_reply_observed: journal.reply_observed,
            provider_status: journal.provider_status,
            ..Self::new(state)
        }
    }

    fn failure(state: &'static str, failure: HttpReport, journal: Option<&Journal>) -> Self {
        let mut report = journal.map_or_else(|| Self::new(state), |j| Self::recorded(state, j));
        report.http_failure = Some(failure);
        report
    }
}

pub(super) fn probe_text(attempt: &str) -> String {
    format!("[Intendant Presence connection test {attempt}] Please reply with exactly 'INTENDANT_PRESENCE_OK {attempt}'. This is only a connectivity test: do not stop, reprioritize, or modify your existing work. Do not use any tools or access any files, apps, or computers for this reply.")
}

pub(super) fn request_body(attempt: &str) -> Value {
    json!({
        "content": {"text": probe_text(attempt)},
        "request_id": attempt,
        "idempotency_token": attempt
    })
}

fn command(args: &[String]) -> Result<&str, &'static str> {
    let op = args.first().map(String::as_str).ok_or(USAGE)?;
    if !matches!(op, "status" | "observe-probe" | "send-probe")
        || args[1..]
            .iter()
            .any(|s| s != "--json" && s != "--allow-existing-dot")
        || (op == "send-probe" && !args.iter().any(|s| s == "--allow-existing-dot"))
        || (op != "send-probe" && args.iter().any(|s| s == "--allow-existing-dot"))
    {
        return Err(USAGE);
    }
    Ok(op)
}

pub(super) async fn run(args: &[String]) -> Result<(), String> {
    let op = command(args)?;
    let root = intendant_core::state_paths::intendant_home().join("presence-dots");
    let report = if op == "status" {
        status(&root)?
    } else {
        tokio::time::timeout(Duration::from_secs(180), operation(&root, op)).await
            .map_err(|_| "existing-dot probe timed out; preserve intent and observe, never automatically resend")??
    };
    if args.iter().any(|s| s == "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|_| "probe_report_failed")?
        );
    } else {
        println!("Dots existing-dot probe: {}", report.state);
        println!(
            "  message observed: {}; exact reply observed: {}",
            report.probe_message_observed, report.probe_reply_observed
        );
        println!("  Presence, voice and desktop: NOT enabled or validated");
        if report.delivery_unconfirmed {
            println!("  preserve the journal; observe-probe reads only and never resends");
        }
    }
    Ok(())
}

fn status(root: &Path) -> Result<Report, String> {
    Ok(match Store::read(root)? {
        None => Report::new("existing_probe_not_prepared"),
        Some(journal) => Report::recorded("existing_probe_recorded_not_live_verified", &journal),
    })
}

fn still_current(fingerprint: &str) -> bool {
    crate::codex_cloud::subscription_cloud_auth()
        .ok()
        .and_then(|auth| auth.dots_binding_fingerprint().ok())
        .is_some_and(|current| current == fingerprint)
}

async fn operation(root: &Path, op: &str) -> Result<Report, String> {
    let store = Store::acquire(root)?;
    let journal = store.load()?;
    if op == "send-probe" {
        if let Some(journal) = &journal {
            return Ok(Report::recorded(
                "existing_probe_attempt_already_recorded",
                journal,
            ));
        }
    } else if journal.is_none() {
        return Ok(Report::new("existing_probe_not_prepared"));
    }
    let auth = crate::codex_cloud::subscription_cloud_auth()
        .map_err(|_| "subscription_login_unavailable")?;
    let fingerprint = auth
        .dots_binding_fingerprint()
        .map_err(|_| "subscription_continuity_identity_unavailable")?;
    if journal
        .as_ref()
        .is_some_and(|j| j.fingerprint != fingerprint)
    {
        return Ok(Report::new("existing_probe_subscription_changed"));
    }
    let client = match ProfileClient::account_bound(&auth).await {
        Ok(client) => client,
        Err(failure) => {
            return Ok(Report::failure(
                "existing_probe_account_discovery_refused",
                failure,
                journal.as_ref(),
            ))
        }
    };
    let verify = |id: String| {
        let auth = &auth;
        async move { super::verify_current_root(auth, &id).await }
    };
    let current = || still_current(&fingerprint);
    if let Some(journal) = journal {
        observe(&store, &client, journal, &current, &verify).await
    } else {
        send(&store, &client, &fingerprint, &current, &verify).await
    }
}

async fn send<C, V, Fut>(
    store: &Store,
    client: &ProfileClient,
    fingerprint: &str,
    current: &C,
    verify: &V,
) -> Result<Report, String>
where
    C: Fn() -> bool,
    V: Fn(String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    if let Some(journal) = store.load()? {
        return Ok(Report::recorded(
            "existing_probe_attempt_already_recorded",
            &journal,
        ));
    }
    let primary = match profile::inspect_primary(client, verify).await {
        Ok(primary) => primary,
        Err(failure) => {
            return Ok(Report::failure(
                "existing_probe_primary_refused",
                failure,
                None,
            ))
        }
    };
    if !current() {
        return Ok(Report::new("existing_probe_subscription_changed"));
    }
    let mut journal = Journal::new(fingerprint.into(), &primary)?;
    // Intent is synced before any POST; a crash or lost response cannot turn
    // into another send. This separate journal never touches test-dot.json.
    store.save(&journal)?;
    if !current() {
        return Ok(Report::recorded(
            "existing_probe_subscription_changed_before_post",
            &journal,
        ));
    }
    let mut report = match client
        .send_existing_probe(&journal.room, &journal.attempt)
        .await
    {
        ProbeOutcome::Unknown { state, status } => {
            journal.provider_status = status;
            Report::recorded(state, &journal)
        }
        ProbeOutcome::Received(value, status) => {
            journal.provider_status = Some(status);
            match receipt(&value, &journal) {
                Ok(id) => {
                    journal.receipt = Some(id);
                    Report::recorded(
                        "existing_probe_receipt_recorded_readback_required",
                        &journal,
                    )
                }
                Err(_) => Report::recorded("existing_probe_receipt_schema_unconfirmed", &journal),
            }
        }
    };
    store.save(&journal)?;
    report.post_attempted_now = true;
    report.identity_live_validated = true;
    report.primary_unchanged = selection_and_root_unchanged(client, &journal).await;
    if !current() {
        report.state = "existing_probe_subscription_changed_requires_review";
        report.identity_live_validated = false;
    } else if report.primary_unchanged != Some(true) {
        report.state = "existing_probe_primary_recheck_requires_review";
        report.identity_live_validated = false;
    }
    Ok(report)
}

async fn selection_and_root_unchanged(client: &ProfileClient, journal: &Journal) -> Option<bool> {
    let selection = profile::read_selection_guard(client).await.ok()?;
    let root = profile::read_current_root(client, &journal.dot)
        .await
        .ok()?;
    Some(selection == journal.selection_guard && root == journal.root)
}

fn matches_binding(primary: &profile::PrimarySnapshot, journal: &Journal) -> bool {
    primary.selection_guard == journal.selection_guard
        && primary.identity.dot == journal.dot
        && primary.identity.root == journal.root
        && primary.identity.room.as_deref() == Some(&journal.room)
        && primary.room_linked()
}

async fn observe<C, V, Fut>(
    store: &Store,
    client: &ProfileClient,
    mut journal: Journal,
    current: &C,
    verify: &V,
) -> Result<Report, String>
where
    C: Fn() -> bool,
    V: Fn(String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    if !current() {
        return Ok(Report::recorded(
            "existing_probe_subscription_changed",
            &journal,
        ));
    }
    let primary = match profile::inspect_primary(client, verify).await {
        Ok(primary) => primary,
        Err(failure) => {
            return Ok(Report::failure(
                "existing_probe_identity_recheck_refused",
                failure,
                Some(&journal),
            ))
        }
    };
    if !matches_binding(&primary, &journal) {
        return Ok(Report::recorded(
            "existing_probe_binding_changed_requires_review",
            &journal,
        ));
    }
    if !current() {
        return Ok(Report::recorded(
            "existing_probe_subscription_changed",
            &journal,
        ));
    }
    let page = match client
        .read_probe_messages(&journal.room, journal.receipt.as_deref())
        .await
    {
        Ok(page) => page,
        Err(failure) => {
            return Ok(Report::failure(
                "existing_probe_readback_refused",
                failure,
                Some(&journal),
            ))
        }
    };
    let evidence = match readback(&page, &journal) {
        Ok(evidence) => evidence,
        Err(_) => {
            return Ok(Report::recorded(
                "existing_probe_readback_schema_unconfirmed",
                &journal,
            ))
        }
    };
    let unchanged = selection_and_root_unchanged(client, &journal).await;
    if !current() || unchanged != Some(true) {
        let mut report = Report::recorded(
            "existing_probe_readback_identity_changed_requires_review",
            &journal,
        );
        report.primary_unchanged = unchanged;
        return Ok(report);
    }
    if let Some(id) = evidence.message {
        journal.receipt = Some(id);
        journal.message_observed = true;
    }
    // An exact assistant nonce without its independently matched user probe
    // is not a validated round trip. Ignore unrelated room contents.
    if journal.message_observed && evidence.reply {
        journal.reply_observed = true;
    }
    store.save(&journal)?;
    let mut report = Report::recorded(
        if journal.reply_observed {
            "existing_probe_round_trip_observed"
        } else if journal.message_observed {
            "existing_probe_message_observed_reply_pending"
        } else {
            "existing_probe_not_observed_outcome_still_unknown"
        },
        &journal,
    );
    report.identity_live_validated = true;
    report.primary_unchanged = Some(true);
    report.message_send_validated = journal.message_observed;
    report.read_window_may_be_incomplete = evidence.incomplete;
    Ok(report)
}

fn receipt(value: &Value, journal: &Journal) -> Result<String, ()> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| profile::valid_resource_id(id))
        .ok_or(())?;
    if value
        .get("account_user_id")
        .and_then(Value::as_str)
        .is_none_or(|id| !profile::valid_resource_id(id))
        || value.pointer("/content/text").and_then(Value::as_str)
            != Some(probe_text(&journal.attempt).as_str())
        || value
            .get("room_id")
            .is_some_and(|id| id.as_str() != Some(&journal.room))
        || value
            .get("request_id")
            .is_some_and(|id| !id.is_null() && id.as_str() != Some(&journal.attempt))
        || value.get("raw_messages").is_some()
        || value.get("deleted_at").is_some_and(|v| !v.is_null())
    {
        return Err(());
    }
    Ok(id.into())
}

struct Evidence {
    message: Option<String>,
    reply: bool,
    incomplete: bool,
}

fn readback(page: &Value, journal: &Journal) -> Result<Evidence, ()> {
    let items = page
        .get("items")
        .and_then(Value::as_array)
        .filter(|a| a.len() <= 20)
        .ok_or(())?;
    let mut message = None;
    let mut reply = false;
    let expected = format!("INTENDANT_PRESENCE_OK {}", journal.attempt);
    for item in items {
        if let Ok(id) = receipt(item, journal) {
            if message.is_some() || journal.receipt.as_ref().is_some_and(|known| known != &id) {
                return Err(());
            }
            message = Some(id);
        }
        if item.get("account_user_id").is_some()
            || item.get("deleted_at").is_some_and(|v| !v.is_null())
            || item
                .get("room_id")
                .is_some_and(|id| id.as_str() != Some(&journal.room))
        {
            continue;
        }
        if let Some(raw) = item
            .get("raw_messages")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= 64)
        {
            for raw in raw {
                if raw.pointer("/author/role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                if let Some(parts) = raw
                    .pointer("/content/parts")
                    .and_then(Value::as_array)
                    .filter(|p| p.len() <= 64)
                {
                    if parts.iter().all(Value::is_string)
                        && parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<String>()
                            .trim()
                            == expected
                    {
                        reply = true;
                    }
                }
            }
        }
    }
    Ok(Evidence {
        message,
        reply,
        // A bounded window is not a complete conversation scan. Missing
        // evidence never authorizes another POST, even without a cursor.
        incomplete: items.len() == 20
            || page.get("next_cursor").is_some_and(|v| !v.is_null())
            || page.get("prev_cursor").is_some_and(|v| !v.is_null()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    const ATTEMPT: &str = "00000000-0000-7000-8000-000000000009";
    const ROOT: &str = "00000000-0000-7000-8000-000000000001";
    const FP: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Clone, Copy)]
    enum Mode {
        Normal,
        Refused,
        LostReceipt,
        PrimaryDrift,
        MissingRoom,
    }

    struct Fixture {
        client: ProfileClient,
        posts: Arc<AtomicUsize>,
        reads: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn fixture(root: std::path::PathBuf, mode: Mode) -> Fixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/tbo", listener.local_addr().unwrap());
        let posts = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let posted = posts.clone();
        let counted = reads.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                let words: Vec<_> = line.split_whitespace().collect();
                assert_eq!(words.len(), 3);
                let method = words[0];
                let path = words[1];
                let mut length = 0;
                let mut header_bytes = 0;
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    header_bytes += line.len();
                    assert!(header_bytes <= 8192);
                    if line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    for prohibited in [
                        "cookie:",
                        "app-attest",
                        "x-openai-sentinel",
                        "x-ios",
                        "x-conduit",
                    ] {
                        assert!(!lower.starts_with(prohibited));
                    }
                    if let Some(n) = lower.strip_prefix("content-length:") {
                        length = n.trim().parse::<usize>().unwrap();
                    }
                }
                assert!(length <= 8192);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let room = if matches!(mode, Mode::MissingRoom) {
                    Value::Null
                } else {
                    json!("fixture-room")
                };
                let (status, value) = if method == "POST" {
                    assert_eq!(path, "/messaging/rooms/fixture-room/messages");
                    assert_eq!(posted.fetch_add(1, Ordering::SeqCst), 0);
                    let j = Store::read(&root).unwrap().unwrap();
                    // The provider cannot see a send before synced intent.
                    assert!(j.receipt.is_none());
                    assert_eq!(
                        serde_json::from_slice::<Value>(&body).unwrap(),
                        request_body(&j.attempt)
                    );
                    if matches!(mode, Mode::LostReceipt) {
                        drop(socket);
                        continue;
                    }
                    (
                        if matches!(mode, Mode::Refused) {
                            403
                        } else {
                            201
                        },
                        json!({"id":"probe-message","account_user_id":"fixture-user",
                            "room_id":"fixture-room","request_id":j.attempt,
                            "content":{"text":probe_text(&j.attempt)}}),
                    )
                } else {
                    assert_eq!(method, "GET");
                    if path == "/tbo/primary" {
                        (
                            200,
                            json!({"selection":{"thread_id":ROOT,
                            "generation":if matches!(mode, Mode::PrimaryDrift) && posted.load(Ordering::SeqCst)>0 {"g2"} else {"g1"},
                            "selected_at":"fixture-time","available":true,
                            "messaging_room_id":room}}),
                        )
                    } else if path == "/tbo/fixture:dot/root-thread" {
                        (200, json!({"root_thread_id":ROOT}))
                    } else if path == format!("/tbo/by-thread/{ROOT}") {
                        (
                            200,
                            json!({"id":"fixture:dot","aeon_kind":"orbit","status":"active",
                            "active_root_thread_id":ROOT,"messaging_room_id":room}),
                        )
                    } else if path.starts_with("/messaging/rooms/fixture-room/messages?limit=20") {
                        counted.fetch_add(1, Ordering::SeqCst);
                        let j = Store::read(&root).unwrap().unwrap();
                        if j.receipt.is_some() {
                            assert!(path.contains("&around=probe-message"));
                        }
                        (
                            200,
                            json!({"items":[
                            {"id":"probe-message","account_user_id":"fixture-user","room_id":j.room,
                                "request_id":j.attempt,"content":{"text":probe_text(&j.attempt)}},
                            {"id":"reply-message","room_id":j.room,"raw_messages":[
                                {"author":{"role":"assistant"},"content":{"parts":[format!("INTENDANT_PRESENCE_OK {}",j.attempt)]}}]}
                        ],"prev_cursor":null,"next_cursor":null}),
                        )
                    } else {
                        panic!("unexpected fixture operation {method} {path}");
                    }
                };
                let body = value.to_string();
                let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        Fixture {
            client: ProfileClient::fixture(base, reqwest::header::HeaderMap::new()),
            posts,
            reads,
            task,
        }
    }

    async fn verify(id: String) -> Result<(), String> {
        assert_eq!(id, ROOT);
        Ok(())
    }

    #[tokio::test]
    async fn one_synced_send_then_read_only_round_trip_and_restart_no_resend() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = Store::acquire(&root).unwrap();
        let legacy = root.join("test-dot.json");
        std::fs::write(&legacy, b"protected uncertain creation").unwrap();
        let server = fixture(root.clone(), Mode::Normal).await;
        let sent = send(&store, &server.client, FP, &|| true, &verify)
            .await
            .unwrap();
        assert_eq!(
            sent.state,
            "existing_probe_receipt_recorded_readback_required"
        );
        assert!(sent.post_attempted_now);
        assert_eq!(sent.primary_unchanged, Some(true));
        assert!(!sent.message_send_validated);
        let seen = observe(
            &store,
            &server.client,
            store.load().unwrap().unwrap(),
            &|| true,
            &verify,
        )
        .await
        .unwrap();
        assert_eq!(seen.state, "existing_probe_round_trip_observed");
        assert!(seen.message_send_validated && seen.probe_reply_observed);
        assert!(
            !seen.presence_backend_enabled
                && !seen.dots_voice_validated
                && !seen.cloud_desktop_validated
        );
        drop(store);
        let resumed = Store::acquire(&root).unwrap();
        let again = send(&resumed, &server.client, FP, &|| true, &verify)
            .await
            .unwrap();
        assert_eq!(again.state, "existing_probe_attempt_already_recorded");
        assert!(!again.post_attempted_now);
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
        assert_eq!(server.reads.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(legacy).unwrap(),
            b"protected uncertain creation"
        );
    }

    #[tokio::test]
    async fn rejected_or_lost_receipt_preserves_intent_and_never_retries_post() {
        for mode in [Mode::Refused, Mode::LostReceipt] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("state");
            let store = Store::acquire(&root).unwrap();
            let server = fixture(root.clone(), mode).await;
            let sent = send(&store, &server.client, FP, &|| true, &verify)
                .await
                .unwrap();
            assert!(sent.delivery_unconfirmed && sent.post_attempted_now);
            assert!(store.load().unwrap().unwrap().receipt.is_none());
            let blocked = send(&store, &server.client, FP, &|| true, &verify)
                .await
                .unwrap();
            assert!(!blocked.post_attempted_now);
            let read = observe(
                &store,
                &server.client,
                store.load().unwrap().unwrap(),
                &|| true,
                &verify,
            )
            .await
            .unwrap();
            assert!(read.probe_reply_observed);
            assert_eq!(server.posts.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn missing_room_or_account_change_before_post_sends_nothing() {
        for mode in [Mode::Normal, Mode::MissingRoom] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("state");
            let store = Store::acquire(&root).unwrap();
            let server = fixture(root.clone(), mode).await;
            let calls = AtomicUsize::new(0);
            let current = || calls.fetch_add(1, Ordering::SeqCst) == 0;
            let result = send(&store, &server.client, FP, &current, &verify).await;
            if matches!(mode, Mode::Normal) {
                assert_eq!(
                    result.unwrap().state,
                    "existing_probe_subscription_changed_before_post"
                );
                assert!(store.load().unwrap().is_some());
            } else {
                assert!(result.is_err());
                assert!(store.load().unwrap().is_none());
            }
            assert_eq!(server.posts.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn changed_primary_never_restores_selection_or_reads_the_old_room() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = Store::acquire(&root).unwrap();
        let server = fixture(root.clone(), Mode::PrimaryDrift).await;
        let sent = send(&store, &server.client, FP, &|| true, &verify)
            .await
            .unwrap();
        assert_eq!(sent.state, "existing_probe_primary_recheck_requires_review");
        assert_eq!(sent.primary_unchanged, Some(false));
        let seen = observe(
            &store,
            &server.client,
            store.load().unwrap().unwrap(),
            &|| true,
            &verify,
        )
        .await
        .unwrap();
        assert_eq!(seen.state, "existing_probe_binding_changed_requires_review");
        assert_eq!(server.reads.load(Ordering::SeqCst), 0);
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    }

    fn journal() -> Journal {
        serde_json::from_value(json!({
            "version": 1, "attempt": ATTEMPT, "fingerprint": "a".repeat(64),
            "selection_guard": "b".repeat(64), "dot": "private:dot",
            "root": "00000000-0000-7000-8000-000000000001", "room": "fixture-room",
            "receipt": null, "provider_status": null,
            "message_observed": false, "reply_observed": false
        }))
        .unwrap()
    }

    fn user() -> Value {
        json!({"id":"probe-message","account_user_id":"fixture-user",
            "room_id":"fixture-room","request_id":ATTEMPT,
            "content":{"text":probe_text(ATTEMPT),"attachments":[]}})
    }

    fn reply(role: &str) -> Value {
        json!({"id":"reply-message","room_id":"fixture-room","raw_messages":[
            {"author":{"role":role},"content":{"content_type":"text","parts":[format!("INTENDANT_PRESENCE_OK {ATTEMPT}")]}}
        ]})
    }

    #[test]
    fn send_requires_explicit_election_and_has_no_arbitrary_input_flags() {
        for args in [
            vec![],
            vec!["send-probe"],
            vec!["send-probe", "--text=anything"],
            vec!["send-probe", "--allow-existing-dot", "--url=anything"],
            vec!["observe-probe", "--allow-existing-dot"],
            vec!["reset"],
            vec!["retry"],
        ] {
            assert!(command(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
        }
        assert_eq!(
            command(&[
                "send-probe".into(),
                "--allow-existing-dot".into(),
                "--json".into()
            ])
            .unwrap(),
            "send-probe"
        );
        let body = request_body(ATTEMPT);
        assert_eq!(body.as_object().unwrap().len(), 3);
        assert_eq!(body["request_id"], body["idempotency_token"]);
        assert!(!body.to_string().contains("attest"));
        assert!(body["content"]["text"]
            .as_str()
            .unwrap()
            .contains("do not stop"));
    }

    #[test]
    fn readback_requires_exact_user_probe_and_only_matches_assistant_nonce() {
        let j = journal();
        let page =
            json!({"items":[user(),reply("assistant")],"prev_cursor":null,"next_cursor":null});
        let evidence = readback(&page, &j).unwrap();
        assert_eq!(evidence.message.as_deref(), Some("probe-message"));
        assert!(evidence.reply);
        assert!(
            !readback(&json!({"items":[user(),reply("user")]}), &j)
                .unwrap()
                .reply
        );
        let mut unrelated = user();
        unrelated["content"]["text"] = json!("private unrelated conversation");
        assert!(readback(&json!({"items":[unrelated]}), &j)
            .unwrap()
            .message
            .is_none());
    }

    #[test]
    fn wrong_room_deleted_mismatched_request_and_duplicate_receipts_refuse() {
        let j = journal();
        for (field, value) in [
            ("room_id", json!("other-room")),
            ("request_id", json!("other-request")),
            ("deleted_at", json!("timestamp")),
        ] {
            let mut item = user();
            item[field] = value;
            assert!(receipt(&item, &j).is_err());
        }
        assert!(readback(&json!({"items":[user(),user()]}), &j).is_err());
        let mut known = journal();
        known.receipt = Some("different-message".into());
        assert!(readback(&json!({"items":[user()]}), &known).is_err());
        assert!(readback(&json!({"items":vec![user();21]}), &j).is_err());
    }

    #[test]
    fn historical_state_is_never_live_validation_or_activation() {
        let mut j = journal();
        j.receipt = Some("private-message-id".into());
        j.message_observed = true;
        j.reply_observed = true;
        let report = Report::recorded("historical", &j);
        assert!(!report.identity_live_validated);
        assert!(!report.message_send_validated);
        assert!(!report.presence_backend_enabled);
        assert!(!report.dots_voice_validated);
        assert!(!report.cloud_desktop_validated);
        let encoded = serde_json::to_string(&report).unwrap();
        for private in [
            ATTEMPT,
            "private-message-id",
            "private:dot",
            "fixture-room",
            "private unrelated conversation",
        ] {
            assert!(!encoded.contains(private));
            assert!(!format!("{report:?}").contains(private));
        }
    }
}
