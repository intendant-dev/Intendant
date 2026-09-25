use super::*;

fn principal() -> TerminalActor {
    TerminalActor::Principal("principal:test:continuity".to_owned())
}

fn policy() -> ShellSpawnPolicy {
    ShellSpawnPolicy {
        may_spawn: true,
        ..Default::default()
    }
}

#[test]
fn terminal_reference_roundtrip_and_rejects_malformed_identity() {
    let boot = "a".repeat(26);
    let instance = "b".repeat(32);
    for name in ["shell-0", "shell with spaces", "a/b.c", "terminal-λ"] {
        let encoded = TerminalReference::encode(&boot, &instance, name);
        let parsed = TerminalReference::parse(&encoded).unwrap().unwrap();
        assert_eq!(parsed.boot_id, boot);
        assert_eq!(parsed.instance_id, instance);
        assert_eq!(parsed.name, name);
    }
    assert!(TerminalReference::parse("shell-0").unwrap().is_none());
    for invalid in [
        "iterm1.",
        "iterm1...",
        "iterm1../etc/passwd",
        "iterm1.a.b.c",
    ] {
        assert!(TerminalReference::parse(invalid)
            .unwrap_err()
            .contains("terminal_invalid_handle"));
    }
    let empty_name = TerminalReference::encode(&boot, &instance, "");
    assert!(TerminalReference::parse(&empty_name).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_reference_never_targets_replacement_or_another_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = TerminalRegistry::new(tmp.path().to_path_buf());
    let actor = principal();
    let key = TerminalKey::local("reused-name");
    let (first, _) = registry
        .open_mcp("reused-name", 80, 24, &actor, policy())
        .await
        .unwrap();
    let old = registry.reference_for(&key, &first.instance_id);
    assert!(registry.close_mcp_visible(&old, &actor).await.unwrap());
    let (second, created) = registry
        .open_mcp("reused-name", 80, 24, &actor, policy())
        .await
        .unwrap();
    assert!(created);
    let new = registry.reference_for(&key, &second.instance_id);
    assert_ne!(old, new);
    assert!(registry.get_mcp_visible(&old, &actor).await.is_err());
    assert!(registry.close_mcp_visible(&old, &actor).await.is_err());
    assert!(registry
        .open_mcp(&old, 80, 24, &actor, policy())
        .await
        .is_err());
    assert!(Arc::ptr_eq(
        &registry
            .get_mcp_visible(&new, &actor)
            .await
            .unwrap()
            .unwrap(),
        &second
    ));
    let successor = TerminalRegistry::new(tmp.path().to_path_buf());
    assert!(successor.get_mcp_visible(&new, &actor).await.is_err());
    assert!(successor
        .open_mcp(&new, 80, 24, &actor, policy())
        .await
        .is_err());
    assert_eq!(successor.len().await, 0);
    registry.close_mcp_visible(&new, &actor).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_drain_keeps_state_and_exited_output_until_explicit_close() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = TerminalRegistry::new(tmp.path().to_path_buf());
    let actor = principal();
    let (session, _) = registry
        .open_mcp("held", 80, 24, &actor, policy())
        .await
        .unwrap();
    let reference = registry.reference_for(&TerminalKey::local("held"), &session.instance_id);
    let rows = registry.freeze_for_drain().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].terminal_id, "held");
    assert!(registry
        .open_mcp("new", 80, 24, &actor, policy())
        .await
        .err()
        .unwrap()
        .contains("daemon_draining"));
    let (attached, created) = registry
        .open_mcp(&reference, 80, 24, &actor, policy())
        .await
        .unwrap();
    assert!(!created);
    assert!(Arc::ptr_eq(&session, &attached));
    session
        .output
        .lock()
        .unwrap()
        .fan_out(b"retained final output\r\n");
    session.mark_exited(17);
    assert_eq!(
        registry.freeze_for_drain().await.len(),
        1,
        "exited scrollback still holds the drain"
    );
    let (exited, created) = registry
        .open_mcp(&reference, 80, 24, &actor, policy())
        .await
        .unwrap();
    assert!(!created, "qualified open must not respawn a dead PTY");
    assert!(!exited.is_alive());
    let (output, _, _) = exited.read_since(0, 65536);
    assert!(String::from_utf8_lossy(&output).contains("retained final output"));
    assert_eq!(exited.exit_status(), Some(17));
    assert!(registry
        .close_mcp_visible(&reference, &actor)
        .await
        .unwrap());
    assert!(registry.freeze_for_drain().await.is_empty());
    assert!(registry
        .open_mcp(&reference, 80, 24, &actor, policy())
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_reference_preserves_visibility_and_sharing() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = TerminalRegistry::new(tmp.path().to_path_buf());
    let owner = principal();
    let stranger = TerminalActor::Principal("principal:test:other".to_owned());
    let (session, _) = registry
        .open_mcp("private", 80, 24, &owner, policy())
        .await
        .unwrap();
    let key = TerminalKey::local("private");
    let reference = registry.reference_for(&key, &session.instance_id);
    assert!(registry
        .get_mcp_visible(&reference, &stranger)
        .await
        .is_err());
    assert!(registry
        .close_mcp_visible(&reference, &stranger)
        .await
        .is_err());
    assert!(registry
        .open_mcp(&reference, 80, 24, &stranger, policy())
        .await
        .is_err());
    assert!(registry
        .get_mcp_visible(&reference, &owner)
        .await
        .unwrap()
        .is_some());
    assert_eq!(registry.set_shared(&key, &owner, true).await, Some(true));
    assert!(registry
        .get_mcp_visible(&reference, &stranger)
        .await
        .unwrap()
        .is_some());
    registry
        .close_mcp_visible(&reference, &owner)
        .await
        .unwrap();
}

