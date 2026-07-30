//! Kimi server-process lifecycle and confined review-turn monitoring.

use super::*;

const MAX_SERVER_INSTANCE_FILES: usize = 256;
const MAX_SERVER_INSTANCE_BYTES: u64 = 16 * 1024;

#[derive(serde::Deserialize)]
struct KimiServerInstanceRegistration {
    pid: u64,
    host: String,
    port: u64,
    started_at: u64,
}

pub(super) async fn wait_for_server_origin(
    stdout: tokio::process::ChildStdout,
) -> Result<(String, BufReader<tokio::process::ChildStdout>), CallerError> {
    let mut reader = BufReader::new(stdout);
    let future = async {
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader
                .read_line(&mut line)
                .await
                .map_err(|_| external("failed to read Kimi server readiness output"))?;
            if read == 0 {
                return Err(external("Kimi server exited before becoming ready"));
            }
            if let Some(origin) = extract_loopback_origin(&line) {
                return Ok(origin);
            }
            // Deliberately do not retain or forward banner lines: the ready
            // banner contains Kimi's bearer token.
        }
    };
    let origin = tokio::time::timeout(STARTUP_TIMEOUT, future)
        .await
        .map_err(|_| external("timed out waiting for Kimi server readiness"))??;
    Ok((origin, reader))
}

/// Kimi 0.28+ publishes each foreground web server under its private
/// `<KIMI_CODE_HOME>/server/instances` registry. Discover the actually-bound
/// ephemeral port from the entry owned by the exact child we spawned. This is
/// stronger than parsing the presentation banner (which carries the bearer and
/// has changed independently of the API) and avoids a reserve/release port
/// race.
pub(super) async fn wait_for_registered_server_origin(
    bridge_home: &Path,
    baseline: &HashSet<PathBuf>,
    child_pid: Option<u32>,
    child: &mut Child,
) -> Result<String, CallerError> {
    let child_pid = child_pid.ok_or_else(|| external("Kimi server process has no PID"))?;
    let instances = bridge_home.join("server").join("instances");
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        ensure_server_child_running(child, "publishing its instance registration")?;
        if let Some(origin) = registered_server_origin(&instances, baseline, child_pid).await? {
            return Ok(origin);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(external(
                "timed out waiting for Kimi server instance registration",
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) async fn capture_server_instance_baseline(
    bridge_home: &Path,
) -> Result<HashSet<PathBuf>, CallerError> {
    let instances = bridge_home.join("server").join("instances");
    let mut entries = match tokio::fs::read_dir(instances).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashSet::new())
        }
        Err(_) => return Err(external("failed to inspect Kimi server instance registry")),
    };
    let mut baseline = HashSet::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|_| external("failed to enumerate Kimi server instance registry"))?
    {
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if baseline.len() >= MAX_SERVER_INSTANCE_FILES {
            return Err(external(
                "Kimi server instance registry is unreasonably large",
            ));
        }
        baseline.insert(entry.path());
    }
    Ok(baseline)
}

async fn registered_server_origin(
    instances: &Path,
    baseline: &HashSet<PathBuf>,
    child_pid: u32,
) -> Result<Option<String>, CallerError> {
    let mut entries = match tokio::fs::read_dir(instances).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(external("failed to inspect Kimi server instance registry")),
    };
    let mut seen = 0usize;
    let mut newest: Option<KimiServerInstanceRegistration> = None;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|_| external("failed to enumerate Kimi server instance registry"))?
    {
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if baseline.contains(&entry.path()) {
            continue;
        }
        seen += 1;
        if seen > MAX_SERVER_INSTANCE_FILES {
            return Err(external(
                "Kimi server instance registry is unreasonably large",
            ));
        }
        let metadata = match tokio::fs::symlink_metadata(entry.path()).await {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(external("failed to inspect Kimi server instance entry")),
        };
        if metadata.len() > MAX_SERVER_INSTANCE_BYTES {
            continue;
        }
        let bytes = match tokio::fs::read(entry.path()).await {
            Ok(bytes) if bytes.len() as u64 <= MAX_SERVER_INSTANCE_BYTES => bytes,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(external("failed to read Kimi server instance entry")),
        };
        let Ok(registration) = serde_json::from_slice::<KimiServerInstanceRegistration>(&bytes)
        else {
            // Kimi deliberately retains unparseable entries because another
            // process may own them. They cannot identify our exact child.
            continue;
        };
        if registration.pid != u64::from(child_pid) {
            continue;
        }
        if newest
            .as_ref()
            .is_none_or(|current| registration.started_at > current.started_at)
        {
            newest = Some(registration);
        }
    }

    let Some(registration) = newest else {
        return Ok(None);
    };
    if registration.port == 0 {
        // The initial atomic registry write precedes the listen call; Kimi
        // rewrites it with the kernel-selected port once binding succeeds.
        return Ok(None);
    }
    if registration.host != "127.0.0.1" {
        return Err(external(
            "refusing non-loopback Kimi server instance registration",
        ));
    }
    let port = u16::try_from(registration.port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| external("Kimi server instance registration has an invalid port"))?;
    Ok(Some(format!("http://127.0.0.1:{port}")))
}

