//! Explicit additional test-dot preparation. Still not an enabled Presence
//! provider, generic RPC, message, voice, desktop, primary-selection or IAM lane.

use super::{
    http::{CreationOutcome, HttpReport, ProfileClient, ProfileRoute},
    profile,
    test_store::{DotRecord, Journal, Phase, Store},
};
use serde::Serialize;
use serde_json::Value;
use std::{future::Future, path::Path, time::Duration};

#[derive(Debug, Serialize)]
struct Report {
    state: &'static str,
    post_attempt_recorded: bool,
    creation_outcome_unconfirmed: bool,
    binding_recorded: bool,
    binding_live_validated: bool,
    primary_unchanged: Option<bool>,
    messaging_room_linked: Option<bool>,
    provider_status: Option<u16>,
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
            post_attempt_recorded: false,
            creation_outcome_unconfirmed: false,
            binding_recorded: false,
            binding_live_validated: false,
            primary_unchanged: None,
            messaging_room_linked: None,
            provider_status: None,
            presence_backend_enabled: false,
            message_send_validated: false,
            dots_voice_validated: false,
            cloud_desktop_validated: false,
            http_failure: None,
        }
    }

    fn recorded(state: &'static str, journal: &Journal) -> Self {
        Self {
            post_attempt_recorded: true,
            creation_outcome_unconfirmed: journal.received.is_none(),
            binding_recorded: journal.phase == Phase::Verified,
            provider_status: journal.provider_status,
            ..Self::new(state)
        }
    }

    fn refused(state: &'static str, failure: HttpReport, journal: Option<&Journal>) -> Self {
        let mut report = journal.map_or_else(|| Self::new(state), |j| Self::recorded(state, j));
        report.http_failure = Some(failure);
        report
    }
}

pub(super) async fn run(args: &[String]) -> Result<(), String> {
    if args.is_empty()
        || !matches!(args[0].as_str(), "create" | "status" | "reconcile")
        || args[1..].iter().any(|s| s != "--json")
    {
        return Err(
            "Usage: intendant presence-dots test-dot <create|status|reconcile> [--json]".into(),
        );
    }
    let root = intendant_core::state_paths::intendant_home().join("presence-dots");
    let report = if args[0] == "status" {
        status(&root)?
    } else {
        tokio::time::timeout(Duration::from_secs(900), operation(&root, &args[0])).await
            .map_err(|_| "test-dot operation timed out; reconcile existing intent, never retry creation blindly".to_string())??
    };
    if args.iter().any(|s| s == "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|_| "could not encode test-dot report")?
        );
    } else {
        println!("Dots test setup: {}", report.state);
        println!(
            "  binding recorded: {}; live verified now: {}",
            report.binding_recorded, report.binding_live_validated
        );
        println!("  Presence/messages/voice/desktop: NOT enabled or validated");
        if report.creation_outcome_unconfirmed {
            println!("  creation outcome unconfirmed: use test-dot reconcile, not another creation attempt");
        }
    }
    Ok(())
}

fn status(root: &Path) -> Result<Report, String> {
    Ok(match Store::read(root)? {
        None => Report::new("test_dot_not_prepared"),
        Some(journal) => Report::recorded("test_dot_journal_present_not_live_verified", &journal),
    })
}

fn still_current(fingerprint: &str) -> bool {
    crate::codex_cloud::subscription_cloud_auth()
        .ok()
        .and_then(|auth| auth.dots_binding_fingerprint().ok())
        .is_some_and(|current| current == fingerprint)
}

