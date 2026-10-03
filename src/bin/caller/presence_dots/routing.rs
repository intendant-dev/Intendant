//! Isolated, read-only App Server account discovery. This is deliberately
//! separate from the voice broker and never adds to its method vocabulary.

use crate::codex_cloud::CodexAuth;
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, Take,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(40);
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_NOTIFICATIONS: usize = 100;
const SERVER_ARGS: &[&str] = &[
    "app-server",
    "-c",
    "features.plugins=false",
    "-c",
    "mcp_servers={}",
    "-c",
    "approval_policy=\"never\"",
    "-c",
    "sandbox_mode=\"read-only\"",
];
// Copy only OS/CLI essentials. No model keys, desktop proofs, inherited
// Intendant authority, proxy credentials, or arbitrary API-base override.
const CHILD_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "SystemRoot",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "APPDATA",
    "LOCALAPPDATA",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "CODEX_HOME",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    Unavailable,
    Timeout,
    Transport,
    Protocol,
    SubscriptionUnavailable,
    AccountMismatch,
    AccountChanged,
    UnsupportedRouting,
}

impl Failure {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Unavailable => "account_discovery_unavailable",
            Self::Timeout => "account_discovery_timeout",
            Self::Transport => "account_discovery_transport_failed",
            Self::Protocol => "account_discovery_protocol_refused",
            Self::SubscriptionUnavailable => "subscription_account_unavailable",
            Self::AccountMismatch => "selected_account_mismatch",
            Self::AccountChanged => "account_changed_during_discovery",
            Self::UnsupportedRouting => "account_routing_unsupported",
        }
    }
}

pub(super) async fn discover(auth: &CodexAuth) -> Result<(), Failure> {
    let neutral = tempfile::tempdir().map_err(|_| Failure::Unavailable)?;
    let mut command = crate::external_agent::spawn_backend_command(
        &crate::external_agent::AgentBackend::Codex,
        "codex",
    );
    command
        .args(SERVER_ARGS)
        .current_dir(neutral.path())
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in CHILD_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    crate::platform::die_with_parent(&mut command);
    let mut child = command.spawn().map_err(|_| Failure::Unavailable)?;
    let result = async {
        let stdout = child.stdout.take().ok_or(Failure::Unavailable)?;
        let stdin = child.stdin.take().ok_or(Failure::Unavailable)?;
        tokio::time::timeout(DISCOVERY_TIMEOUT, read_account(stdout, stdin, auth))
            .await
            .map_err(|_| Failure::Timeout)?
    }
    .await;
    // Kill and reap only this owned helper; cleanup cannot replace its
    // observed result. kill_on_drop also covers cancellation during discovery.
    let _ = tokio::time::timeout(Duration::from_secs(2), child.kill()).await;
    result
}

#[derive(Clone, Copy)]
enum ReadRequest {
    Initialize,
    Account,
    Requirements,
}

impl ReadRequest {
    fn method(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Account => "account/read",
            Self::Requirements => "configRequirements/read",
        }
    }
}

struct Client<R, W> {
    reader: BufReader<Take<R>>,
    writer: W,
    bytes: usize,
    notifications: usize,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Client<R, W> {
    fn new(reader: R, writer: W) -> Self {
        Self {
            // Bounds even a single unterminated line, before JSON allocation.
            reader: BufReader::new(reader.take((MAX_OUTPUT_BYTES + 1) as u64)),
            writer,
            bytes: 0,
            notifications: 0,
        }
    }

    async fn write(&mut self, value: Value) -> Result<(), Failure> {
        let mut bytes = serde_json::to_vec(&value).map_err(|_| Failure::Protocol)?;
        bytes.push(b'\n');
        self.writer
            .write_all(&bytes)
            .await
            .map_err(|_| Failure::Transport)?;
        self.writer.flush().await.map_err(|_| Failure::Transport)
    }

    async fn request(
        &mut self,
        id: u64,
        request: ReadRequest,
        params: Value,
    ) -> Result<Value, Failure> {
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            self.write(json!({"id": id, "method": request.method(), "params": params}))
                .await?;
            loop {
                let mut line = Vec::new();
                let read = self
                    .reader
                    .read_until(b'\n', &mut line)
                    .await
                    .map_err(|_| Failure::Transport)?;
                self.bytes += read;
                if self.bytes > MAX_OUTPUT_BYTES {
                    return Err(Failure::Protocol);
                }
                if read == 0 || line.last() != Some(&b'\n') {
                    return Err(Failure::Transport);
                }
                let mut value: Value =
                    serde_json::from_slice(&line).map_err(|_| Failure::Protocol)?;
                let object = value.as_object_mut().ok_or(Failure::Protocol)?;
                if object.contains_key("method") {
                    // No client-side approvals, tools, login, or server requests.
                    if object.contains_key("id") {
                        return Err(Failure::Protocol);
                    }
                    if object.get("method").and_then(Value::as_str) == Some("account/updated") {
                        return Err(Failure::AccountChanged);
                    }
                    self.notifications += 1;
                    if self.notifications > MAX_NOTIFICATIONS {
                        return Err(Failure::Protocol);
                    }
                    continue;
                }
                if object.get("id").and_then(Value::as_u64) != Some(id)
                    || object.contains_key("error")
                {
                    return Err(Failure::Protocol);
                }
                return object.remove("result").ok_or(Failure::Protocol);
            }
        })
        .await
        .map_err(|_| Failure::Timeout)?
    }
}