/// Kimi 0.28+ removed the 0.27 `server run` entrypoint. Detect only its bounded,
/// post-exit diagnostic and retry the same configured executable with the new
/// foreground `web --no-open` entrypoint. The captured stderr is never logged:
/// future Kimi builds may include sensitive startup material there.
pub(super) async fn legacy_server_entrypoint_removed(stderr: tokio::process::ChildStderr) -> bool {
    const LIMIT: u64 = 8 * 1024;
    let mut bytes = Vec::new();
    let mut bounded = stderr.take(LIMIT + 1);
    let read = tokio::time::timeout(Duration::from_secs(1), bounded.read_to_end(&mut bytes)).await;
    if !matches!(read, Ok(Ok(_))) || bytes.len() as u64 > LIMIT {
        return false;
    }
    legacy_server_entrypoint_removed_text(&String::from_utf8_lossy(&bytes))
}

pub(super) fn legacy_server_entrypoint_removed_text(message: &str) -> bool {
    let message = super::super::strip_ansi_escapes(message).to_ascii_lowercase();
    (message.contains("`kimi server` has been deprecated")
        && message.contains("use `kimi web` instead"))
        || message.contains("unknown command 'server'")
}

pub(super) fn extract_loopback_origin(line: &str) -> Option<String> {
    let clean = super::super::strip_ansi_escapes(line);
    for prefix in ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"] {
        let Some(start) = clean.find(prefix) else {
            continue;
        };
        let rest = &clean[start..];
        let end = rest
            .find(|character: char| character == '#' || character.is_whitespace())
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        if reqwest::Url::parse(candidate)
            .ok()
            .and_then(|url| url.port())
            .is_some()
        {
            return Some(candidate.trim_end_matches('/').to_string());
        }
    }
    None
}

pub(super) async fn wait_for_server_token(
    path: &Path,
    child: &mut Child,
) -> Result<String, CallerError> {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        ensure_server_child_running(child, "publishing its token file")?;
        match tokio::fs::read_to_string(path).await {
            Ok(token) => {
                validate_token_permissions(path).await?;
                let token = token.trim();
                if token.len() < 16
                    || token.len() > 4096
                    || !token.bytes().all(|byte| byte.is_ascii_graphic())
                {
                    return Err(external("Kimi server token file is malformed"));
                }
                return Ok(token.to_string());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(external("failed to read Kimi server token file")),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(external("timed out waiting for Kimi server token file"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) async fn wait_for_server_health(
    api: &KimiApi,
    child: &mut Child,
) -> Result<(), CallerError> {
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        ensure_server_child_running(child, "passing its health check")?;
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(external("timed out waiting for Kimi server health"));
        }
        let attempt_timeout = std::cmp::min(deadline - now, Duration::from_secs(2));
        if matches!(
            tokio::time::timeout(attempt_timeout, api.health()).await,
            Ok(Ok(()))
        ) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn ensure_server_child_running(child: &mut Child, phase: &str) -> Result<(), CallerError> {
    match child.try_wait() {
        Ok(Some(status)) => Err(external(format!(
            "Kimi server exited before {phase} ({status})"
        ))),
        Ok(None) => Ok(()),
        Err(_) => Err(external("failed to inspect Kimi server process")),
    }
}

#[cfg(unix)]
pub(super) async fn validate_token_permissions(path: &Path) -> Result<(), CallerError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| external("failed to inspect Kimi server token file"))?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(external(
            "refusing Kimi server token readable by group or other users",
        ));
    }
    Ok(())
}

/// Kimi persists its loopback bearer for its own `server ps/kill` commands.
/// Intendant owns the child PID and holds the captured token in memory, so the
/// file is unnecessary after handshake and would only expose the private v2
/// control surface to Kimi's own absolute-path file tools.
pub(super) async fn remove_captured_server_token(path: &Path) -> Result<(), CallerError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| external("failed to inspect captured Kimi server token"))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(external(
            "refusing a non-regular Kimi server token before cleanup",
        ));
    }
    tokio::fs::remove_file(path).await.map_err(|error| {
        external(format!(
            "failed to remove captured Kimi server token: {error}"
        ))
    })?;
    match tokio::fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(external("Kimi server token remained on disk after cleanup")),
        Err(error) => Err(external(format!(
            "could not verify Kimi server token cleanup: {error}"
        ))),
    }
}

