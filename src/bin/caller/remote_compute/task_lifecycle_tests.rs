//! Hermetic regressions for the executor races exposed by MCP Tasks.
use super::*;
use std::{
    future::{poll_fn, Future},
    pin::Pin,
    task::Poll,
};

struct Job(String);
impl Job {
    fn queued(owner: Option<&str>) -> Self {
        let id = format!("remote-dispatch-test-{}", uuid::Uuid::new_v4());
        insert_home_job(
            RemoteCommandJobView {
                job_id: id.clone(),
                host: format!("cloud:task_e_{}", uuid::Uuid::new_v4().simple()),
                state: RemoteCommandState::Queued,
                program: "fixture".into(),
                arg_count: 0,
                cwd: None,
                expected_revision: "0123456".into(),
                require_clean: true,
                timeout_s: 10,
                source: RemoteSourceMode::GitRevision,
                cache: RemoteCacheMode::None,
                acquisition: None,
                snapshot_digest: None,
                snapshot_bytes: None,
                created_at_unix_ms: 0,
                updated_at_unix_ms: 0,
                result: None,
                error: None,
            },
            owner.map(str::to_owned),
        )
        .unwrap();
        Self(id)
    }
    fn state(&self) -> RemoteCommandState {
        read_home_job(&self.0, &RemoteCommandCaller::Unrestricted)
            .unwrap()
            .state
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        remove_home_job(&self.0);
    }
}
async fn assert_pending<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("operation completed before its dependency"),
    })
    .await;
}

#[tokio::test]
async fn cancellation_before_dispatch_prevents_the_start_frame() {
    let job = Job::queued(None);
    let caller = RemoteCommandCaller::Unrestricted;
    assert_eq!(
        cancel_remote_command(&job.0, &caller).await.unwrap().state,
        RemoteCommandState::Cancelled
    );
    let (sender, mut receiver) = mpsc::channel(1);
    assert!(!dispatch_home_command(&job.0, &sender, "start".into())
        .await
        .unwrap());
    assert!(receiver.try_recv().is_err());
    assert_eq!(job.state(), RemoteCommandState::Cancelled);
}

#[tokio::test]
async fn backpressured_start_cannot_be_prematurely_declared_cancelled() {
    let job = Job::queued(None);
    let caller = RemoteCommandCaller::Unrestricted;
    let (sender, mut receiver) = mpsc::channel(1);
    sender.send("occupied".into()).await.unwrap();
    let dispatch = dispatch_home_command(&job.0, &sender, "start".into());
    tokio::pin!(dispatch);
    assert_pending(dispatch.as_mut()).await;
    let cancel = cancel_remote_command(&job.0, &caller);
    tokio::pin!(cancel);
    assert_pending(cancel.as_mut()).await;
    assert_eq!(job.state(), RemoteCommandState::Queued);
    assert_eq!(receiver.recv().await.unwrap(), "occupied");
    assert!(dispatch.await.unwrap());
    assert_eq!(job.state(), RemoteCommandState::Running);
    assert_eq!(receiver.recv().await.unwrap(), "start");
    // The fixture intentionally has no attached worker. Cancellation must use
    // the Running path and report detachment, never fabricate Cancelled.
    let result = cancel.await.unwrap();
    assert_eq!(result.state, RemoteCommandState::Failed);
    assert!(result.error.unwrap().contains("detached"));
}

#[tokio::test]
async fn completion_wins_while_cancellation_waits_for_dispatch_gate() {
    let job = Job::queued(None);
    let caller = RemoteCommandCaller::Unrestricted;
    let gate = home_job_dispatch_gate(&job.0, &caller).unwrap();
    let dispatch = gate.lock().await;
    let cancel = cancel_remote_command(&job.0, &caller);
    tokio::pin!(cancel);
    assert_pending(cancel.as_mut()).await;
    update_home_job(&job.0, |job| job.state = RemoteCommandState::Succeeded).unwrap();
    drop(dispatch);
    assert_eq!(cancel.await.unwrap().state, RemoteCommandState::Succeeded);
}

#[tokio::test]
async fn dispatch_gate_preserves_agent_session_ownership() {
    let job = Job::queued(Some("owner"));
    let stranger = RemoteCommandCaller::AgentSession("stranger".into());
    assert!(home_job_dispatch_gate(&job.0, &stranger).is_err());
    assert!(cancel_remote_command(&job.0, &stranger).await.is_err());
    assert_eq!(job.state(), RemoteCommandState::Queued);
    let owner = RemoteCommandCaller::AgentSession("owner".into());
    assert_eq!(
        cancel_remote_command(&job.0, &owner).await.unwrap().state,
        RemoteCommandState::Cancelled
    );
}