async fn read_account<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: R,
    writer: W,
    auth: &CodexAuth,
) -> Result<(), Failure> {
    let mut client = Client::new(reader, writer);
    let initialized = client.request(1, ReadRequest::Initialize, json!({
        "clientInfo": {"name": "intendant-dots-routing-diagnostic", "version": env!("CARGO_PKG_VERSION")},
        "capabilities": {"experimentalApi": true}
    })).await?;
    if initialized
        .get("userAgent")
        .and_then(Value::as_str)
        .is_none()
    {
        return Err(Failure::Protocol);
    }
    tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.write(json!({"method": "initialized"})),
    )
    .await
    .map_err(|_| Failure::Timeout)??;
    let account = client
        .request(2, ReadRequest::Account, json!({"refreshToken": false}))
        .await?;
    validate_account(&account, auth)?;
    let requirements = client
        .request(3, ReadRequest::Requirements, json!({}))
        .await?;
    validate_requirements(&requirements)
}

fn validate_account(account: &Value, auth: &CodexAuth) -> Result<(), Failure> {
    if account.pointer("/account/type").and_then(Value::as_str) != Some("chatgpt") {
        return Err(Failure::SubscriptionUnavailable);
    }
    let routing = account
        .get("workspaceRouting")
        .and_then(Value::as_object)
        .ok_or(Failure::UnsupportedRouting)?;
    let selected = routing
        .get("chatgptAccountId")
        .and_then(Value::as_str)
        .ok_or(Failure::Protocol)?;
    if !auth.matches_account(selected) {
        return Err(Failure::AccountMismatch);
    }
    // Never forward a bearer to an arbitrary returned origin or guess a
    // residency route. This slice validates only the researched normal route.
    if routing.get("backendOrigin").and_then(Value::as_str) != Some("https://chatgpt.com")
        || routing
            .get("accountRoutingOverride")
            .and_then(Value::as_str)
            != Some("NO_CONSTRAINT")
    {
        return Err(Failure::UnsupportedRouting);
    }
    Ok(())
}