#[cfg(windows)]
pub(super) async fn validate_token_permissions(path: &Path) -> Result<(), CallerError> {
    crate::platform::validate_owner_private_permissions(path).map_err(|error| {
        external(format!(
            "refusing Kimi server token outside an owner-private Windows ACL boundary: {error}"
        ))
    })
}

#[cfg(not(any(unix, windows)))]
pub(super) async fn validate_token_permissions(_path: &Path) -> Result<(), CallerError> {
    Ok(())
}

pub(super) async fn drain_silently(mut reader: impl AsyncRead + Unpin) {
    let mut buffer = [0u8; 4096];
    while reader
        .read(&mut buffer)
        .await
        .ok()
        .is_some_and(|read| read > 0)
    {}
}

pub(super) async fn terminate_spawned_child(
    pid: Option<u32>,
    child: &mut Child,
    bridge_home: &Path,
) {
    // A failed handshake must not leave the loopback bearer on disk. This is
    // safe before and after capture: NotFound is the ordinary pre-token case.
    let _ = tokio::fs::remove_file(bridge_home.join("server.token")).await;
    if let Some(pid) = pid {
        crate::platform::terminate_process_tree_now(pid);
        super::super::unregister_child_process(pid);
    } else {
        let _ = child.start_kill();
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
}

pub(super) async fn clear_review_lease_if_nonce(
    slot: &Arc<tokio::sync::Mutex<Option<KimiReviewToolLease>>>,
    nonce: &str,
) {
    let mut lease = slot.lock().await;
    if lease.as_ref().is_some_and(|lease| lease.nonce == nonce) {
        *lease = None;
    }
}

pub(super) async fn retain_review_lease_if_empty(
    slot: &Arc<tokio::sync::Mutex<Option<KimiReviewToolLease>>>,
    lease: &KimiReviewToolLease,
) {
    let mut current = slot.lock().await;
    if current.is_none() {
        *current = Some(lease.clone());
    }
}

/// Recover a failed or protocol-drifted review submission without ever
/// widening tools around a prompt whose absence has not been proved.
pub(super) async fn fail_review_submission_closed(
    api: &KimiApi,
    rpc: &KimiRpcApi,
    leases: &Arc<tokio::sync::Mutex<Option<KimiReviewToolLease>>>,
    lease: &KimiReviewToolLease,
    reason: &str,
) -> CallerError {
    match stop_review_prompt_before_restore(api, lease).await {
        Ok(true) => {
            let restore = restore_review_tools_if_unchanged(rpc, lease).await;
            clear_review_lease_if_nonce(leases, &lease.nonce).await;
            match restore {
                Ok(_) => external(reason.to_string()),
                Err(error) => external(format!(
                    "{reason}; the prompt is stopped, but Kimi's prior tools could not be restored: {error}"
                )),
            }
        }
        Ok(false) => {
            retain_review_lease_if_empty(leases, lease).await;
            external(format!(
                "{reason}; Intendant could not prove the submitted prompt stopped, so Kimi remains confined to zero active tools"
            ))
        }
        Err(error) => {
            retain_review_lease_if_empty(leases, lease).await;
            external(format!(
                "{reason}; Kimi prompt state could not be verified ({error}), so Intendant left zero active tools and blocked widening"
            ))
        }
    }
}

/// Restore the pre-review set only while the live set is still exactly the
/// temporary review set. A dashboard/CLI/Kimi-UI tool change is authoritative
/// and must not be overwritten by a late review-completion poll.
pub(super) async fn restore_review_tools_if_unchanged(
    rpc: &KimiRpcApi,
    lease: &KimiReviewToolLease,
) -> Result<bool, CallerError> {
    let current = rpc.tools(&lease.session_id, &lease.agent_id).await?;
    if active_tool_names(&current) != lease.review_tools {
        return Ok(false);
    }
    rpc.set_active_tools(&lease.session_id, &lease.agent_id, &lease.previous_tools)
        .await?;
    Ok(true)
}

/// Stop the exact review prompt and prove it left both active and queued
/// slots before any broader tool set is restored. `false` is a safe timeout:
/// callers leave the review tools confined and terminate the server.
pub(super) async fn stop_review_prompt_before_restore(
    api: &KimiApi,
    lease: &KimiReviewToolLease,
) -> Result<bool, CallerError> {
    if lease
        .prompt_id
        .as_ref()
        .is_some_and(|id| lease.baseline_prompt_ids.contains(id))
    {
        return Err(external(
            "Kimi review prompt id collided with a pre-existing prompt",
        ));
    }

    // Even when submission returned an error or omitted its id, the HTTP
    // request may have reached Kimi. Observe a full grace window, aborting
    // only ids absent from the pre-submit snapshot.
    for _ in 0..20 {
        let prompts = api.list_prompts(&lease.session_id).await?;
        let current = pending_prompt_ids(&prompts)?;
        let mut review_ids = current
            .difference(&lease.baseline_prompt_ids)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(prompt_id) = lease.prompt_id.as_ref() {
            if current.contains(prompt_id) && !review_ids.contains(prompt_id) {
                review_ids.push(prompt_id.clone());
            }
        }
        for prompt_id in review_ids {
            api.abort_prompt(&lease.session_id, &prompt_id).await?;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let prompts = api.list_prompts(&lease.session_id).await?;
    let current = pending_prompt_ids(&prompts)?;
    let no_new_prompt = current.is_subset(&lease.baseline_prompt_ids);
    let exact_prompt_absent = lease
        .prompt_id
        .as_ref()
        .is_none_or(|prompt_id| !current.contains(prompt_id));
    Ok(no_new_prompt && exact_prompt_absent)
}

pub(super) async fn monitor_review_prompt(
    api: KimiApi,
    rpc: KimiRpcApi,
    leases: Arc<tokio::sync::Mutex<Option<KimiReviewToolLease>>>,
    events: Option<mpsc::UnboundedSender<AgentEvent>>,
    lease: KimiReviewToolLease,
) {
    let Some(prompt_id) = lease.prompt_id.as_deref() else {
        return;
    };
    loop {
        let still_owned = leases
            .lock()
            .await
            .as_ref()
            .is_some_and(|current| current.nonce == lease.nonce);
        if !still_owned {
            return;
        }
        match api.list_prompts(&lease.session_id).await {
            Ok(prompts) => {
                let pending = match prompt_is_pending(&prompts, prompt_id) {
                    Ok(pending) => pending,
                    Err(error) => {
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: format!(
                                    "Kimi returned malformed review prompt state; restoration will retry: {}",
                                    bounded_wire_text(&error.to_string(), 1024)
                                ),
                            });
                        }
                        tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
                        continue;
                    }
                };
                match rpc.tools(&lease.session_id, &lease.agent_id).await {
                    Ok(inventory) if active_tool_names(&inventory) != lease.review_tools => {
                        if pending {
                            if let Err(error) = api.abort_prompt(&lease.session_id, prompt_id).await
                            {
                                if let Some(events) = events.as_ref() {
                                    let _ = events.send(AgentEvent::Log {
                                        level: "warn".into(),
                                        message: format!(
                                            "Kimi review tool confinement changed and prompt abort will retry: {}",
                                            bounded_wire_text(&error.to_string(), 1024)
                                        ),
                                    });
                                }
                            }
                            tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
                            continue;
                        }
                        clear_review_lease_if_nonce(&leases, &lease.nonce).await;
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: "Kimi review aborted because the active tool set changed; preserving the operator's newer tool set"
                                    .into(),
                            });
                        }
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: format!(
                                    "Could not verify Kimi review tool confinement; aborting the review: {}",
                                    bounded_wire_text(&error.to_string(), 1024)
                                ),
                            });
                        }
                        if pending {
                            let _ = api.abort_prompt(&lease.session_id, prompt_id).await;
                            tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
                            continue;
                        }
                        clear_review_lease_if_nonce(&leases, &lease.nonce).await;
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: "Kimi review ended, but current tools could not be verified; preserving them instead of risking an overwrite"
                                    .into(),
                            });
                        }
                        return;
                    }
                }
                if pending {
                    tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
                    continue;
                }
                match restore_review_tools_if_unchanged(&rpc, &lease).await {
                    Ok(restored) => {
                        clear_review_lease_if_nonce(&leases, &lease.nonce).await;
                        if !restored {
                            if let Some(events) = events.as_ref() {
                                let _ = events.send(AgentEvent::Log {
                                level: "info".into(),
                                message: "Kimi review finished; preserving a newer operator tool-set change"
                                    .into(),
                            });
                            }
                        }
                        return;
                    }
                    Err(error) => {
                        if let Some(events) = events.as_ref() {
                            let _ = events.send(AgentEvent::Log {
                                level: "warn".into(),
                                message: format!(
                                    "Kimi review finished, but tool restoration will retry: {}",
                                    bounded_wire_text(&error.to_string(), 1024)
                                ),
                            });
                        }
                    }
                }
            }
            Err(error) => {
                if let Some(events) = events.as_ref() {
                    let _ = events.send(AgentEvent::Log {
                        level: "warn".into(),
                        message: format!(
                            "Could not inspect Kimi review prompt state; restoration will retry: {}",
                            bounded_wire_text(&error.to_string(), 1024)
                        ),
                    });
                }
            }
        }
        tokio::time::sleep(REVIEW_PROMPT_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write_registration(
        directory: &Path,
        name: &str,
        pid: u64,
        host: &str,
        port: u64,
        started_at: u64,
    ) {
        tokio::fs::write(
            directory.join(name),
            serde_json::json!({
                "server_id": name,
                "pid": pid,
                "host": host,
                "port": port,
                "started_at": started_at,
                "heartbeat_at": started_at,
            })
            .to_string(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn registered_origin_is_bound_to_the_newest_exact_child_entry() {
        let temp = tempfile::tempdir().unwrap();
        let instances = temp.path().join("server").join("instances");
        tokio::fs::create_dir_all(&instances).await.unwrap();
        tokio::fs::write(instances.join("malformed.json"), b"{")
            .await
            .unwrap();
        write_registration(&instances, "sibling.json", 41, "127.0.0.1", 49_999, 9_000).await;
        write_registration(&instances, "stale.json", 42, "127.0.0.1", 41_000, 1_000).await;
        let baseline = capture_server_instance_baseline(temp.path()).await.unwrap();
        write_registration(&instances, "current.json", 42, "127.0.0.1", 0, 2_000).await;

        assert_eq!(
            registered_server_origin(&instances, &baseline, 42)
                .await
                .unwrap(),
            None
        );

        write_registration(&instances, "current.json", 42, "127.0.0.1", 42_000, 2_000).await;
        assert_eq!(
            registered_server_origin(&instances, &baseline, 42)
                .await
                .unwrap(),
            Some("http://127.0.0.1:42000".to_string())
        );
    }

    #[tokio::test]
    async fn registered_origin_rejects_non_loopback_and_invalid_ports() {
        let temp = tempfile::tempdir().unwrap();
        let instances = temp.path().join("instances");
        tokio::fs::create_dir_all(&instances).await.unwrap();
        let baseline = HashSet::new();
        write_registration(&instances, "current.json", 42, "0.0.0.0", 42_000, 2_000).await;
        assert!(registered_server_origin(&instances, &baseline, 42)
            .await
            .unwrap_err()
            .to_string()
            .contains("non-loopback"));

        write_registration(&instances, "current.json", 42, "127.0.0.1", 70_000, 2_000).await;
        assert!(registered_server_origin(&instances, &baseline, 42)
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid port"));
    }
}