#[tokio::test]
async fn terminal_drain_holds_opening_reservations_and_reaps_cancelled_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = TerminalRegistry::new(tmp.path().to_path_buf());
    let (done_tx, done_rx) = watch::channel(false);
    registry.sessions.write().await.insert(
        TerminalKey::local("opening"),
        SessionSlot::Opening(Arc::new(OpeningSlot {
            done: done_rx,
            prev: None,
        })),
    );
    let rows = registry.freeze_for_drain().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "opening");
    drop(done_tx);
    assert!(
        registry.freeze_for_drain().await.is_empty(),
        "a cancelled reservation must not pin the daemon forever"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn terminal_open_admitted_before_drain_can_finish_but_late_open_cannot() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = Arc::new(TerminalRegistry::new(tmp.path().to_path_buf()));
    let cwd = tmp.path().to_path_buf();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let opener = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .open_or_attach_with(
                    TerminalKey::local("in-flight"),
                    &principal(),
                    true,
                    move || {
                        let _ = started_tx.send(());
                        release_rx
                            .recv_timeout(Duration::from_secs(10))
                            .map_err(|_| "test release timed out".to_owned())?;
                        PtySession::spawn(
                            80,
                            24,
                            Some(cwd),
                            Some("principal:test:continuity".to_owned()),
                            false,
                            None,
                        )
                    },
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), started_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(registry.freeze_for_drain().await[0].state, "opening");
    assert!(registry
        .open_mcp("late", 80, 24, &principal(), policy())
        .await
        .is_err());
    release_tx.send(()).unwrap();
    let (session, created) = opener.await.unwrap().unwrap();
    assert!(created);
    assert!(session.is_alive());
    assert_eq!(registry.freeze_for_drain().await.len(), 1);
    let reference = registry.reference_for(&TerminalKey::local("in-flight"), &session.instance_id);
    registry
        .close_mcp_visible(&reference, &principal())
        .await
        .unwrap();
    assert!(registry.freeze_for_drain().await.is_empty());
}
