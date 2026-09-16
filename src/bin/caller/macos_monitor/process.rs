//! Exact child ownership. A failed exchange permanently retires this broker;
//! neither a PID nor a native display ID can be adopted or used for cleanup.

use super::protocol::*;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub(super) struct Process {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    seq: u64,
}

impl Process {
    pub(super) async fn spawn() -> Result<Self, String> {
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        Self::spawn_command(Command::new(executable).arg(HELPER_ARG)).await
    }

    async fn spawn_command(command: &mut Command) -> Result<Self, String> {
        let mut child = command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| e.to_string())?;
        let input = child.stdin.take().ok_or("missing helper stdin")?;
        let output = child.stdout.take().ok_or("missing helper stdout")?;
        let mut process = Self {
            child,
            input: Some(input),
            output: BufReader::new(output),
            seq: 0,
        };
        let ready = tokio::time::timeout(Duration::from_secs(6), process.read_reply()).await;
        if !matches!(
            ready,
            Ok(Ok(Reply {
                seq: 0,
                result: Outcome::Ready { version: VERSION }
            }))
        ) {
            let detail = match ready {
                Ok(Ok(Reply {
                    result: Outcome::Error { message, .. },
                    ..
                })) => message,
                _ => "unavailable or invalid protocol handshake".into(),
            };
            let _ = process.close().await;
            return Err(format!("macOS monitor helper: {detail}; broker retired"));
        }
        Ok(process)
    }

    async fn read_reply(&mut self) -> Result<Reply, String> {
        let mut line = Vec::new();
        loop {
            let bytes = self.output.fill_buf().await.map_err(|e| e.to_string())?;
            if bytes.is_empty() {
                return Err("monitor helper closed stdout".into());
            }
            let n = bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |n| n + 1);
            if line.len() + n > MAX_LINE {
                return Err("monitor reply too long".into());
            }
            line.extend_from_slice(&bytes[..n]);
            self.output.consume(n);
            if line.last() == Some(&b'\n') {
                return serde_json::from_slice(&line).map_err(|e| e.to_string());
            }
        }
    }

    pub(super) fn live(&mut self) -> Result<(), String> {
        match self.child.try_wait().map_err(|e| e.to_string())? {
            None => Ok(()),
            Some(_) => Err("monitor helper exited; broker retired".into()),
        }
    }

    pub(super) async fn exchange(&mut self, op: Operation) -> Result<Outcome, String> {
        self.live()?;
        self.seq = self
            .seq
            .checked_add(1)
            .ok_or("monitor sequence exhausted")?;
        let seq = self.seq;
        let line = encode(&Request { seq, op })?;
        let reply = tokio::time::timeout(Duration::from_secs(6), async {
            let input = self.input.as_mut().ok_or("monitor helper is closed")?;
            input.write_all(&line).await.map_err(|e| e.to_string())?;
            input.flush().await.map_err(|e| e.to_string())?;
            self.read_reply().await
        })
        .await
        .map_err(|_| "monitor protocol deadline exceeded; cleanup required")??;
        if reply.seq != seq {
            return Err("monitor response sequence mismatch".into());
        }
        self.live()?;
        match reply.result {
            Outcome::Error {
                message,
                fatal: true,
            } => Err(message),
            result => Ok(result),
        }
    }

    pub(super) async fn close(&mut self) -> Result<(), String> {
        // EOF asks the main-thread owner to release its monitors. A stalled
        // helper is killed via this retained Child only. Reaping has a second
        // bounded deadline; on timeout the retained Child's kill-on-drop and
        // Tokio reaper remain responsible. Exit/kill uncertainty never permits
        // respawn and is never reported as successful native cleanup.
        self.input.take();
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(_)) => Err("monitor helper reported unsuccessful cleanup".into()),
            _ => {
                let _ = self.child.start_kill();
                tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await
                    .map_err(|_| "monitor helper reap deadline exceeded; retained child kill-on-drop remains armed")?
                    .map_err(|e| e.to_string())?;
                Err("monitor helper required termination; native cleanup unconfirmed".into())
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn eof_reaps_exact_child_and_rejects_bad_handshake() {
        // Hermetic pipe fixtures only. No installed daemon or GUI calls.
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("printf '%s\\n' '{\"seq\":0,\"result\":{\"status\":\"ready\",\"version\":1}}'; cat >/dev/null");
        let mut process = Process::spawn_command(&mut command).await.unwrap();
        assert!(process.live().is_ok());
        process.close().await.unwrap();
        assert!(process.child.try_wait().unwrap().is_some());
        assert!(process.live().is_err());
        let mut bad = Command::new("/bin/sh");
        bad.arg("-c").arg("printf 'not-json\\n'");
        assert!(Process::spawn_command(&mut bad).await.is_err());
    }
}
