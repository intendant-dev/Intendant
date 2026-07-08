use base64::Engine;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use intendant_core::net::DEFAULT_GATEWAY_PORT as DEFAULT_PORT;

#[derive(Debug, Clone)]
struct Config {
    base_url: String,
    session_id: Option<String>,
    managed_context: Option<String>,
    raw: bool,
    json: bool,
    /// `--peer` target as the user typed it. `Some` routes every request to
    /// that federated peer's `/mcp` with fail-closed peer semantics (see
    /// `rpc` / `mcp_url`); also the name echoed in peer-mode errors.
    peer: Option<String>,
    /// Outbound `Authorization: Bearer` for the resolved peer (its
    /// `[[peer]] bearer_token`); sent only in peer mode.
    bearer: Option<String>,
}

#[derive(Debug)]
struct CommandArgs {
    positional: Vec<String>,
    values: BTreeMap<String, Vec<String>>,
    bools: BTreeSet<String>,
}

pub async fn run(raw_args: Vec<String>) -> Result<(), String> {
    let (config, command) = parse_global_args(raw_args)?;
    let (mut config, command) = parse_output_flags(config, command);
    if command.is_empty() {
        print_help();
        return Ok(());
    }
    if matches!(command[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(());
    }

    let client = match config.peer.clone() {
        Some(needle) => configure_peer_mode(&mut config, &needle)?,
        None => reqwest::Client::new(),
    };
    match command[0].as_str() {
        "status" => {
            ensure_help(&command[1..], help_status)?;
            let response =
                call_tool(&client, &config, "get_status", Value::Object(Map::new())).await?;
            print_tool_response(response, &config, None)?;
        }
        "logs" => run_logs(&client, &config, &command[1..]).await?,
        "tools" | "tool" => run_tools(&client, &config, &command[1..]).await?,
        "display" => run_display(&client, &config, &command[1..]).await?,
        "browser" | "browsers" => run_browser(&client, &config, &command[1..]).await?,
        "cu" => run_cu(&client, &config, &command[1..]).await?,
        "shared" | "shared-view" => run_shared(&client, &config, &command[1..]).await?,
        "approval" | "approvals" => run_approval(&client, &config, &command[1..]).await?,
        "input" => run_input(&client, &config, &command[1..]).await?,
        "settings" | "set" => run_settings(&client, &config, &command[1..]).await?,
        "task" => run_task(&client, &config, &command[1..]).await?,
        "controller" => run_controller(&client, &config, &command[1..]).await?,
        "context" => run_context(&client, &config, &command[1..]).await?,
        "audio" => run_audio(&client, &config, &command[1..]).await?,
        "peer" | "peers" => run_peer(&client, &config, &command[1..]).await?,
        other => {
            return Err(format!(
                "unknown command '{other}'. Run `intendant ctl --help`."
            ));
        }
    }
    Ok(())
}

fn parse_global_args(mut raw: Vec<String>) -> Result<(Config, Vec<String>), String> {
    let mut base_url = std::env::var("INTENDANT_MCP_URL").unwrap_or_default();
    let mut port = std::env::var("INTENDANT_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let mut session_id = std::env::var("INTENDANT_SESSION_ID").ok();
    let mut managed_context = std::env::var("INTENDANT_MANAGED_CONTEXT").ok();
    let mut raw_output = false;
    let mut json_output = false;
    let mut peer: Option<String> = None;
    let mut url_flag_given = false;
    let mut command_start = 0;

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--url" => {
                i += 1;
                url_flag_given = true;
                base_url = raw
                    .get(i)
                    .cloned()
                    .ok_or_else(|| "--url requires a value".to_string())?;
            }
            "--peer" => {
                i += 1;
                peer = Some(
                    raw.get(i)
                        .cloned()
                        .ok_or_else(|| "--peer requires a value".to_string())?,
                );
            }
            "--port" => {
                i += 1;
                let value = raw
                    .get(i)
                    .ok_or_else(|| "--port requires a value".to_string())?;
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port value '{value}'"))?;
            }
            "--session" | "--session-id" => {
                i += 1;
                session_id = Some(
                    raw.get(i)
                        .cloned()
                        .ok_or_else(|| "--session requires a value".to_string())?,
                );
            }
            "--managed-context" => {
                i += 1;
                managed_context = Some(
                    raw.get(i)
                        .cloned()
                        .ok_or_else(|| "--managed-context requires a value".to_string())?,
                );
            }
            "--raw" => raw_output = true,
            "--json" => json_output = true,
            arg if arg.starts_with("--url=") => {
                url_flag_given = true;
                base_url = arg.trim_start_matches("--url=").to_string();
            }
            arg if arg.starts_with("--peer=") => {
                peer = Some(arg.trim_start_matches("--peer=").to_string());
            }
            arg if arg.starts_with("--port=") => {
                let value = arg.trim_start_matches("--port=");
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port value '{value}'"))?;
            }
            arg if arg.starts_with("--session=") => {
                session_id = Some(arg.trim_start_matches("--session=").to_string());
            }
            arg if arg.starts_with("--session-id=") => {
                session_id = Some(arg.trim_start_matches("--session-id=").to_string());
            }
            arg if arg.starts_with("--managed-context=") => {
                managed_context = Some(arg.trim_start_matches("--managed-context=").to_string());
            }
            _ => {
                command_start = i;
                break;
            }
        }
        i += 1;
        command_start = i;
    }

    let command = raw.split_off(command_start);
    // --peer replaces the whole URL derivation (flag- or env-provided) with
    // the peer's /mcp endpoint; only the explicit --url flag is a conflict —
    // INTENDANT_MCP_URL / INTENDANT_PORT are silently overridden.
    if peer.is_some() && url_flag_given {
        return Err("--peer and --url are mutually exclusive".to_string());
    }
    let peer = match peer.map(|value| value.trim().to_string()) {
        Some(value) if value.is_empty() => {
            return Err("--peer requires a non-empty value".to_string());
        }
        other => other,
    };
    let base_url = if base_url.trim().is_empty() {
        format!("http://localhost:{port}/mcp")
    } else {
        base_url
    };

    Ok((
        Config {
            base_url,
            session_id: clean_opt(session_id),
            managed_context: clean_opt(managed_context),
            raw: raw_output,
            json: json_output,
            peer,
            bearer: None,
        },
        command,
    ))
}

fn parse_output_flags(mut config: Config, raw: Vec<String>) -> (Config, Vec<String>) {
    let mut command = Vec::with_capacity(raw.len());
    for arg in raw {
        match arg.as_str() {
            "--raw" => config.raw = true,
            "--json" => config.json = true,
            _ => command.push(arg),
        }
    }
    (config, command)
}

fn clean_opt(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve `--peer <needle>` against the `[[peer]]` configuration — project
/// first, user-level fallback second — and switch the invocation to peer
/// routing: point `base_url` at the peer's stateless JSON-RPC `/mcp`
/// endpoint, adopt the peer's outbound bearer, and build the mTLS-capable
/// HTTP client whose client certificate binds the principal the peer's IAM
/// profile authorizes.
fn configure_peer_mode(config: &mut Config, needle: &str) -> Result<reqwest::Client, String> {
    let project = crate::project::Project::detect()
        .map_err(|e| format!("--peer could not load the project configuration: {e}"))?;
    let peer = resolve_peer_with_user_fallback(
        &project.config.peers,
        &project.root.join("intendant.toml"),
        &user_peers_file(),
        needle,
    )?;
    config.base_url = peer_mcp_endpoint(&peer.card_url)?;
    config.bearer = peer.bearer_token.clone();

    let mut pins = Vec::with_capacity(peer.pinned_fingerprints.len());
    for raw in &peer.pinned_fingerprints {
        let fp = crate::peer::transport::pinning::parse_fingerprint(raw)
            .map_err(|e| format!("peer '{needle}': invalid pinned fingerprint {raw:?}: {e}"))?;
        pins.push(fp);
    }
    // Explicit [[peer]] client_cert/client_key wins (same pairing rule the
    // daemon applies at peer boot); otherwise fall back to the installed
    // access client identity for TLS peers.
    let identity = crate::startup::peer_boot::peer_client_identity_from_config(&peer)
        .map_err(|e| format!("peer '{needle}': {e}"))?
        .or_else(|| {
            if crate::peer::transport::tls_client::url_uses_tls(&config.base_url) {
                crate::peer::transport::tls_client::installed_access_client_identity_paths()
            } else {
                None
            }
        });
    // 120s: `cu actions` batches can legitimately carry long Wait actions.
    crate::peer::transport::tls_client::reqwest_client(
        std::time::Duration::from_secs(120),
        &pins,
        identity.as_ref(),
    )
    .map_err(|e| format!("peer '{needle}': failed to build TLS client: {e}"))
}

/// Two-layer `--peer` resolution: the project's `[[peer]]` entries first
/// (unchanged behavior), then — only when the project yields ZERO matches,
/// including when there is no project config at all — the user-level peers
/// file. Both layers use the same matching rules ([`peer_matches`]). Because
/// the project layer wins outright whenever it matches at all, an ambiguous
/// match ACROSS the two layers cannot arise by construction; ambiguity
/// within a single layer stays an error.
///
/// SCOPE GUARD: the user-level peers file is a `ctl --peer` RESOLUTION
/// fallback ONLY. Daemon startup (`startup/peer_boot.rs`) must keep
/// federating from the project config alone — a daemon that auto-federates
/// with every peer in a machine-global file would be a semantic change
/// nobody asked for.
fn resolve_peer_with_user_fallback(
    project_peers: &[crate::project::PeerConfig],
    project_config_path: &Path,
    user_peers_path: &Path,
    needle: &str,
) -> Result<crate::project::PeerConfig, String> {
    if project_peers.iter().any(|peer| peer_matches(peer, needle)) {
        return resolve_peer(project_peers, needle).cloned();
    }
    let user_peers = load_user_peers(user_peers_path)?;
    if user_peers.iter().any(|peer| peer_matches(peer, needle)) {
        return resolve_peer(&user_peers, needle).cloned();
    }
    Err(format!(
        "no configured peer matches '{needle}'; searched {} and {}",
        describe_peer_source(project_config_path, project_peers),
        describe_peer_source(user_peers_path, &user_peers),
    ))
}

/// The user-level peers file: `[[peer]]` entries in the same shape as the
/// project config's, at `<state root>/peers.toml` — `~/.intendant/peers.toml`
/// by default, relocated by `$INTENDANT_HOME` (which is also what keeps
/// hermetic harnesses away from the real user's file). Peers are
/// machine-scoped identities — their `client_cert`/`client_key` paths are
/// absolute, like the access certs already under the state root — not
/// project state, so a peer recorded here is reachable from any working
/// directory.
fn user_peers_file() -> PathBuf {
    crate::platform::intendant_home().join("peers.toml")
}

/// Load `[[peer]]` entries from the user-level peers file. A missing file
/// is simply "no user-level peers" (Ok, empty); an unreadable or
/// unparseable file the user did write fails loud instead of being
/// silently skipped.
fn load_user_peers(path: &Path) -> Result<Vec<crate::project::PeerConfig>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read user-level peers file {}: {e}", path.display()))?;
    #[derive(serde::Deserialize)]
    struct UserPeersFile {
        #[serde(default, rename = "peer")]
        peers: Vec<crate::project::PeerConfig>,
    }
    let parsed: UserPeersFile = toml::from_str(&content).map_err(|e| {
        format!(
            "failed to parse user-level peers file {}: {e}",
            path.display()
        )
    })?;
    Ok(parsed.peers)
}