async fn operation(root: &Path, command: &str) -> Result<Report, String> {
    let store = Store::acquire(root)?;
    let journal = store.load()?;
    // A durable record, corrupt file, or incomplete attempt is never silently
    // reset. Even a verified record must be freshly checked before live use.
    if command == "create" {
        if let Some(journal) = &journal {
            return Ok(Report::recorded(
                "existing_test_dot_intent_requires_reconciliation",
                journal,
            ));
        }
    } else if journal.is_none() {
        return Ok(Report::new("test_dot_not_prepared"));
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
        return Ok(Report::new("test_dot_subscription_changed"));
    }
    let client = match ProfileClient::account_bound(&auth).await {
        Ok(client) => client,
        Err(failure) => {
            return Ok(Report::refused(
                "test_dot_account_discovery_refused",
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
        reconcile(&store, &client, journal, &current, &verify).await
    } else {
        create(&store, &client, &fingerprint, &current, &verify).await
    }
}

async fn create<C, V, Fut>(
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
    // Recheck under the cross-process lock; callers cannot supply a second
    // attempt while any previous intent remains recorded.
    if let Some(journal) = store.load()? {
        return Ok(Report::recorded(
            "existing_test_dot_intent_requires_reconciliation",
            &journal,
        ));
    }
    let primary = match tokio::time::timeout(
        Duration::from_secs(90),
        profile::inspect_primary(client, verify),
    )
    .await
    {
        Ok(Ok(primary)) => primary,
        Ok(Err(failure)) => {
            return Ok(Report::refused(
                "test_dot_primary_preparation_refused",
                failure,
                None,
            ))
        }
        Err(_) => return Ok(Report::new("test_dot_primary_preparation_timed_out")),
    };
    if !current() {
        return Ok(Report::new("test_dot_subscription_changed"));
    }
    let mut journal = Journal::new(fingerprint.into(), &primary);
    // This synced write must succeed before ANY POST. Process death between
    // this point and the response leaves one uncertain intent, never a retry.
    store.save(&journal)?;
    match client.create_additional(&journal.label).await {
        CreationOutcome::Unknown { state, status } => {
            journal.provider_status = status;
            let unchanged = profile::read_selection_guard(client)
                .await
                .ok()
                .map(|guard| guard == journal.primary_guard);
            if unchanged == Some(false) {
                journal.phase = Phase::NeedsReview;
            }
            store.save(&journal)?;
            let mut report = Report::recorded(state, &journal);
            report.primary_unchanged = unchanged;
            Ok(report)
        }
        CreationOutcome::Received(value, status) => {
            journal.provider_status = Some(status);
            let Some(record) = parse_created(&value) else {
                store.save(&journal)?;
                return Ok(Report::recorded(
                    "creation_identity_outcome_unconfirmed",
                    &journal,
                ));
            };
            journal.received = Some(record);
            journal.phase = Phase::Received;
            store.save(&journal)?;
            verify_received(store, client, journal, current, verify).await
        }
    }
}

fn parse_created(value: &Value) -> Option<DotRecord> {
    let room = match value.get("messaging_room_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(room)) if profile::valid_segment(room) => Some(room.clone()),
        _ => return None,
    };
    let record = DotRecord {
        dot: value.get("id")?.as_str()?.into(),
        root: value.get("root_thread_id")?.as_str()?.into(),
        room,
    };
    record.valid().then_some(record)
}

async fn reconcile<C, V, Fut>(
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
    if journal.phase == Phase::NeedsReview {
        return Ok(Report::recorded("test_dot_requires_owner_review", &journal));
    }
    if journal.received.is_none() {
        let id = match find_attempt(client, &journal.label).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Ok(Report::recorded(
                    "creation_not_observed_outcome_still_unknown",
                    &journal,
                ))
            }
            Err(failure) => {
                return Ok(Report::refused(
                    "test_dot_reconciliation_refused",
                    failure,
                    Some(&journal),
                ))
            }
        };
        if id == journal.primary_dot {
            journal.phase = Phase::NeedsReview;
            store.save(&journal)?;
            return Ok(Report::recorded(
                "test_dot_primary_identity_refused",
                &journal,
            ));
        }
        let root = match profile::read_current_root(client, &id).await {
            Ok(root) => root,
            Err(failure) => {
                return Ok(Report::refused(
                    "test_dot_reconciliation_refused",
                    failure,
                    Some(&journal),
                ))
            }
        };
        journal.received = Some(DotRecord {
            dot: id,
            root,
            room: None,
        });
        journal.phase = Phase::Received;
        store.save(&journal)?;
    }
    verify_received(store, client, journal, current, verify).await
}

async fn verify_received<C, V, Fut>(
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
    let record = journal
        .received
        .as_ref()
        .ok_or("test_dot_journal_refused")?;
    if !journal.separate(record) {
        journal.phase = Phase::NeedsReview;
        store.save(&journal)?;
        return Ok(Report::recorded(
            "test_dot_primary_identity_refused",
            &journal,
        ));
    }
    if !current() {
        return Ok(Report::recorded("test_dot_subscription_changed", &journal));
    }
    let identity =
        match profile::inspect_dedicated(client, &record.dot, &journal.label, verify).await {
            Ok(identity) => identity,
            Err(failure) => {
                return Ok(Report::refused(
                    "test_dot_identity_verification_refused",
                    failure,
                    Some(&journal),
                ))
            }
        };
    let checked = DotRecord {
        dot: identity.dot,
        root: identity.root,
        room: identity.room,
    };
    if !journal.separate(&checked)
        || record
            .room
            .as_ref()
            .is_some_and(|room| checked.room.as_ref() != Some(room))
    {
        journal.phase = Phase::NeedsReview;
        store.save(&journal)?;
        return Ok(Report::recorded(
            "test_dot_identity_cross_link_refused",
            &journal,
        ));
    }
    let unchanged = match profile::read_selection_guard(client).await {
        Ok(guard) => guard == journal.primary_guard,
        Err(failure) => {
            return Ok(Report::refused(
                "test_dot_primary_recheck_refused",
                failure,
                Some(&journal),
            ))
        }
    };
    if !unchanged {
        journal.phase = Phase::NeedsReview;
        store.save(&journal)?;
        let mut report = Report::recorded("test_dot_primary_changed_requires_review", &journal);
        report.primary_unchanged = Some(false);
        return Ok(report);
    }
    if !current() {
        return Ok(Report::recorded("test_dot_subscription_changed", &journal));
    }
    let room_linked = checked.room.is_some();
    journal.received = Some(checked);
    journal.phase = Phase::Verified;
    store.save(&journal)?;
    let mut report = Report::recorded("separate_test_dot_identity_verified", &journal);
    report.binding_live_validated = true;
    report.primary_unchanged = Some(true);
    report.messaging_room_linked = Some(room_linked);
    Ok(report)
}

/// Read at most 100 profiles, require complete bounded pagination, and accept
/// only ONE exact, journal-generated label. Zero/ambiguous/incomplete results
/// remain uncertain; no result permits another POST or selection mutation.
async fn find_attempt(client: &ProfileClient, label: &str) -> Result<Option<String>, HttpReport> {
    let refused = || HttpReport::new("test_dot_reconcile_catalog_refused", true, Some(200));
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    let mut matched: Option<String> = None;
    for _ in 0..4 {
        let value = client.read(ProfileRoute::List(cursor.as_deref())).await?;
        let items = value
            .get("items")
            .and_then(Value::as_array)
            .filter(|items| items.len() <= 25)
            .ok_or_else(refused)?;
        for item in items {
            let name = item
                .get("display_name")
                .and_then(Value::as_str)
                .ok_or_else(refused)?;
            if name == label {
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| profile::valid_resource_id(id))
                    .ok_or_else(refused)?;
                if matched.as_deref().is_some_and(|matched| matched != id) {
                    return Err(refused());
                }
                matched = Some(id.into());
            }
        }
        cursor = match value.get("cursor") {
            Some(Value::Null) => return Ok(matched),
            Some(Value::String(cursor))
                if !cursor.is_empty()
                    && cursor.len() <= 512
                    && !cursor.chars().any(char::is_control) =>
            {
                Some(cursor.clone())
            }
            _ => return Err(refused()),
        };
        if !seen.insert(cursor.clone()) {
            return Err(refused());
        }
    }
    Err(refused())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        sync::Mutex,
    };

    const PRIMARY: &str = "fixture:primary";
    const TEST_DOT: &str = "fixture:test";
    const PRIMARY_ROOT: &str = "00000000-0000-7000-8000-000000000001";
    const TEST_ROOT: &str = "00000000-0000-7000-8000-000000000002";
    const OTHER_ROOT: &str = "00000000-0000-7000-8000-000000000003";
    const FP: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[derive(Clone, Copy)]
    enum Mode {
        Normal,
        HistoricalReceipt,
        Rejected(u16),
        Unknown,
        UnknownMissing,
        UnknownAmbiguous,
        RootDrift,
        PrimaryDrift,
        WrongRoom,
    }

    struct Fixture {
        client: ProfileClient,
        posts: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
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
        let requests = Arc::new(AtomicUsize::new(0));
        let posted = posts.clone();
        let counted = requests.clone();
        let task = tokio::spawn(async move {
            let mut label = String::new();
            let mut test_root_reads = 0;
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut first = String::new();
                socket.read_line(&mut first).await.unwrap();
                let words: Vec<_> = first.split_whitespace().collect();
                assert_eq!(words.len(), 3);
                let method = words[0];
                let path = words[1];
                let mut content_length = 0;
                let mut header_bytes = 0;
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    header_bytes += line.len();
                    assert!(header_bytes <= 8192);
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.to_ascii_lowercase().starts_with("cookie:"));
                    assert!(!line.to_ascii_lowercase().starts_with("app-attest"));
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse::<usize>().unwrap();
                    }
                }
                assert!(content_length <= 8192);
                let mut body = vec![0; content_length];
                socket.read_exact(&mut body).await.unwrap();
                counted.fetch_add(1, Ordering::SeqCst);
                let (status, value) = if method == "POST" {
                    assert_eq!(path, "/tbo");
                    posted.fetch_add(1, Ordering::SeqCst);
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(body.as_object().unwrap().len(), 4);
                    assert_eq!(body["create_additional"], true);
                    assert_eq!(body["create_thread"], true);
                    assert_eq!(body["should_initialize"], true);
                    // Intent must be durable and readable BEFORE the server
                    // sees the request, not after it returns a response.
                    let journal = Store::read(&root).unwrap().unwrap();
                    assert!(journal.phase == Phase::Attempted);
                    label = body["display_name"].as_str().unwrap().into();
                    assert_eq!(label, journal.label);
                    if matches!(
                        mode,
                        Mode::Unknown | Mode::UnknownMissing | Mode::UnknownAmbiguous
                    ) {
                        // Provider got the whole body; response lost. Client
                        // must NOT automatically submit a replacement POST.
                        drop(socket);
                        continue;
                    }
                    (
                        if let Mode::Rejected(status) = mode {
                            status
                        } else {
                            201
                        },
                        json!({"id":TEST_DOT,"root_thread_id":if matches!(mode, Mode::HistoricalReceipt) {OTHER_ROOT} else {TEST_ROOT},
                        "messaging_room_id": if matches!(mode, Mode::WrongRoom) {"wrong-room"} else {"test-room"}}),
                    )
                } else {
                    assert_eq!(method, "GET");
                    if path == "/tbo/primary" {
                        (
                            200,
                            json!({"selection":{"thread_id":PRIMARY_ROOT,"generation":if matches!(mode, Mode::PrimaryDrift) && posted.load(Ordering::SeqCst)>0 {"g2"}else{"g1"},
                            "selected_at":"fixture-time","available":true,"aeon_id":"internal:private/opaque","messaging_room_id":"primary-room"}}),
                        )
                    } else if path == format!("/tbo/{PRIMARY}/root-thread") {
                        (200, json!({"root_thread_id":PRIMARY_ROOT}))
                    } else if path == format!("/tbo/{TEST_DOT}/root-thread") {
                        test_root_reads += 1;
                        (
                            200,
                            json!({"root_thread_id":if matches!(mode, Mode::RootDrift) && test_root_reads>1 {OTHER_ROOT} else {TEST_ROOT}}),
                        )
                    } else if path == format!("/tbo/by-thread/{PRIMARY_ROOT}") {
                        (
                            200,
                            json!({"id":PRIMARY,"aeon_kind":"orbit","status":"active","active_root_thread_id":PRIMARY_ROOT,"messaging_room_id":"primary-room","display_name":"existing private primary"}),
                        )
                    } else if path == format!("/tbo/by-thread/{TEST_ROOT}") {
                        (
                            200,
                            json!({"id":TEST_DOT,"aeon_kind":"orbit","status":"active","active_root_thread_id":TEST_ROOT,"messaging_room_id":"test-room","display_name":label}),
                        )
                    } else if path == "/tbo?limit=25" {
                        let mut items =
                            vec![json!({"id":PRIMARY,"display_name":"existing private primary"})];
                        if !matches!(mode, Mode::UnknownMissing) {
                            items.push(json!({"id":TEST_DOT,"display_name":label}));
                        }
                        if matches!(mode, Mode::UnknownAmbiguous) {
                            items.push(json!({"id":"fixture:another","display_name":label}));
                        }
                        (200, json!({"items":items,"cursor":null}))
                    } else {
                        panic!("unexpected fixture request {method} {path}");
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
            requests,
            task,
        }
    }

    fn assert_private_report(report: &Report) {
        let encoded = serde_json::to_string(report).unwrap();
        for private in [
            PRIMARY,
            TEST_DOT,
            PRIMARY_ROOT,
            TEST_ROOT,
            OTHER_ROOT,
            FP,
            "existing private primary",
            "internal:private/opaque",
            "test-room",
        ] {
            assert!(!encoded.contains(private));
            assert!(!format!("{report:?}").contains(private));
        }
        assert!(!report.presence_backend_enabled);
        assert!(!report.message_send_validated);
        assert!(!report.dots_voice_validated);
        assert!(!report.cloud_desktop_validated);
    }

    #[tokio::test]
    async fn additional_dot_is_bound_once_without_primary_mutation_or_activation() {
        for mode in [Mode::Normal, Mode::HistoricalReceipt] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("state");
            let server = fixture(root.clone(), mode).await;
            let store = Store::acquire(&root).unwrap();
            let roots = Arc::new(Mutex::new(Vec::new()));
            let verify = |id: String| {
                let roots = roots.clone();
                async move {
                    roots.lock().await.push(id);
                    Ok(())
                }
            };
            let report = create(&store, &server.client, FP, &|| true, &verify)
                .await
                .unwrap();
            assert_eq!(report.state, "separate_test_dot_identity_verified");
            assert!(report.binding_recorded && report.binding_live_validated);
            assert_eq!(report.primary_unchanged, Some(true));
            assert_eq!(report.messaging_room_linked, Some(true));
            assert_private_report(&report);
            assert_eq!(*roots.lock().await, [PRIMARY_ROOT, TEST_ROOT]);
            assert_eq!(server.posts.load(Ordering::SeqCst), 1);
            let journal = store.load().unwrap().unwrap();
            assert_eq!(journal.received.unwrap().root, TEST_ROOT);
            let requests = server.requests.load(Ordering::SeqCst);
            let repeated = create(&store, &server.client, FP, &|| true, &verify)
                .await
                .unwrap();
            assert_eq!(
                repeated.state,
                "existing_test_dot_intent_requires_reconciliation"
            );
            assert!(!repeated.binding_live_validated);
            assert_eq!(server.requests.load(Ordering::SeqCst), requests);
            let cached = status(&root).unwrap();
            assert!(cached.binding_recorded);
            assert!(!cached.binding_live_validated);
        }
    }

    #[tokio::test]
    async fn provider_rejections_keep_intent_and_never_guess_eligibility_or_retry() {
        for code in [401, 403, 503] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("state");
            let server = fixture(root.clone(), Mode::Rejected(code)).await;
            let store = Store::acquire(&root).unwrap();
            let report = create(&store, &server.client, FP, &|| true, &|_| async { Ok(()) })
                .await
                .unwrap();
            assert_eq!(report.provider_status, Some(code));
            assert!(report.creation_outcome_unconfirmed);
            assert!(!report.binding_recorded && !report.binding_live_validated);
            assert_private_report(&report);
            assert!(store.load().unwrap().unwrap().received.is_none());
            create(&store, &server.client, FP, &|| true, &|_| async { Ok(()) })
                .await
                .unwrap();
            assert_eq!(server.posts.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn lost_creation_response_survives_restart_and_only_reads_to_reconcile() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let server = fixture(root.clone(), Mode::Unknown).await;
        {
            let store = Store::acquire(&root).unwrap();
            let report = create(&store, &server.client, FP, &|| true, &|_| async { Ok(()) })
                .await
                .unwrap();
            assert_eq!(report.state, "creation_transport_outcome_unknown");
            assert!(report.creation_outcome_unconfirmed);
            assert_eq!(report.primary_unchanged, Some(true));
            assert_private_report(&report);
        }
        let store = Store::acquire(&root).unwrap();
        let journal = store.load().unwrap().unwrap();
        assert!(journal.phase == Phase::Attempted);
        let report = reconcile(&store, &server.client, journal, &|| true, &|_| async {
            Ok(())
        })
        .await
        .unwrap();
        assert!(report.binding_live_validated);
        assert_private_report(&report);
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn missing_or_ambiguous_reconciliation_never_retries_creation() {
        for mode in [Mode::UnknownMissing, Mode::UnknownAmbiguous] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("state");
            let server = fixture(root.clone(), mode).await;
            let store = Store::acquire(&root).unwrap();
            create(&store, &server.client, FP, &|| true, &|_| async { Ok(()) })
                .await
                .unwrap();
            let journal = store.load().unwrap().unwrap();
            let report = reconcile(&store, &server.client, journal, &|| true, &|_| async {
                Ok(())
            })
            .await
            .unwrap();
            assert!(!report.binding_recorded && !report.binding_live_validated);
            assert!(report.creation_outcome_unconfirmed);
            assert_eq!(server.posts.load(Ordering::SeqCst), 1);
            assert_private_report(&report);
        }
    }

    #[tokio::test]
    async fn identity_room_root_and_primary_drift_refuse_without_corrective_effects() {
        for mode in [Mode::RootDrift, Mode::PrimaryDrift, Mode::WrongRoom] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("state");
            let server = fixture(root.clone(), mode).await;
            let store = Store::acquire(&root).unwrap();
            let report = create(&store, &server.client, FP, &|| true, &|_| async { Ok(()) })
                .await
                .unwrap();
            assert!(!report.binding_recorded && !report.binding_live_validated);
            if matches!(mode, Mode::PrimaryDrift) {
                assert_eq!(report.primary_unchanged, Some(false));
            }
            assert_eq!(server.posts.load(Ordering::SeqCst), 1);
            assert_private_report(&report);
        }
    }

    #[tokio::test]
    async fn account_change_and_corrupt_intent_cannot_activate_or_post_again() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let server = fixture(root.clone(), Mode::Normal).await;
        let store = Store::acquire(&root).unwrap();
        let checks = AtomicUsize::new(0);
        let current = || checks.fetch_add(1, Ordering::SeqCst) == 0;
        let report = create(&store, &server.client, FP, &current, &|_| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(report.state, "test_dot_subscription_changed");
        assert!(!report.binding_recorded && !report.binding_live_validated);
        assert!(store.load().unwrap().unwrap().phase == Phase::Received);
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
        assert_private_report(&report);
        crate::file_watcher::atomic_write(&root.join("test-dot.json"), b"corrupt fixture intent")
            .unwrap();
        assert!(
            create(&store, &server.client, FP, &|| true, &|_| async { Ok(()) })
                .await
                .is_err()
        );
        assert_eq!(server.posts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unprepared_status_is_read_only_and_does_not_need_auth_or_make_directories() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("not-created");
        assert_eq!(status(&missing).unwrap().state, "test_dot_not_prepared");
        assert!(!missing.exists());
    }
}