fn validate_requirements(response: &Value) -> Result<(), Failure> {
    match response.get("requirements") {
        Some(Value::Null) => Ok(()),
        Some(Value::Object(requirements)) => {
            if requirements
                .get("application")
                .is_some_and(|value| !value.is_null() && !value.is_object())
            {
                return Err(Failure::Protocol);
            }
            // Managed routing/network requirements need their own reviewed
            // enforcement, not a diagnostic that silently disregards them.
            if ["chatgptBaseUrl", "enforceResidency"]
                .iter()
                .any(|key| requirements.get(*key).is_some_and(|v| !v.is_null()))
                || requirements
                    .get("application")
                    .and_then(|v| v.get("network"))
                    .is_some_and(|v| !v.is_null())
            {
                return Err(Failure::UnsupportedRouting);
            }
            Ok(())
        }
        _ => Err(Failure::Protocol),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No real home or process environment is read by these tests.
    fn account_fixture() -> Value {
        json!({"account": {"type": "chatgpt"}, "workspaceRouting": {
            "chatgptAccountId": "fixture-account", "backendOrigin": "https://chatgpt.com",
            "accountRoutingOverride": "NO_CONSTRAINT"
        }})
    }

    #[test]
    fn routing_binds_selected_account_and_refuses_other_origins_or_requirements() {
        let auth =
            crate::codex_cloud::fixture_subscription_auth("fixture-token", "fixture-account");
        let mut account = account_fixture();
        assert_eq!(validate_account(&account, &auth), Ok(()));
        account["workspaceRouting"]["chatgptAccountId"] = json!("different-account");
        assert_eq!(
            validate_account(&account, &auth),
            Err(Failure::AccountMismatch)
        );
        account = account_fixture();
        for origin in [
            "https://other.invalid",
            "https://chatgpt.com/",
            "http://chatgpt.com",
            "https://chatgpt.com@other.invalid",
        ] {
            account["workspaceRouting"]["backendOrigin"] = json!(origin);
            assert_eq!(
                validate_account(&account, &auth),
                Err(Failure::UnsupportedRouting)
            );
        }
        account = account_fixture();
        account["workspaceRouting"]["accountRoutingOverride"] = json!("us");
        assert_eq!(
            validate_account(&account, &auth),
            Err(Failure::UnsupportedRouting)
        );
        assert_eq!(
            validate_requirements(&json!({"requirements": null})),
            Ok(())
        );
        assert_eq!(
            validate_requirements(&json!({"requirements": {"allowedSandboxModes": ["read-only"]}})),
            Ok(())
        );
        for requirements in [
            json!({"chatgptBaseUrl": "https://other.invalid"}),
            json!({"enforceResidency": "us"}),
            json!({"application": {"network": {"enabled": true}}}),
        ] {
            assert_eq!(
                validate_requirements(&json!({"requirements": requirements})),
                Err(Failure::UnsupportedRouting)
            );
        }
        assert_eq!(validate_requirements(&json!({})), Err(Failure::Protocol));
        assert_eq!(
            validate_requirements(&json!({"requirements": {"application": false}})),
            Err(Failure::Protocol)
        );
    }

    #[tokio::test]
    async fn routing_pipe_sends_only_closed_read_vocabulary() {
        let auth =
            crate::codex_cloud::fixture_subscription_auth("fixture-token", "fixture-account");
        let (client, server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(client);
        let fixture = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server);
            let mut reader = BufReader::new(reader);
            for (id, method, result) in [
                (1, "initialize", json!({"userAgent": "fixture"})),
                (2, "account/read", account_fixture()),
                (3, "configRequirements/read", json!({"requirements": null})),
            ] {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["id"], id);
                assert_eq!(request["method"], method);
                if id == 2 {
                    assert_eq!(request["params"]["refreshToken"], false);
                }
                writer
                    .write_all(format!("{}\n", json!({"id": id, "result": result})).as_bytes())
                    .await
                    .unwrap();
                if id == 1 {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    assert_eq!(
                        serde_json::from_str::<Value>(&line).unwrap(),
                        json!({"method": "initialized"})
                    );
                }
            }
        });
        assert_eq!(read_account(reader, writer, &auth).await, Ok(()));
        fixture.await.unwrap();
    }

    #[tokio::test]
    async fn routing_pipe_refuses_actions_drift_and_unbounded_output_without_reflection() {
        for response in [
            json!({"id": 1, "method": "item/tool/call", "params": {"secret": "provider-secret"}}),
            json!({"id": 99, "result": {"secret": "provider-secret"}}),
            json!({"id": 1, "error": {"message": "provider-secret"}}),
            json!({"method": "account/updated", "params": {"secret": "provider-secret"}}),
        ] {
            let bytes = format!("{response}\n").into_bytes();
            let mut client = Client::new(bytes.as_slice(), tokio::io::sink());
            let error = client
                .request(1, ReadRequest::Initialize, json!({}))
                .await
                .unwrap_err();
            assert!(!format!("{error:?}").contains("provider-secret"));
        }
        let oversized = vec![b'x'; MAX_OUTPUT_BYTES + 1];
        let mut client = Client::new(oversized.as_slice(), tokio::io::sink());
        assert_eq!(
            client.request(1, ReadRequest::Initialize, json!({})).await,
            Err(Failure::Protocol)
        );
        let notifications =
            format!("{}\n", json!({"method": "fixture"})).repeat(MAX_NOTIFICATIONS + 1);
        let mut client = Client::new(notifications.as_bytes(), tokio::io::sink());
        assert_eq!(
            client.request(1, ReadRequest::Initialize, json!({})).await,
            Err(Failure::Protocol)
        );
    }

    #[test]
    fn routing_child_policy_has_no_inherited_tool_or_provider_authority() {
        assert!(SERVER_ARGS
            .windows(2)
            .any(|v| v == ["-c", "features.plugins=false"]));
        assert!(SERVER_ARGS
            .windows(2)
            .any(|v| v == ["-c", "mcp_servers={}"]));
        assert!(SERVER_ARGS
            .windows(2)
            .any(|v| v == ["-c", "approval_policy=\"never\""]));
        assert!(SERVER_ARGS
            .windows(2)
            .any(|v| v == ["-c", "sandbox_mode=\"read-only\""]));
        assert!(!CHILD_ENV.iter().any(|name| name.starts_with("INTENDANT")
            || name.ends_with("API_KEY")
            || *name == "CODEX_API_BASE_URL"));
    }
}