/// Pick the `[[peer]]` entry `--peer <needle>` refers to. A peer matches when
/// the needle equals its `label` (case-insensitive), the host of its
/// `card_url`, or its `card_url` exactly; a needle containing ':' also
/// matches on the segment after the LAST ':' — peer ids look like
/// "intendant:nicks-mac", so the suffix is compared against label/host. (The
/// needle is the side that gets split, never the card_url host, since URLs
/// carry ':' for ports.)
fn resolve_peer<'a>(
    peers: &'a [crate::project::PeerConfig],
    needle: &str,
) -> Result<&'a crate::project::PeerConfig, String> {
    let matches: Vec<&crate::project::PeerConfig> = peers
        .iter()
        .filter(|peer| peer_matches(peer, needle))
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(format!(
            "no configured peer matches '{needle}'; configured peers: {}",
            peers
                .iter()
                .map(describe_peer)
                .collect::<Vec<_>>()
                .join(", ")
        )),
        _ => Err(format!(
            "--peer '{needle}' is ambiguous; it matches: {}",
            matches
                .iter()
                .map(|peer| describe_peer(peer))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn peer_matches(peer: &crate::project::PeerConfig, needle: &str) -> bool {
    if needle == peer.card_url || label_or_host_matches(peer, needle) {
        return true;
    }
    match needle.rsplit_once(':') {
        Some((_, suffix)) => label_or_host_matches(peer, suffix),
        None => false,
    }
}

fn label_or_host_matches(peer: &crate::project::PeerConfig, needle: &str) -> bool {
    if peer
        .label
        .as_deref()
        .is_some_and(|label| label.eq_ignore_ascii_case(needle))
    {
        return true;
    }
    card_url_host(peer).is_some_and(|host| host.eq_ignore_ascii_case(needle))
}

fn card_url_host(peer: &crate::project::PeerConfig) -> Option<String> {
    reqwest::Url::parse(&peer.card_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

/// Render a configured peer for "which peers exist" error listings:
/// `label (host)` when both are known, else whichever exists, else card_url.
fn describe_peer(peer: &crate::project::PeerConfig) -> String {
    let host = card_url_host(peer);
    match (peer.label.as_deref(), host) {
        (Some(label), Some(host)) => format!("{label} ({host})"),
        (Some(label), None) => label.to_string(),
        (None, Some(host)) => host,
        (None, None) => peer.card_url.clone(),
    }
}

/// Render one resolution layer for the no-match error: its path plus either
/// the peers it configures or why it contributed none — so the error names
/// both locations `--peer` searched and where a fix belongs.
fn describe_peer_source(path: &Path, peers: &[crate::project::PeerConfig]) -> String {
    if peers.is_empty() {
        let why = if path.exists() {
            "no [[peer]] entries"
        } else {
            "not found"
        };
        format!("{} ({why})", path.display())
    } else {
        format!(
            "{} (peers: {})",
            path.display(),
            peers
                .iter()
                .map(describe_peer)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Derive the peer gateway's stateless JSON-RPC `/mcp` endpoint from its
/// Agent Card URL: keep scheme/host/port, drop path and query. The card is
/// served at `<gateway>/.well-known/agent-card.json`, so the card_url origin
/// IS the gateway origin that serves `/mcp`. (`via_urls` only override the
/// `/ws` federation transport, not HTTP RPC.)
fn peer_mcp_endpoint(card_url: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(card_url)
        .map_err(|e| format!("invalid peer card_url '{card_url}': {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "peer card_url '{card_url}' must be http(s) to derive the /mcp endpoint"
        ));
    }
    Ok(format!("{}/mcp", url.origin().ascii_serialization()))
}

fn parse_command_args(
    raw: &[String],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> Result<CommandArgs, String> {
    let value_flags: BTreeSet<&str> = value_flags.iter().copied().collect();
    let bool_flags: BTreeSet<&str> = bool_flags.iter().copied().collect();
    let mut positional = Vec::new();
    let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut bools = BTreeSet::new();
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        if arg == "--" {
            positional.extend(raw[i + 1..].iter().cloned());
            break;
        }
        if let Some((flag, value)) = arg.split_once('=') {
            if flag.starts_with("--") && value_flags.contains(flag) {
                values
                    .entry(flag.to_string())
                    .or_default()
                    .push(value.to_string());
            } else if flag.starts_with("--") && bool_flags.contains(flag) {
                return Err(format!("{flag} does not take a value"));
            } else {
                positional.push(arg.clone());
            }
        } else if arg.starts_with("--") && value_flags.contains(arg.as_str()) {
            i += 1;
            let value = raw
                .get(i)
                .cloned()
                .ok_or_else(|| format!("{arg} requires a value"))?;
            values.entry(arg.clone()).or_default().push(value);
        } else if arg.starts_with("--") {
            if !bool_flags.contains(arg.as_str()) {
                return Err(format!("unknown flag {arg}"));
            }
            bools.insert(arg.clone());
        } else {
            positional.push(arg.clone());
        }
        i += 1;
    }
    Ok(CommandArgs {
        positional,
        values,
        bools,
    })
}

impl CommandArgs {
    fn one(&self, flag: &str) -> Option<&str> {
        self.values
            .get(flag)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    fn all(&self, flag: &str) -> impl Iterator<Item = &str> {
        self.values
            .get(flag)
            .into_iter()
            .flat_map(|v| v.iter().map(String::as_str))
    }

    fn has(&self, flag: &str) -> bool {
        self.bools.contains(flag)
    }
}

async fn run_logs(client: &reqwest::Client, config: &Config, raw: &[String]) -> Result<(), String> {
    ensure_help(raw, help_logs)?;
    let args = parse_command_args(raw, &["--since-id", "--level", "--limit"], &[])?;
    let mut map = Map::new();
    insert_u64(&mut map, "since_id", args.one("--since-id"))?;
    insert_string(&mut map, "level_filter", args.one("--level"));
    insert_usize(&mut map, "limit", args.one("--limit"))?;
    let response = call_tool(client, config, "get_logs", Value::Object(map)).await?;
    print_tool_response(response, config, None)
}

async fn run_tools(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_tools();
        return Ok(());
    }
    match raw[0].as_str() {
        "list" => {
            ensure_help(&raw[1..], help_tools_list)?;
            let response = rpc(client, config, "tools/list", Value::Object(Map::new())).await?;
            if config.raw || config.json {
                print_json(&response)?;
            } else {
                let tools = response
                    .pointer("/result/tools")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "tools/list response missing result.tools".to_string())?;
                for tool in tools {
                    let name = tool
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("<unnamed>");
                    let description = tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .replace('\n', " ");
                    if description.is_empty() {
                        println!("{name}");
                    } else {
                        println!("{name}\t{description}");
                    }
                }
            }
        }
        "schema" | "help" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let name = args
                .positional
                .first()
                .ok_or_else(|| "tools schema requires a tool name".to_string())?;
            let response = rpc(client, config, "tools/list", Value::Object(Map::new())).await?;
            let tools = response
                .pointer("/result/tools")
                .and_then(Value::as_array)
                .ok_or_else(|| "tools/list response missing result.tools".to_string())?;
            let tool = tools
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name.as_str()))
                .ok_or_else(|| format!("tool '{name}' is not advertised by this MCP endpoint"))?;
            print_json(tool)?;
        }
        "call" => {
            ensure_help(&raw[1..], help_tools_call)?;
            let args = parse_command_args(&raw[1..], &["--args", "--arg"], &[])?;
            let name = args
                .positional
                .first()
                .ok_or_else(|| "tools call requires a tool name".to_string())?;
            let arguments = tool_arguments_from_flags(&args)?;
            let response = call_tool(client, config, name, arguments).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown tools command '{other}'")),
    }
    Ok(())
}

async fn run_display(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_display();
        return Ok(());
    }
    match raw[0].as_str() {
        "list" => {
            let response =
                call_tool(client, config, "list_displays", Value::Object(Map::new())).await?;
            print_tool_response(response, config, None)?;
        }
        "frames" => {
            let args = parse_command_args(&raw[1..], &["--stream", "--count"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "stream", args.one("--stream"));
            insert_usize(&mut map, "count", args.one("--count"))?;
            let response = call_tool(client, config, "list_frames", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "read-frame" | "frame" => {
            let args = parse_command_args(&raw[1..], &["--stream"], &[])?;
            let frame_id = args
                .positional
                .first()
                .cloned()
                .unwrap_or_else(|| "latest".to_string());
            let mut map = Map::new();
            map.insert("frame_id".to_string(), Value::String(frame_id));
            insert_string(&mut map, "stream", args.one("--stream"));
            let response = call_tool(client, config, "read_frame", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "screenshot" => {
            ensure_help(&raw[1..], help_display_screenshot)?;
            let args = parse_command_args(&raw[1..], &["--target", "--output"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "display_target", args.one("--target"));
            let response = call_tool(client, config, "take_screenshot", Value::Object(map)).await?;
            print_tool_response(response, config, output_path(args.one("--output")))?;
        }
        "take" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let id = positional_u32(&args, 0, "display take requires a display id")?;
            let mut map = Map::new();
            map.insert("display_id".to_string(), Value::from(id));
            let response = call_tool(client, config, "take_display", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "release" => {
            let args = parse_command_args(&raw[1..], &["--note"], &[])?;
            let id = positional_u32(&args, 0, "display release requires a display id")?;
            let mut map = Map::new();
            map.insert("display_id".to_string(), Value::from(id));
            insert_string(&mut map, "note", args.one("--note"));
            let response = call_tool(client, config, "release_display", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "grant-user" | "grant_user" | "grant-user-display" | "grant_user_display" => {
            let args = parse_command_args(&raw[1..], &["--display-id"], &[])?;
            let mut map = Map::new();
            if let Some(id) = args.one("--display-id") {
                insert_u32(&mut map, "display_id", Some(id))?;
            } else if !args.positional.is_empty() {
                let id = positional_u32(&args, 0, "display grant-user requires a display id")?;
                map.insert("display_id".to_string(), Value::from(id));
            }
            let response =
                call_tool(client, config, "grant_user_display", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "revoke-user" | "revoke_user" | "revoke-user-display" | "revoke_user_display" => {
            let args = parse_command_args(&raw[1..], &["--display-id", "--note"], &[])?;
            let mut map = Map::new();
            if let Some(id) = args.one("--display-id") {
                insert_u32(&mut map, "display_id", Some(id))?;
            } else if !args.positional.is_empty() {
                let id = positional_u32(&args, 0, "display revoke-user requires a display id")?;
                map.insert("display_id".to_string(), Value::from(id));
            }
            insert_string(&mut map, "note", args.one("--note"));
            let response =
                call_tool(client, config, "revoke_user_display", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown display command '{other}'")),
    }
    Ok(())
}

async fn run_browser(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_browser();
        return Ok(());
    }
    match raw[0].as_str() {
        "providers" => {
            let response = call_tool(
                client,
                config,
                "browser_workspace_providers",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "list" | "ls" => {
            let response = call_tool(
                client,
                config,
                "list_browser_workspaces",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "create" | "open" => {
            let args = parse_command_args(
                &raw[1..],
                &[
                    "--url",
                    "--label",
                    "--provider",
                    "--peer",
                    "--session",
                    "--profile-dir",
                ],
                &[],
            )?;
            let mut map = Map::new();
            let url = args
                .one("--url")
                .or_else(|| args.positional.first().map(String::as_str));
            insert_string(&mut map, "url", url);
            insert_string(&mut map, "label", args.one("--label"));
            insert_string(&mut map, "provider", args.one("--provider"));
            insert_string(&mut map, "peer_id", args.one("--peer"));
            insert_string(&mut map, "owner_session_id", args.one("--session"));
            insert_string(&mut map, "profile_dir", args.one("--profile-dir"));
            let response = call_tool(
                client,
                config,
                "create_browser_workspace",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "close" => {
            let args = parse_command_args(&raw[1..], &["--reason"], &[])?;
            let id = args
                .positional
                .first()
                .ok_or_else(|| "browser close requires a workspace id".to_string())?;
            let mut map = Map::new();
            map.insert("workspace_id".to_string(), Value::String(id.clone()));
            insert_string(&mut map, "reason", args.one("--reason"));
            let response = call_tool(
                client,
                config,
                "close_browser_workspace",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "acquire" | "take" => {
            let args = parse_command_args(
                &raw[1..],
                &["--holder", "--holder-kind", "--note"],
                &["--force"],
            )?;
            let id = args
                .positional
                .first()
                .ok_or_else(|| "browser acquire requires a workspace id".to_string())?;
            let holder = args
                .one("--holder")
                .or(config.session_id.as_deref())
                .unwrap_or("intendant-ctl");
            let mut map = Map::new();
            map.insert("workspace_id".to_string(), Value::String(id.clone()));
            map.insert("holder_id".to_string(), Value::String(holder.to_string()));
            insert_string(&mut map, "holder_kind", args.one("--holder-kind"));
            insert_string(&mut map, "note", args.one("--note"));
            if args.has("--force") {
                map.insert("force".to_string(), Value::Bool(true));
            }
            let response = call_tool(
                client,
                config,
                "acquire_browser_workspace",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "release" => {
            let args = parse_command_args(&raw[1..], &["--holder", "--note"], &[])?;
            let id = args
                .positional
                .first()
                .ok_or_else(|| "browser release requires a workspace id".to_string())?;
            let mut map = Map::new();
            map.insert("workspace_id".to_string(), Value::String(id.clone()));
            insert_string(&mut map, "holder_id", args.one("--holder"));
            insert_string(&mut map, "note", args.one("--note"));
            let response = call_tool(
                client,
                config,
                "release_browser_workspace",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown browser command '{other}'")),
    }
    Ok(())
}

async fn run_cu(client: &reqwest::Client, config: &Config, raw: &[String]) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_cu();
        return Ok(());
    }
    match raw[0].as_str() {
        "actions" | "exec" => {
            ensure_help(&raw[1..], help_cu_actions)?;
            let args = parse_command_args(
                &raw[1..],
                &["--actions", "--target", "--coordinate-space", "--output"],
                &[],
            )?;
            let actions = args
                .one("--actions")
                .ok_or_else(|| "cu actions requires --actions JSON".to_string())
                .and_then(read_json_value)?;
            validate_cu_actions(&actions)?;
            let mut map = Map::new();
            map.insert("actions".to_string(), actions);
            insert_string(&mut map, "display_target", args.one("--target"));
            insert_string(&mut map, "coordinate_space", args.one("--coordinate-space"));
            let response =
                call_tool(client, config, "execute_cu_actions", Value::Object(map)).await?;
            print_tool_response(response, config, output_path(args.one("--output")))?;
        }
        "screenshot" => {
            let next = std::iter::once("screenshot".to_string())
                .chain(raw[1..].iter().cloned())
                .collect::<Vec<_>>();
            run_display(client, config, &next).await?;
        }
        "elements" | "read-screen" => {
            let args = parse_command_args(&raw[1..], &["--target", "--format"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "display_target", args.one("--target"));
            insert_string(&mut map, "format", args.one("--format"));
            let response = call_tool(client, config, "read_screen", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown cu command '{other}'")),
    }
    Ok(())
}

async fn run_shared(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_shared();
        return Ok(());
    }
    match raw[0].as_str() {
        "show" => {
            let args = parse_command_args(
                &raw[1..],
                &["--target", "--display-id", "--reason", "--focus"],
                &[],
            )?;
            let mut map = shared_target_map(&args)?;
            insert_string(&mut map, "reason", args.one("--reason"));
            if let Some(region) = args.one("--focus") {
                map.insert("focus_region".to_string(), parse_region(region)?);
            }
            let response =
                call_tool(client, config, "show_shared_view", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "focus" => {
            ensure_help(&raw[1..], help_shared_focus)?;
            let args = parse_command_args(
                &raw[1..],
                &["--target", "--display-id", "--region", "--note"],
                &[],
            )?;
            let mut map = shared_target_map(&args)?;
            let region = args
                .one("--region")
                .or_else(|| args.positional.first().map(String::as_str))
                .ok_or_else(|| "shared focus requires --region x,y,width,height".to_string())?;
            map.insert("region".to_string(), parse_region(region)?);
            insert_string(&mut map, "note", args.one("--note"));
            let response =
                call_tool(client, config, "focus_shared_view", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "input" | "request-input" => {
            let args =
                parse_command_args(&raw[1..], &["--target", "--display-id", "--reason"], &[])?;
            let mut map = shared_target_map(&args)?;
            insert_string(&mut map, "reason", args.one("--reason"));
            let response = call_tool(
                client,
                config,
                "request_shared_view_input",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "hide" => {
            let args = parse_command_args(&raw[1..], &["--reason"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "reason", args.one("--reason"));
            let response =
                call_tool(client, config, "hide_shared_view", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "capture" => {
            let args = parse_command_args(
                &raw[1..],
                &["--target", "--display-id", "--reason", "--output"],
                &[],
            )?;
            let mut map = shared_target_map(&args)?;
            insert_string(&mut map, "reason", args.one("--reason"));
            let response = call_tool(
                client,
                config,
                "capture_shared_view_frame",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, output_path(args.one("--output")))?;
        }
        other => return Err(format!("unknown shared command '{other}'")),
    }
    Ok(())
}

async fn run_approval(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_approval();
        return Ok(());
    }
    match raw[0].as_str() {
        "pending" => {
            let response = call_tool(
                client,
                config,
                "get_pending_approval",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "approve" | "deny" | "skip" | "approve-all" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let id = positional_u64(&args, 0, "approval action requires an id")?;
            let tool = match raw[0].as_str() {
                "approve" => "approve",
                "deny" => "deny",
                "skip" => "skip",
                "approve-all" => "approve_all",
                _ => unreachable!(),
            };
            let mut map = Map::new();
            map.insert("id".to_string(), Value::from(id));
            let response = call_tool(client, config, tool, Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown approval command '{other}'")),
    }
    Ok(())
}

async fn run_input(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_input();
        return Ok(());
    }
    match raw[0].as_str() {
        "pending" => {
            let response = call_tool(
                client,
                config,
                "get_pending_input",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "respond" => {
            let args = parse_command_args(&raw[1..], &["--text"], &[])?;
            let text = args
                .one("--text")
                .map(str::to_string)
                .or_else(|| {
                    if args.positional.is_empty() {
                        None
                    } else {
                        Some(args.positional.join(" "))
                    }
                })
                .ok_or_else(|| "input respond requires text".to_string())?;
            let mut map = Map::new();
            map.insert("text".to_string(), Value::String(text));
            let response = call_tool(client, config, "respond", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown input command '{other}'")),
    }
    Ok(())
}

async fn run_settings(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_settings();
        return Ok(());
    }
    match raw[0].as_str() {
        "autonomy" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let level = args
                .positional
                .first()
                .ok_or_else(|| "settings autonomy requires a level".to_string())?;
            let response = call_tool(
                client,
                config,
                "set_autonomy",
                json_object([("level", Value::String(level.clone()))]),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "verbosity" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let level = args
                .positional
                .first()
                .ok_or_else(|| "settings verbosity requires a level".to_string())?;
            let response = call_tool(
                client,
                config,
                "set_verbosity",
                json_object([("level", Value::String(level.clone()))]),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown settings command '{other}'")),
    }
    Ok(())
}

async fn run_task(client: &reqwest::Client, config: &Config, raw: &[String]) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_task();
        return Ok(());
    }
    match raw[0].as_str() {
        "start" => {
            let response =
                call_tool(client, config, "start_task", task_start_args(&raw[1..])?).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown task command '{other}'")),
    }
    Ok(())
}

fn task_start_args(raw: &[String]) -> Result<Value, String> {
    let args = parse_command_args(
        raw,
        &[
            "--task",
            "--session",
            "--session-id",
            "--display-target",
            "--frame",
        ],
        &["--orchestrate", "--direct"],
    )?;
    let task = args
        .one("--task")
        .map(str::to_string)
        .or_else(|| {
            if args.positional.is_empty() {
                None
            } else {
                Some(args.positional.join(" "))
            }
        })
        .ok_or_else(|| "task start requires a task".to_string())?;
    let mut map = Map::new();
    map.insert("task".to_string(), Value::String(task));
    insert_string(
        &mut map,
        "session_id",
        args.one("--session").or_else(|| args.one("--session-id")),
    );
    if args.has("--orchestrate") {
        map.insert("orchestrate".to_string(), Value::Bool(true));
    } else if args.has("--direct") {
        map.insert("orchestrate".to_string(), Value::Bool(false));
    }
    let frames: Vec<Value> = args
        .all("--frame")
        .map(|v| Value::String(v.to_string()))
        .collect();
    if !frames.is_empty() {
        map.insert("reference_frame_ids".to_string(), Value::Array(frames));
    }
    insert_string(&mut map, "display_target", args.one("--display-target"));
    Ok(Value::Object(map))
}

async fn run_controller(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_controller();
        return Ok(());
    }
    match raw[0].as_str() {
        "status" => {
            let response = call_tool(
                client,
                config,
                "get_controller_loop_status",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "restart-status" => {
            let response = call_tool(
                client,
                config,
                "get_restart_status",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "halt" => {
            let args = parse_command_args(&raw[1..], &[], &["--one-shot"])?;
            let mut map = Map::new();
            if args.has("--one-shot") {
                map.insert("persistent".to_string(), Value::Bool(false));
            }
            let response = call_tool(
                client,
                config,
                "request_controller_loop_halt",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "clear-halt" => {
            let response = call_tool(
                client,
                config,
                "clear_controller_loop_halt",
                Value::Object(Map::new()),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "intervene" => {
            let args = parse_command_args(&raw[1..], &[], &[])?;
            let mode = args
                .positional
                .first()
                .ok_or_else(|| "controller intervene requires stop or abort".to_string())?;
            let response = call_tool(
                client,
                config,
                "intervene_controller_loop",
                json_object([("mode", Value::String(mode.clone()))]),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "schedule" => {
            let args = parse_command_args(
                &raw[1..],
                &[
                    "--controller-id",
                    "--goal",
                    "--reason",
                    "--after",
                    "--command",
                    "--max-attempts",
                    "--cooldown-sec",
                ],
                &["--auto-start"],
            )?;
            let mut map = Map::new();
            insert_required_string(&mut map, "controller_id", args.one("--controller-id"))?;
            insert_required_string(&mut map, "north_star_goal", args.one("--goal"))?;
            insert_string(&mut map, "reason", args.one("--reason"));
            insert_string(&mut map, "restart_after", args.one("--after"));
            insert_string(&mut map, "restart_command", args.one("--command"));
            insert_u32(&mut map, "max_attempts", args.one("--max-attempts"))?;
            insert_u64(&mut map, "cooldown_sec", args.one("--cooldown-sec"))?;
            if args.has("--auto-start") {
                map.insert("auto_start_task".to_string(), Value::Bool(true));
            }
            let response = call_tool(
                client,
                config,
                "schedule_controller_restart",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "cancel" => {
            let args = parse_command_args(&raw[1..], &["--restart-id"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "restart_id", args.one("--restart-id"));
            let response = call_tool(
                client,
                config,
                "cancel_controller_restart",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "complete" => {
            let args = parse_command_args(
                &raw[1..],
                &["--restart-id", "--token", "--status", "--summary"],
                &[],
            )?;
            let mut map = Map::new();
            insert_required_string(&mut map, "restart_id", args.one("--restart-id"))?;
            insert_required_string(&mut map, "turn_complete_token", args.one("--token"))?;
            insert_string(&mut map, "status", args.one("--status"));
            insert_string(&mut map, "handoff_summary", args.one("--summary"));
            let response = call_tool(
                client,
                config,
                "controller_turn_complete",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown controller command '{other}'")),
    }
    Ok(())
}

async fn run_context(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_context();
        return Ok(());
    }
    match raw[0].as_str() {
        "rewind" => {
            let args = parse_command_args(
                &raw[1..],
                &[
                    "--session",
                    "--item-id",
                    "--position",
                    "--reason",
                    "--primer",
                    "--preserve",
                    "--discard",
                    "--artifact",
                    "--next-step",
                ],
                &[],
            )?;
            let mut map = Map::new();
            insert_string(&mut map, "session_id", args.one("--session"));
            let item_id = args
                .one("--item-id")
                .ok_or_else(|| "context rewind requires --item-id".to_string())?;
            let position = args.one("--position").unwrap_or("before");
            map.insert(
                "anchor".to_string(),
                json_object([
                    ("item_id", Value::String(item_id.to_string())),
                    ("position", Value::String(position.to_string())),
                ]),
            );
            insert_required_string(&mut map, "reason", args.one("--reason"))?;
            insert_required_string(&mut map, "primer", args.one("--primer"))?;
            insert_string_array(&mut map, "preserve", args.all("--preserve"));
            insert_string_array(&mut map, "discard", args.all("--discard"));
            insert_string_array(&mut map, "artifacts", args.all("--artifact"));
            insert_string_array(&mut map, "next_steps", args.all("--next-step"));
            let response = call_tool(client, config, "rewind_context", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "inspect" | "inspect-anchor" => {
            let args = parse_command_args(&raw[1..], &["--session", "--item-id", "--radius"], &[])?;
            let mut map = Map::new();
            insert_string(&mut map, "session_id", args.one("--session"));
            insert_required_string(&mut map, "item_id", args.one("--item-id"))?;
            insert_u32(&mut map, "radius", args.one("--radius"))?;
            let response =
                call_tool(client, config, "inspect_rewind_anchor", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "backout" => {
            let args = parse_command_args(
                &raw[1..],
                &["--session", "--record-id", "--mode", "--name"],
                &["--allow-cache-reset"],
            )?;
            let mut map = Map::new();
            insert_string(&mut map, "session_id", args.one("--session"));
            insert_required_string(&mut map, "record_id", args.one("--record-id"))?;
            insert_string(&mut map, "mode", args.one("--mode"));
            insert_string(&mut map, "name", args.one("--name"));
            if args.has("--allow-cache-reset") {
                map.insert("allow_cache_reset".to_string(), Value::Bool(true));
            }
            let response = call_tool(client, config, "rewind_backout", Value::Object(map)).await?;
            print_tool_response(response, config, None)?;
        }
        "claim-fission" => {
            let args = parse_command_args(
                &raw[1..],
                &[
                    "--group-id",
                    "--branch-session-id",
                    "--expected-canonical-session-id",
                ],
                &[],
            )?;
            let mut map = Map::new();
            insert_required_string(&mut map, "group_id", args.one("--group-id"))?;
            insert_required_string(
                &mut map,
                "branch_session_id",
                args.one("--branch-session-id"),
            )?;
            insert_string(
                &mut map,
                "expected_canonical_session_id",
                args.one("--expected-canonical-session-id"),
            );
            let response = call_tool(
                client,
                config,
                "claim_fission_canonical",
                Value::Object(map),
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown context command '{other}'")),
    }
    Ok(())
}

async fn run_audio(
    client: &reqwest::Client,
    config: &Config,
    raw: &[String],
) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_audio();
        return Ok(());
    }
    match raw[0].as_str() {
        "spawn" => {
            let args = parse_command_args(&raw[1..], &["--args"], &[])?;
            let value = args
                .one("--args")
                .ok_or_else(|| "audio spawn requires --args JSON".to_string())
                .and_then(read_json_value)?;
            if !value.is_object() {
                return Err("--args must be a JSON object".to_string());
            }
            let response = call_tool(client, config, "spawn_live_audio", value).await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown audio command '{other}'")),
    }
    Ok(())
}

async fn run_peer(client: &reqwest::Client, config: &Config, raw: &[String]) -> Result<(), String> {
    if raw.is_empty() || is_help(raw) {
        help_peer();
        return Ok(());
    }
    match raw[0].as_str() {
        "list" => {
            let response =
                call_tool(client, config, "list_peers", Value::Object(Map::new())).await?;
            print_tool_response(response, config, None)?;
        }
        "message" => {
            let response = call_tool(
                client,
                config,
                "peer_send_message",
                peer_message_args(&raw[1..])?,
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        "task" => {
            let response = call_tool(
                client,
                config,
                "peer_delegate_task",
                peer_task_args(&raw[1..])?,
            )
            .await?;
            print_tool_response(response, config, None)?;
        }
        other => return Err(format!("unknown peer command '{other}'")),
    }
    Ok(())
}

fn peer_message_args(raw: &[String]) -> Result<Value, String> {
    let args = parse_command_args(raw, &["--session"], &[])?;
    let peer_id = args
        .positional
        .first()
        .ok_or_else(|| "peer message requires a peer id".to_string())?;
    let message = args
        .positional
        .get(1..)
        .filter(|rest| !rest.is_empty())
        .map(|rest| rest.join(" "))
        .ok_or_else(|| "peer message requires message text".to_string())?;
    let mut map = Map::new();
    map.insert("peer_id".to_string(), Value::String(peer_id.clone()));
    map.insert("message".to_string(), Value::String(message));
    insert_string(&mut map, "session", args.one("--session"));
    Ok(Value::Object(map))
}

fn peer_task_args(raw: &[String]) -> Result<Value, String> {
    let args = parse_command_args(raw, &["--context"], &[])?;
    let peer_id = args
        .positional
        .first()
        .ok_or_else(|| "peer task requires a peer id".to_string())?;
    let instructions = args
        .positional
        .get(1..)
        .filter(|rest| !rest.is_empty())
        .map(|rest| rest.join(" "))
        .ok_or_else(|| "peer task requires instructions".to_string())?;
    let mut map = Map::new();
    map.insert("peer_id".to_string(), Value::String(peer_id.clone()));
    map.insert("instructions".to_string(), Value::String(instructions));
    if let Some(context) = args.one("--context") {
        // Free-form context is legal: forward valid JSON parsed, anything else
        // as a plain string value.
        let value = serde_json::from_str::<Value>(context)
            .unwrap_or_else(|_| Value::String(context.to_string()));
        map.insert("context".to_string(), value);
    }
    Ok(Value::Object(map))
}

async fn call_tool(
    client: &reqwest::Client,
    config: &Config,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    rpc(
        client,
        config,
        "tools/call",
        serde_json::json!({
            "name": name,
            "arguments": arguments,
        }),
    )
    .await
}

async fn rpc(
    client: &reqwest::Client,
    config: &Config,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let url = mcp_url(config)?;
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mut request = client.post(url).json(&body);
    if config.peer.is_some() {
        // Opt into fail-closed peer semantics on the target gateway: without
        // a client cert the request is rejected instead of downgraded to an
        // anonymous principal.
        request = request.header(
            crate::peer::transport::intendant::PEER_CLIENT_HEADER,
            crate::peer::transport::intendant::PEER_CLIENT_HEADER_VALUE,
        );
        if let Some(bearer) = &config.bearer {
            request = request.bearer_auth(bearer);
        }
    }
    let response = request.send().await.map_err(|e| match &config.peer {
        Some(peer) => format!("request to peer '{peer}' failed: {e}"),
        None => format!("request failed: {e}"),
    })?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON-RPC response: {e}: {text}"))
}

fn mcp_url(config: &Config) -> Result<reqwest::Url, String> {
    let mut url =
        reqwest::Url::parse(&config.base_url).map_err(|e| format!("invalid MCP URL: {e}"))?;
    // Peer mode deliberately appends nothing: session_id / managed_context
    // scope sessions of the LOCAL daemon and are meaningless cross-daemon.
    if config.peer.is_none() {
        let mut pairs = url.query_pairs_mut();
        if let Some(session_id) = &config.session_id {
            pairs.append_pair("session_id", session_id);
        }
        if let Some(managed_context) = &config.managed_context {
            pairs.append_pair("managed_context", managed_context);
        }
    }
    Ok(url)
}

fn print_tool_response(
    response: Value,
    config: &Config,
    output_path: Option<PathBuf>,
) -> Result<(), String> {
    if config.raw {
        return print_json(&response);
    }
    if let Some(error) = response.get("error") {
        print_json(error)?;
        return Err("MCP tool call failed".to_string());
    }
    let result = response
        .get("result")
        .ok_or_else(|| "JSON-RPC response missing result".to_string())?;
    if config.json {
        if let Some(text) = single_text_content(result) {
            if let Ok(value) = serde_json::from_str::<Value>(text) {
                return print_json(&value);
            }
        }
        return print_json(result);
    }
    if let Some(path) = output_path {
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            for text in text_contents(result) {
                println!("{text}");
            }
            return Err("tool returned isError=true".to_string());
        }
        save_first_image_or_path(result, &path)?;
        for text in text_contents(result) {
            println!("{text}");
        }
        println!("wrote {}", path.display());
        return Ok(());
    }
    let mut printed = false;
    for text in text_contents(result) {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            print_json(&value)?;
        } else {
            println!("{text}");
        }
        printed = true;
    }
    let images = image_contents(result).count();
    if images > 0 {
        println!("[{images} image content block(s); rerun with --output PATH to save]");
        printed = true;
    }
    if !printed {
        print_json(result)?;
    }
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err("tool returned isError=true".to_string());
    }
    Ok(())
}

fn single_text_content(result: &Value) -> Option<&str> {
    let mut texts = text_contents(result);
    let first = texts.next()?;
    if texts.next().is_none() {
        Some(first)
    } else {
        None
    }
}

fn text_contents(result: &Value) -> impl Iterator<Item = &str> {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|content| content.iter())
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
}

fn image_contents(result: &Value) -> impl Iterator<Item = (&str, &str)> {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|content| content.iter())
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|item| {
            let data = item.get("data").and_then(Value::as_str)?;
            let mime = item
                .get("mimeType")
                .or_else(|| item.get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            Some((data, mime))
        })
}

fn save_first_image_or_path(result: &Value, path: &PathBuf) -> Result<(), String> {
    if let Some((data, _mime)) = image_contents(result).next() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("failed to decode image data: {e}"))?;
        return std::fs::write(path, bytes)
            .map_err(|e| format!("failed to write {}: {e}", path.display()));
    }

    if let Some(source) = screenshot_path_from_text(result) {
        std::fs::copy(&source, path).map_err(|e| {
            format!(
                "failed to copy screenshot from {} to {}: {e}",
                source.display(),
                path.display()
            )
        })?;
        return Ok(());
    }

    Err(
        "tool result did not include an image content block or readable screenshot_path"
            .to_string(),
    )
}

fn screenshot_path_from_text(result: &Value) -> Option<PathBuf> {
    text_contents(result)
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find_map(|value| {
            value
                .get("screenshot_path")
                .or_else(|| value.get("path"))
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
}

fn print_json(value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    println!("{text}");
    Ok(())
}

/// Validate a cu-actions JSON array against the real `CuAction` type before
/// sending, so shape mistakes fail fast with the expected shapes echoed back
/// instead of surfacing as an opaque server-side deserialization error.
/// Uses the same type the server deserializes into — no schema duplication.
fn validate_cu_actions(actions: &Value) -> Result<(), String> {
    let items = actions
        .as_array()
        .ok_or_else(|| format!("--actions must be a JSON array\n\n{CU_ACTION_SHAPES}"))?;
    if items.is_empty() {
        return Err("--actions array is empty; provide at least one action".to_string());
    }
    for (i, item) in items.iter().enumerate() {
        if let Err(e) = serde_json::from_value::<crate::computer_use::CuAction>(item.clone()) {
            return Err(format!(
                "actions[{i}] is not a valid CU action: {e}\n\n{CU_ACTION_SHAPES}\n\n\
                 For the raw JSON schema: intendant ctl tools schema execute_cu_actions\n\
                 To bypass client validation: intendant ctl tools call execute_cu_actions --args JSON"
            ));
        }
    }
    Ok(())
}

fn tool_arguments_from_flags(args: &CommandArgs) -> Result<Value, String> {
    let mut map = match args.one("--args") {
        Some(value) => match read_json_value(value)? {
            Value::Object(map) => map,
            _ => return Err("--args must be a JSON object".to_string()),
        },
        None => Map::new(),
    };
    for pair in args.all("--arg") {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("--arg expects key=value, got '{pair}'"))?;
        map.insert(key.to_string(), parse_jsonish(value)?);
    }
    Ok(Value::Object(map))
}

fn read_json_value(input: &str) -> Result<Value, String> {
    let text = if input == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("failed to read stdin: {e}"))?;
        text
    } else if let Some(path) = input.strip_prefix('@') {
        std::fs::read_to_string(path).map_err(|e| format!("failed to read {path}: {e}"))?
    } else {
        input.to_string()
    };
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON: {e}"))
}

fn parse_jsonish(value: &str) -> Result<Value, String> {
    if matches!(value, "true" | "false" | "null")
        || value.starts_with('{')
        || value.starts_with('[')
        || value.starts_with('"')
    {
        return serde_json::from_str(value)
            .map_err(|e| format!("invalid JSON value '{value}': {e}"));
    }
    if let Ok(v) = value.parse::<i64>() {
        return Ok(Value::from(v));
    }
    if let Ok(v) = value.parse::<f64>() {
        return Ok(Value::from(v));
    }
    Ok(Value::String(value.to_string()))
}

fn parse_region(value: &str) -> Result<Value, String> {
    let parts: Vec<&str> = value.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err("region must be x,y,width,height".to_string());
    }
    let parse = |s: &str| {
        s.parse::<f64>()
            .map_err(|_| format!("invalid region coordinate '{s}'"))
    };
    Ok(json_object([
        ("x", Value::from(parse(parts[0])?)),
        ("y", Value::from(parse(parts[1])?)),
        ("width", Value::from(parse(parts[2])?)),
        ("height", Value::from(parse(parts[3])?)),
    ]))
}

fn shared_target_map(args: &CommandArgs) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    insert_string(&mut map, "display_target", args.one("--target"));
    insert_u32(&mut map, "display_id", args.one("--display-id"))?;
    Ok(map)
}

fn output_path(value: Option<&str>) -> Option<PathBuf> {
    value.map(PathBuf::from)
}

fn json_object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

fn insert_string(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn insert_required_string(
    map: &mut Map<String, Value>,
    key: &str,
    value: Option<&str>,
) -> Result<(), String> {
    let value = value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("missing required --{}", key.replace('_', "-")))?;
    map.insert(key.to_string(), Value::String(value.to_string()));
    Ok(())
}

fn insert_string_array<'a>(
    map: &mut Map<String, Value>,
    key: &str,
    values: impl Iterator<Item = &'a str>,
) {
    let values: Vec<Value> = values.map(|v| Value::String(v.to_string())).collect();
    if !values.is_empty() {
        map.insert(key.to_string(), Value::Array(values));
    }
}

fn insert_u64(map: &mut Map<String, Value>, key: &str, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        let parsed = value
            .parse::<u64>()
            .map_err(|_| format!("invalid integer for --{}", key.replace('_', "-")))?;
        map.insert(key.to_string(), Value::from(parsed));
    }
    Ok(())
}

fn insert_u32(map: &mut Map<String, Value>, key: &str, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        let parsed = value
            .parse::<u32>()
            .map_err(|_| format!("invalid integer for --{}", key.replace('_', "-")))?;
        map.insert(key.to_string(), Value::from(parsed));
    }
    Ok(())
}

fn insert_usize(
    map: &mut Map<String, Value>,
    key: &str,
    value: Option<&str>,
) -> Result<(), String> {
    if let Some(value) = value {
        let parsed = value
            .parse::<usize>()
            .map_err(|_| format!("invalid integer for --{}", key.replace('_', "-")))?;
        map.insert(key.to_string(), Value::from(parsed));
    }
    Ok(())
}

fn positional_u64(args: &CommandArgs, index: usize, message: &str) -> Result<u64, String> {
    args.positional
        .get(index)
        .ok_or_else(|| message.to_string())?
        .parse::<u64>()
        .map_err(|_| message.to_string())
}

fn positional_u32(args: &CommandArgs, index: usize, message: &str) -> Result<u32, String> {
    args.positional
        .get(index)
        .ok_or_else(|| message.to_string())?
        .parse::<u32>()
        .map_err(|_| message.to_string())
}

fn is_help(raw: &[String]) -> bool {
    raw.len() == 1 && matches!(raw[0].as_str(), "-h" | "--help" | "help")
}

fn ensure_help(raw: &[String], help: fn()) -> Result<(), String> {
    if is_help(raw) {
        help();
        std::process::exit(0);
    }
    Ok(())
}

fn print_help() {
    println!(
        "intendant ctl controls a running Intendant daemon through its HTTP MCP endpoint.\n\
\n\
Usage: intendant ctl [global flags] <command> [args]\n\
\n\
Global flags:\n\
  --url URL                 MCP URL (default http://localhost:8765/mcp)\n\
  --port PORT               Dashboard/MCP port when --url is omitted\n\
  --peer ID                 Route commands to a federated peer's /mcp over mTLS ([[peer]] label or host, from the project intendant.toml or ~/.intendant/peers.toml); authorized by the profile the peer granted this daemon\n\
  --session ID              Session id to bind to the MCP request\n\
  --managed-context MODE    vanilla or managed\n\
  --json                    Print parsed JSON where possible\n\
  --raw                     Print raw JSON-RPC responses\n\
\n\
Commands:\n\
  status                    Get current status\n\
  logs                      Read log entries\n\
  tools                     Lazy MCP tool discovery and generic calls\n\
  display                   Displays, frames, screenshots, display claims\n\
  browser                   Browser workspaces and leases\n\
  cu                        Computer-use actions\n\
  shared                    Shared display collaboration\n\
  approval                  Pending approvals and approval responses\n\
  input                     Pending human question and response\n\
  settings                  Autonomy and verbosity\n\
  task                      Start tasks\n\
  controller                Controller loop and restart controls\n\
  context                   Managed-context rewind/backout controls\n\
  audio                     Live-audio controls\n\
  peer                      Federated peers, messaging, task delegation\n\
\n\
Run `intendant ctl <command> --help` for focused help."
    );
}

fn help_status() {
    println!("Usage: intendant ctl status [--json|--raw]");
}

fn help_logs() {
    println!(
        "Usage: intendant ctl logs [--since-id N] [--level LEVEL] [--limit N]\n\
Levels include info, model, agent, error, warn, subagent, debug."
    );
}

fn help_tools() {
    println!(
        "Usage:\n\
  intendant ctl tools list\n\
  intendant ctl tools schema TOOL\n\
  intendant ctl tools call TOOL [--args JSON|@file|-] [--arg key=value]\n\
\n\
Use this for lazy discovery of rare or newly-added Intendant capabilities."
    );
}

fn help_tools_list() {
    println!("Usage: intendant ctl tools list [--json|--raw]");
}

fn help_tools_call() {
    println!(
        "Usage: intendant ctl tools call TOOL [--args JSON|@file|-] [--arg key=value]\n\
Examples:\n\
  intendant ctl tools call get_status\n\
  intendant ctl tools call get_logs --arg limit=10"
    );
}

fn help_display() {
    println!(
        "Usage:\n\
  intendant ctl display list\n\
  intendant ctl display frames [--stream NAME] [--count N]\n\
  intendant ctl display read-frame [latest|ID] [--stream NAME]\n\
  intendant ctl display screenshot [--target TARGET] [--output out.png]\n\
  intendant ctl display grant-user [DISPLAY_ID|--display-id ID]\n\
  intendant ctl display revoke-user [DISPLAY_ID|--display-id ID] [--note TEXT]\n\
  intendant ctl display take DISPLAY_ID\n\
  intendant ctl display release DISPLAY_ID [--note TEXT]"
    );
}

fn help_display_screenshot() {
    println!(
        "Usage: intendant ctl display screenshot [--target TARGET] [--output out.png]\n\
Targets include user_session, display_99, 99, and legacy :99.\n\
Omit --target to auto-detect: a live agent virtual display when one exists, else the user session."
    );
}

fn help_browser() {
    println!(
        "Usage:\n\
  intendant ctl browser providers\n\
  intendant ctl browser list\n\
  intendant ctl browser create [URL] [--label TEXT] [--provider auto|cdp|system_cdp|playwright|agent_browser] [--peer PEER_ID] [--session ID] [--profile-dir PATH]\n\
  intendant ctl browser acquire WORKSPACE_ID [--holder ID] [--holder-kind agent|human] [--note TEXT] [--force]\n\
  intendant ctl browser release WORKSPACE_ID [--holder ID] [--note TEXT]\n\
  intendant ctl browser close WORKSPACE_ID [--reason TEXT]\n\
\n\
CDP uses a managed Chromium/Chrome-for-Testing executable by default. Use --provider system_cdp, or set INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER=1, to opt into system Chrome/Chromium."
    );
}

/// Canonical shape reference for the cu-actions JSON — shown by
/// `cu actions --help` and echoed on validation errors. Kept in sync with
/// `crate::computer_use::CuAction` by the round-trip test on
/// [`CU_ACTIONS_EXAMPLE`].
const CU_ACTION_SHAPES: &str = r#"Actions are a JSON array of tagged objects (coordinates in pixels unless
--coordinate-space normalized_1000 maps 0-1000 onto the display):
  {"type":"click","x":N,"y":N}                    optional "button": left|right|middle
  {"type":"double_click","x":N,"y":N}             optional "button"
  {"type":"triple_click","x":N,"y":N}             optional "button"
  {"type":"mouse_down","x":N,"y":N}               press without releasing; optional "button"
  {"type":"mouse_up","x":N,"y":N}                 release; optional "button"
  {"type":"type","text":"..."}                    trailing \n presses Enter
  {"type":"paste","text":"..."}                   clipboard+paste; fast for long text
  {"type":"key","key":"Return"}                   key or chord, e.g. "ctrl+shift+t"
  {"type":"hold_key","key":"shift","ms":N}        hold a key/chord for N ms
  {"type":"scroll","x":N,"y":N,"direction":"up|down|left|right"}  optional "amount" (default 3)
  {"type":"move_mouse","x":N,"y":N}
  {"type":"drag","start_x":N,"start_y":N,"end_x":N,"end_y":N}
  {"type":"screenshot"}
  {"type":"zoom","x":N,"y":N,"width":N,"height":N}  region capture at native (Retina) detail
  {"type":"wait","ms":N}
A screenshot of the final state is captured automatically after the last action (unless it was already a screenshot/zoom)."#;

/// A working example covering common actions, shown in help and parsed by a
/// unit test to guarantee the documented shapes match `CuAction`.
const CU_ACTIONS_EXAMPLE: &str = r#"[{"type":"click","x":120,"y":260},{"type":"type","text":"hello"},{"type":"key","key":"Return"}]"#;

fn help_cu() {
    println!(
        "Usage:\n\
  intendant ctl cu actions --actions JSON|@file|- [--target TARGET] [--coordinate-space pixel|normalized_1000] [--output out.png]\n\
  intendant ctl cu screenshot [--target TARGET] [--output out.png]\n\
  intendant ctl cu elements [--target TARGET] [--format text|json]\n\
\n\
Run `intendant ctl cu actions --help` for the action JSON shapes.\n\
`cu elements` reads the frontmost app's UI element tree (roles, labels, values, frames) — \n\
cheap textual grounding: click the center of a reported frame. macOS user-session only for now.\n\
Targets: user_session (needs display grant), 99/display_99 (virtual).\n\
Omit to auto-detect: a live agent virtual display when one exists, else the user session."
    );
}

fn help_cu_actions() {
    println!(
        "Usage: intendant ctl cu actions --actions JSON|@file|- [--target TARGET] [--coordinate-space pixel|normalized_1000] [--output out.png]\n\
\n\
{CU_ACTION_SHAPES}\n\
\n\
Example:\n\
  intendant ctl cu actions --actions '{CU_ACTIONS_EXAMPLE}' --output after.png"
    );
}

fn help_shared() {
    println!(
        "Usage:\n\
  intendant ctl shared show [--target TARGET|--display-id ID] [--reason TEXT] [--focus x,y,w,h]\n\
  intendant ctl shared focus --region x,y,w,h [--target TARGET|--display-id ID] [--note TEXT]\n\
  intendant ctl shared input [--target TARGET|--display-id ID] [--reason TEXT]\n\
  intendant ctl shared capture [--target TARGET|--display-id ID] [--output out.png]\n\
  intendant ctl shared hide [--reason TEXT]\n\
\n\
Regions are normalized fractions from 0.0 to 1.0."
    );
}

fn help_shared_focus() {
    println!("Usage: intendant ctl shared focus --region x,y,width,height [--note TEXT]");
}

fn help_approval() {
    println!(
        "Usage:\n\
  intendant ctl approval pending\n\
  intendant ctl approval approve ID\n\
  intendant ctl approval deny ID\n\
  intendant ctl approval skip ID\n\
  intendant ctl approval approve-all ID"
    );
}

fn help_input() {
    println!(
        "Usage:\n\
  intendant ctl input pending\n\
  intendant ctl input respond TEXT..."
    );
}

fn help_settings() {
    println!(
        "Usage:\n\
  intendant ctl settings autonomy low|medium|high|full\n\
  intendant ctl settings verbosity quiet|normal|verbose|debug"
    );
}

fn help_task() {
    println!(
        "Usage: intendant ctl task start [--task TEXT] [--session ID] [--orchestrate|--direct] [--display-target TARGET] [--frame ID]\n\
If --task is omitted, remaining positional text becomes the task."
    );
}

fn help_controller() {
    println!(
        "Usage:\n\
  intendant ctl controller status\n\
  intendant ctl controller restart-status\n\
  intendant ctl controller halt [--one-shot]\n\
  intendant ctl controller clear-halt\n\
  intendant ctl controller intervene stop|abort\n\
  intendant ctl controller schedule --controller-id ID --goal TEXT [--after turn_end|now]\n\
  intendant ctl controller cancel [--restart-id ID]\n\
  intendant ctl controller complete --restart-id ID --token TOKEN [--status TEXT] [--summary TEXT]"
    );
}

fn help_context() {
    println!(
        "Usage:\n\
  intendant ctl --managed-context managed context rewind --item-id ID --position before|after --reason TEXT --primer TEXT\n\
  intendant ctl --managed-context managed context inspect --item-id ID [--radius N]\n\
  intendant ctl --managed-context managed context backout --record-id ID [--mode inspect|restore|fork|backout]\n\
  intendant ctl context claim-fission --group-id ID --branch-session-id ID"
    );
}

fn help_audio() {
    println!(
        "Usage: intendant ctl audio spawn --args JSON|@file|-\n\
The JSON object is the spawn_live_audio parameter object."
    );
}

fn help_peer() {
    println!(
        "Usage:\n\
  intendant ctl peer list\n\
  intendant ctl peer message PEER_ID TEXT... [--session ID]\n\
  intendant ctl peer task PEER_ID INSTRUCTIONS... [--context JSON|TEXT]\n\
\n\
`list` shows the federated peers and their capabilities.\n\
`message` sends text to the peer's agent.\n\
`task` delegates work the peer's own agent executes under its own autonomy and approvals.\n\
--context accepts a JSON value; non-JSON text is passed through as a string."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    // Minted ids (session, request, workspace, …) ride ctl argv as flag
    // values and positionals; base64url mints can lead with '-'. These pin
    // the parser invariants that keep such tokens from dying as flags (the
    // 2026-07-08 `peer complete` merge-queue ejection class; see 8c9c0d96).

    #[test]
    fn parse_command_args_accepts_dash_leading_values_and_positionals() {
        // A dash-leading token in a value position is the value…
        let parsed =
            parse_command_args(&args(&["--session", "-vyeJaE3hyqm4"]), &["--session"], &[])
                .expect("flag value may lead with a dash");
        assert_eq!(parsed.one("--session"), Some("-vyeJaE3hyqm4"));
        // …and a single-dash token in positional position is a positional.
        let parsed = parse_command_args(&args(&["-vyeJaE3hyqm4"]), &[], &[])
            .expect("single-dash token is a positional, not a flag");
        assert_eq!(parsed.positional, vec!["-vyeJaE3hyqm4".to_string()]);
    }

    #[test]
    fn parse_command_args_double_dash_forces_positionals() {
        let parsed = parse_command_args(&args(&["--", "--looks-like-a-flag"]), &[], &[])
            .expect("everything after -- is positional");
        assert_eq!(parsed.positional, vec!["--looks-like-a-flag".to_string()]);
    }

    #[test]
    fn parse_command_args_unknown_double_dash_flag_still_errors() {
        let err = parse_command_args(&args(&["--sesion", "x"]), &["--session"], &[]).unwrap_err();
        assert!(err.contains("unknown flag --sesion"), "got: {err}");
    }

    #[test]
    fn parse_global_args_flag_values_accept_dash_leading_tokens() {
        let (config, command) =
            parse_global_args(args(&["--session", "-abc123", "status"])).expect("parses");
        assert_eq!(config.session_id.as_deref(), Some("-abc123"));
        assert_eq!(command, args(&["status"]));
    }

    #[test]
    fn cu_actions_example_matches_cu_action_type() {
        // Guards the documented example (and by extension CU_ACTION_SHAPES)
        // against drifting from the real CuAction enum.
        let value: Value = serde_json::from_str(CU_ACTIONS_EXAMPLE).expect("example parses");
        validate_cu_actions(&value).expect("example validates");
    }

    #[test]
    fn cu_action_shapes_cover_every_variant() {
        // One canonical instance per CuAction variant; adding a variant to the
        // enum without updating CU_ACTION_SHAPES should trip a reviewer here.
        let all = serde_json::json!([
            {"type":"click","x":1,"y":2,"button":"middle"},
            {"type":"double_click","x":1,"y":2},
            {"type":"triple_click","x":1,"y":2},
            {"type":"mouse_down","x":1,"y":2,"button":"left"},
            {"type":"mouse_up","x":1,"y":2},
            {"type":"type","text":"hello\n"},
            {"type":"paste","text":"long text"},
            {"type":"key","key":"ctrl+shift+t"},
            {"type":"hold_key","key":"shift","ms":500},
            {"type":"scroll","x":3,"y":4,"direction":"down","amount":2},
            {"type":"move_mouse","x":5,"y":6},
            {"type":"drag","start_x":1,"start_y":2,"end_x":3,"end_y":4},
            {"type":"screenshot"},
            {"type":"zoom","x":10,"y":20,"width":300,"height":200},
            {"type":"wait","ms":100},
        ]);
        validate_cu_actions(&all).expect("all shapes validate");
        let listed = |name: &str| {
            assert!(
                CU_ACTION_SHAPES.contains(&format!("\"type\":\"{name}\"")),
                "CU_ACTION_SHAPES is missing the {name} action"
            );
        };
        for name in [
            "click",
            "double_click",
            "triple_click",
            "mouse_down",
            "mouse_up",
            "type",
            "paste",
            "key",
            "hold_key",
            "scroll",
            "move_mouse",
            "drag",
            "screenshot",
            "zoom",
            "wait",
        ] {
            listed(name);
        }
    }

    #[test]
    fn invalid_cu_action_error_names_index_and_echoes_shapes() {
        let bad = serde_json::json!([
            {"type":"click","x":1,"y":2},
            {"type":"clik","x":1,"y":2},
        ]);
        let err = validate_cu_actions(&bad).expect_err("bad action rejected");
        assert!(err.contains("actions[1]"), "error names the index: {err}");
        assert!(
            err.contains("\"type\":\"click\""),
            "error echoes the shapes: {err}"
        );
    }

    #[test]
    fn non_array_and_empty_cu_actions_rejected() {
        let obj = serde_json::json!({"type":"click","x":1,"y":2});
        assert!(validate_cu_actions(&obj).is_err());
        assert!(validate_cu_actions(&serde_json::json!([])).is_err());
    }

    #[test]
    fn task_start_args_accepts_session_flag_after_subcommand() {
        let value = task_start_args(&args(&[
            "--session",
            "managed-session-1",
            "--direct",
            "continue",
            "the",
            "task",
        ]))
        .expect("task args should parse");

        assert_eq!(
            value.pointer("/session_id").and_then(Value::as_str),
            Some("managed-session-1")
        );
        assert_eq!(
            value.pointer("/task").and_then(Value::as_str),
            Some("continue the task")
        );
        assert_eq!(
            value.pointer("/orchestrate").and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn peer_message_args_joins_text_and_omits_absent_session() {
        let value = peer_message_args(&args(&["peer-1", "hello", "over", "there"]))
            .expect("message args should parse");

        assert_eq!(
            value.pointer("/peer_id").and_then(Value::as_str),
            Some("peer-1")
        );
        assert_eq!(
            value.pointer("/message").and_then(Value::as_str),
            Some("hello over there")
        );
        assert!(value.get("session").is_none());
    }

    #[test]
    fn peer_message_args_accepts_session_flag() {
        let value = peer_message_args(&args(&["--session", "sess-1", "peer-1", "ping", "pong"]))
            .expect("message args should parse");

        assert_eq!(
            value.pointer("/session").and_then(Value::as_str),
            Some("sess-1")
        );
        assert_eq!(
            value.pointer("/message").and_then(Value::as_str),
            Some("ping pong")
        );
    }

    #[test]
    fn peer_message_args_requires_peer_id_and_message_text() {
        assert!(peer_message_args(&args(&[])).is_err());
        assert!(peer_message_args(&args(&["peer-1"])).is_err());
    }

    #[test]
    fn peer_task_args_joins_instructions_and_parses_json_context() {
        let value = peer_task_args(&args(&[
            "peer-1",
            "audit",
            "the",
            "logs",
            "--context",
            r#"{"repo":"intendant"}"#,
        ]))
        .expect("task args should parse");

        assert_eq!(
            value.pointer("/peer_id").and_then(Value::as_str),
            Some("peer-1")
        );
        assert_eq!(
            value.pointer("/instructions").and_then(Value::as_str),
            Some("audit the logs")
        );
        assert_eq!(
            value.pointer("/context/repo").and_then(Value::as_str),
            Some("intendant")
        );
    }

    #[test]
    fn peer_task_args_passes_free_form_context_as_string() {
        let value = peer_task_args(&args(&["peer-1", "task", "--context", "just some notes"]))
            .expect("task args should parse");

        assert_eq!(
            value.pointer("/context").and_then(Value::as_str),
            Some("just some notes")
        );
    }

    #[test]
    fn peer_task_args_omits_absent_context() {
        let value = peer_task_args(&args(&["peer-1", "go"])).expect("task args should parse");
        assert!(value.get("context").is_none());
    }

    #[test]
    fn save_output_copies_screenshot_path_when_image_block_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("captured.png");
        let output = dir.path().join("requested.png");
        let png_bytes = b"\x89PNG\r\n\x1a\npath-backed";
        std::fs::write(&source, png_bytes).expect("write source");

        let result = serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::json!({
                    "status": "screenshot captured",
                    "screenshot_path": source,
                    "width": 10,
                    "height": 20
                }).to_string()
            }]
        });

        save_first_image_or_path(&result, &output).expect("save from screenshot_path");
        assert_eq!(std::fs::read(output).expect("read output"), png_bytes);
    }

    #[test]
    fn save_output_prefers_inline_image_block_over_screenshot_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("captured.png");
        let output = dir.path().join("requested.png");
        std::fs::write(&source, b"path-backed").expect("write source");

        let inline_bytes = b"\x89PNG\r\n\x1a\ninline";
        let inline = base64::engine::general_purpose::STANDARD.encode(inline_bytes);
        let result = serde_json::json!({
            "content": [
                {
                    "type": "text",
                    "text": serde_json::json!({
                        "screenshot_path": source
                    }).to_string()
                },
                {
                    "type": "image",
                    "data": inline,
                    "mimeType": "image/png"
                }
            ]
        });

        save_first_image_or_path(&result, &output).expect("save from image block");
        assert_eq!(std::fs::read(output).expect("read output"), inline_bytes);
    }

    fn peer_config(card_url: &str, label: Option<&str>) -> crate::project::PeerConfig {
        crate::project::PeerConfig {
            card_url: card_url.to_string(),
            label: label.map(str::to_string),
            bearer_token: None,
            via_urls: Vec::new(),
            client_cert: None,
            client_key: None,
            pinned_fingerprints: Vec::new(),
            browser_tcp_via_url: None,
        }
    }

    fn test_config() -> Config {
        Config {
            base_url: "http://localhost:8765/mcp".to_string(),
            session_id: Some("sess-1".to_string()),
            managed_context: Some("managed".to_string()),
            raw: false,
            json: false,
            peer: None,
            bearer: None,
        }
    }

    #[test]
    fn resolve_peer_matches_label_case_insensitively() {
        let peers = vec![
            peer_config(
                "https://mac.example:8766/.well-known/agent-card.json",
                Some("nicks-mac"),
            ),
            peer_config("https://dell.example/.well-known/agent-card.json", None),
        ];
        let peer = resolve_peer(&peers, "Nicks-Mac").expect("label matches");
        assert_eq!(peer.label.as_deref(), Some("nicks-mac"));
    }

    #[test]
    fn resolve_peer_matches_card_url_host() {
        let peers = vec![
            peer_config(
                "https://mac.example:8766/.well-known/agent-card.json",
                Some("nicks-mac"),
            ),
            peer_config("https://dell.example/.well-known/agent-card.json", None),
        ];
        let peer = resolve_peer(&peers, "dell.example").expect("host matches");
        assert_eq!(
            peer.card_url,
            "https://dell.example/.well-known/agent-card.json"
        );
    }

    #[test]
    fn resolve_peer_matches_colon_id_suffix_against_label_and_host() {
        let peers = vec![
            peer_config(
                "https://mac.example:8766/.well-known/agent-card.json",
                Some("nicks-mac"),
            ),
            peer_config("https://dell.example/.well-known/agent-card.json", None),
        ];
        let by_label = resolve_peer(&peers, "intendant:nicks-mac").expect("suffix label matches");
        assert_eq!(by_label.label.as_deref(), Some("nicks-mac"));
        let by_host = resolve_peer(&peers, "intendant:dell.example").expect("suffix host matches");
        assert_eq!(
            by_host.card_url,
            "https://dell.example/.well-known/agent-card.json"
        );
    }

    #[test]
    fn resolve_peer_matches_exact_card_url() {
        let card_url = "http://localhost:8766/.well-known/agent-card.json";
        let peers = vec![peer_config(card_url, None)];
        let peer = resolve_peer(&peers, card_url).expect("exact card_url matches");
        assert_eq!(peer.card_url, card_url);
    }

    #[test]
    fn resolve_peer_ambiguous_lists_matches() {
        let peers = vec![
            peer_config(
                "https://one.example/.well-known/agent-card.json",
                Some("twin"),
            ),
            peer_config(
                "https://two.example/.well-known/agent-card.json",
                Some("twin"),
            ),
        ];
        let err = resolve_peer(&peers, "twin").expect_err("ambiguous is an error");
        assert!(err.contains("ambiguous"), "says ambiguous: {err}");
        assert!(err.contains("twin (one.example)"), "lists first: {err}");
        assert!(err.contains("twin (two.example)"), "lists second: {err}");
    }

    #[test]
    fn resolve_peer_no_match_lists_configured_peers() {
        let peers = vec![
            peer_config(
                "https://mac.example:8766/.well-known/agent-card.json",
                Some("nicks-mac"),
            ),
            peer_config("https://dell.example/.well-known/agent-card.json", None),
        ];
        let err = resolve_peer(&peers, "nope").expect_err("no match is an error");
        assert!(err.contains("no configured peer matches 'nope'"), "{err}");
        assert!(err.contains("nicks-mac (mac.example)"), "{err}");
        assert!(err.contains("dell.example"), "{err}");
    }

    #[test]
    fn user_fallback_resolves_when_project_has_no_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let user_path = dir.path().join("peers.toml");
        std::fs::write(
            &user_path,
            r#"
[[peer]]
card_url = "https://dell.example/.well-known/agent-card.json"
label = "dell"
bearer_token = "tok"
"#,
        )
        .expect("write user peers");
        // No project config at all — and the same matching rules (here the
        // `intendant:<label>` suffix form) apply to the user layer.
        let peer = resolve_peer_with_user_fallback(
            &[],
            &dir.path().join("intendant.toml"),
            &user_path,
            "intendant:dell",
        )
        .expect("user-level fallback resolves");
        assert_eq!(peer.label.as_deref(), Some("dell"));
        assert_eq!(peer.bearer_token.as_deref(), Some("tok"));
        assert_eq!(
            peer.card_url,
            "https://dell.example/.well-known/agent-card.json"
        );
    }

    #[test]
    fn project_match_wins_without_reading_the_user_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let user_path = dir.path().join("peers.toml");
        // Deliberately corrupt: a project match must short-circuit before
        // the user layer is even read, so this must not error.
        std::fs::write(&user_path, "not [ valid toml").expect("write corrupt user peers");
        let project_peers = vec![peer_config(
            "https://project.example/.well-known/agent-card.json",
            Some("dell"),
        )];
        let peer = resolve_peer_with_user_fallback(
            &project_peers,
            &dir.path().join("intendant.toml"),
            &user_path,
            "dell",
        )
        .expect("project layer wins");
        assert_eq!(
            peer.card_url,
            "https://project.example/.well-known/agent-card.json"
        );
    }

    #[test]
    fn user_fallback_no_match_error_names_both_locations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_config = dir.path().join("intendant.toml");
        std::fs::write(&project_config, "").expect("write project config");
        let user_path = dir.path().join("peers.toml");
        std::fs::write(
            &user_path,
            "[[peer]]\ncard_url = \"https://dell.example/.well-known/agent-card.json\"\n",
        )
        .expect("write user peers");
        let project_peers = vec![peer_config(
            "https://mac.example:8766/.well-known/agent-card.json",
            Some("nicks-mac"),
        )];
        let err =
            resolve_peer_with_user_fallback(&project_peers, &project_config, &user_path, "nope")
                .expect_err("no match is an error");
        assert!(err.contains("no configured peer matches 'nope'"), "{err}");
        assert!(
            err.contains(&project_config.display().to_string()),
            "names the project config: {err}"
        );
        assert!(
            err.contains(&user_path.display().to_string()),
            "names the user peers file: {err}"
        );
        assert!(
            err.contains("nicks-mac (mac.example)"),
            "lists project peers: {err}"
        );
        assert!(err.contains("dell.example"), "lists user peers: {err}");
    }

    #[test]
    fn user_fallback_no_match_error_marks_absent_files_as_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_config = dir.path().join("intendant.toml");
        let user_path = dir.path().join("peers.toml");
        let err = resolve_peer_with_user_fallback(&[], &project_config, &user_path, "dell")
            .expect_err("nothing configured anywhere is an error");
        assert!(err.contains("no configured peer matches 'dell'"), "{err}");
        assert!(
            err.contains(&format!("{} (not found)", project_config.display())),
            "{err}"
        );
        assert!(
            err.contains(&format!("{} (not found)", user_path.display())),
            "{err}"
        );
    }

    #[test]
    fn ambiguity_within_the_user_layer_stays_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let user_path = dir.path().join("peers.toml");
        std::fs::write(
            &user_path,
            r#"
[[peer]]
card_url = "https://one.example/.well-known/agent-card.json"
label = "twin"

[[peer]]
card_url = "https://two.example/.well-known/agent-card.json"
label = "twin"
"#,
        )
        .expect("write user peers");
        let err = resolve_peer_with_user_fallback(
            &[],
            &dir.path().join("intendant.toml"),
            &user_path,
            "twin",
        )
        .expect_err("ambiguous within one layer is an error");
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("twin (one.example)"), "{err}");
        assert!(err.contains("twin (two.example)"), "{err}");
    }

    #[test]
    fn load_user_peers_missing_file_is_empty_and_invalid_file_is_loud() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("peers.toml");
        assert!(
            load_user_peers(&path)
                .expect("missing file means no user-level peers")
                .is_empty()
        );
        std::fs::write(&path, "not [ valid toml").expect("write corrupt file");
        let err = load_user_peers(&path).expect_err("corrupt file errors");
        assert!(
            err.contains(&path.display().to_string()),
            "names the file: {err}"
        );
    }

    /// The user-level fallback file must live under the state root
    /// (`intendant_home()`): intendant-core's state_paths tests pin that
    /// `$INTENDANT_HOME` relocates that root (and its default follows
    /// `$HOME`), so this derivation is what isolates hermetic harnesses —
    /// the e2e rigs point `HOME` at a temp dir and their spawned binaries
    /// then derive the peers path under the rig home, never the developer's
    /// real `~/.intendant/peers.toml`. Pure path computation: the test
    /// itself reads nothing from the real home. (In this bin's test build
    /// `intendant_home()` is process-cached and env mutation races the
    /// parallel runner — per the state_paths convention, behavior tests
    /// thread explicit paths instead, as the tests above do.)
    #[test]
    fn user_peers_file_is_under_the_state_root() {
        assert_eq!(
            user_peers_file(),
            crate::platform::intendant_home().join("peers.toml")
        );
    }

    #[test]
    fn peer_mcp_endpoint_keeps_explicit_port_and_drops_path_and_query() {
        assert_eq!(
            peer_mcp_endpoint("https://peer.example:8766/.well-known/agent-card.json?v=1")
                .expect("endpoint derives"),
            "https://peer.example:8766/mcp"
        );
    }

    #[test]
    fn peer_mcp_endpoint_without_explicit_port() {
        assert_eq!(
            peer_mcp_endpoint("http://peer.example/.well-known/agent-card.json")
                .expect("endpoint derives"),
            "http://peer.example/mcp"
        );
    }

    #[test]
    fn peer_mcp_endpoint_rejects_non_http_schemes() {
        assert!(peer_mcp_endpoint("ws://peer.example/ws").is_err());
        assert!(peer_mcp_endpoint("not a url").is_err());
    }

    #[test]
    fn peer_mode_mcp_url_omits_session_params_non_peer_keeps_them() {
        let mut config = test_config();
        config.peer = Some("nicks-mac".to_string());
        let url = mcp_url(&config).expect("peer url parses");
        assert_eq!(url.query(), None, "peer mode appends no query params");

        let config = test_config();
        let url = mcp_url(&config).expect("local url parses");
        let query = url.query().expect("local mode keeps query params");
        assert!(query.contains("session_id=sess-1"), "{query}");
        assert!(query.contains("managed_context=managed"), "{query}");
    }

    #[test]
    fn parse_global_args_rejects_peer_combined_with_url() {
        let err = parse_global_args(args(&["--peer", "x", "--url", "http://h/mcp", "status"]))
            .expect_err("conflict is an error");
        assert!(err.contains("mutually exclusive"), "{err}");
        let err = parse_global_args(args(&["--url=http://h/mcp", "--peer=x", "status"]))
            .expect_err("conflict is an error in = form too");
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn parse_global_args_accepts_both_peer_flag_forms() {
        let (config, command) =
            parse_global_args(args(&["--peer", "nicks-mac", "status"])).expect("space form");
        assert_eq!(config.peer.as_deref(), Some("nicks-mac"));
        assert!(config.bearer.is_none());
        assert_eq!(command, args(&["status"]));

        let (config, command) =
            parse_global_args(args(&["--peer=intendant:nicks-mac", "display", "list"]))
                .expect("= form");
        assert_eq!(config.peer.as_deref(), Some("intendant:nicks-mac"));
        assert_eq!(command, args(&["display", "list"]));
    }

    #[test]
    fn parse_global_args_rejects_missing_or_empty_peer_value() {
        assert!(parse_global_args(args(&["--peer"])).is_err());
        assert!(parse_global_args(args(&["--peer=", "status"])).is_err());
        assert!(parse_global_args(args(&["--peer", "  ", "status"])).is_err());
    }
}
