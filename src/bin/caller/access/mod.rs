//! `intendant access` subcommand.
//!
//! Generates a per-user access CA plus server/client certificates for the
//! native `--tls` / `--mtls` dashboard gateway, then optionally runs a
//! strict HTTPS enrollment server for importing the client identity on
//! browsers and mobile devices.
//!
//! Shared across platforms: cert generation (pure-Rust rcgen +
//! p12-keystore), client cert distribution, and import instructions.
//! Platform differences are isolated behind the `AccessBackend` trait.

use std::{fmt, net::IpAddr, path::PathBuf};

pub mod access_policy;
pub mod actor;
pub(crate) mod authority_store;
pub mod backend;
// `serve-certs` remains a Unix-only interactive server. Windows setup uses
// the same pure-Rust certificate generation and IAM seeding, then prints
// exact CurrentUser certificate-store import commands instead.
#[cfg(not(target_os = "windows"))]
pub mod cert_server;
pub mod certs;
pub mod client_key;
pub mod enrollment;
pub mod iam;
pub mod org;
pub mod pinning;
pub mod state;
#[cfg(not(target_os = "windows"))]
pub mod wizard;

/// Resolve the display label for this daemon.
///
/// The access cert store can outlive IP changes. Older setups also defaulted
/// `host_label` to the primary IP address, which made browser/client access
/// labels look like transport coordinates instead of daemon identity. Prefer a
/// human-readable stored label, then the system hostname, and use an IP address
/// only as the last real fallback.
///
/// Callable from `intendant --web` without running any `access` action,
/// because the backend's `cert_dir()` is a pure path accessor with no
/// privileged I/O.
///
/// Cached for the process lifetime: resolution shells out to `hostname`
/// and probes up to three cert-store directories, and callers sit on
/// request-serving paths (org target ids, peer cards). The label only
/// changes through `intendant access` runs, which are separate processes.
pub fn resolve_host_label() -> String {
    static LABEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LABEL
        .get_or_init(|| {
            let be = backend::select_backend();
            let cert_dir = be.cert_dir();
            let candidates = host_label_candidates(&cert_dir);
            choose_host_label(candidates, hostname().ok().as_deref())
        })
        .clone()
}

/// Read the system hostname by shelling out to the platform `hostname` command.
fn hostname() -> Result<String, std::io::Error> {
    let output = std::process::Command::new("hostname").output()?;
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(s)
}

fn host_label_candidates(primary_cert_dir: &std::path::Path) -> Vec<String> {
    let mut paths = vec![primary_cert_dir.to_path_buf()];
    if let Some(data_dir) = dirs::data_dir() {
        paths.push(data_dir.join("intendant").join("access-certs"));
    }
    paths.push(crate::platform::intendant_home().join("access-certs"));

    let mut out = Vec::new();
    for path in dedup_paths(paths) {
        if let Ok(label) = state::read_host_label(&path) {
            out.push(label);
        }
    }
    out
}

fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in paths {
        if !out.iter().any(|existing| existing == &path) {
            out.push(path);
        }
    }
    out
}

fn choose_host_label(candidates: Vec<String>, system_hostname: Option<&str>) -> String {
    let cleaned: Vec<String> = candidates
        .into_iter()
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty())
        .collect();

    if let Some(label) = cleaned.iter().find(|label| !is_ip_label(label)) {
        return label.clone();
    }

    let hostname = system_hostname.unwrap_or("").trim();
    if !hostname.is_empty() && !is_ip_label(hostname) {
        return hostname.to_string();
    }

    if let Some(label) = cleaned.first() {
        return label.clone();
    }
    if !hostname.is_empty() {
        return hostname.to_string();
    }
    "local".to_string()
}

fn is_ip_label(label: &str) -> bool {
    label.parse::<IpAddr>().is_ok()
}

// Network-interface enumeration lives in intendant-core so leaf crates
// (the display pipeline's WebRTC host-candidate gathering) can use it
// without a dependency on the access subsystem. Re-exported here so
// every existing `crate::access::routable_local_addrs` caller keeps
// compiling.
pub use intendant_core::net::{is_link_local_v6, routable_local_addrs};

/// First-boot self-provisioning for the dashboard's default-mTLS material.
///
/// The dashboard refuses plaintext by default, and a service-managed boot
/// on a fresh machine has no human at a prompt to run `intendant access
/// setup` — without this, `install.sh --service` on a clean box installs a
/// crash loop. A truly virgin cert dir is provisioned in place with the
/// same durable material setup would create (CA, server pair for the
/// machine's addresses, enrollable client identity). Returns `Ok(None)`
/// when the dir holds any existing material: partial or foreign state
/// keeps the loud startup error instead, because regenerating a CA
/// strands every browser enrolled against it.
pub fn provision_virgin_access_certs() -> AccessResult<Option<PathBuf>> {
    let be = backend::select_backend();
    let cert_dir = be.cert_dir();
    if !certs::dir_is_virgin(&cert_dir) {
        return Ok(None);
    }
    let primary_ip = match be.detect_primary_ip() {
        Ok(text) => text.parse().map_err(|_| {
            AccessError(format!("detected primary IP '{text}' is not an IP address"))
        })?,
        // detect_primary_ip shells out (`route` / `ip`), which a service
        // environment's minimal PATH may not carry. Interface enumeration
        // still works — provision for what the box has rather than fail
        // the whole first boot on knowing which address is primary.
        Err(err) => match routable_local_addrs(false).into_iter().next() {
            Some(addr) => addr,
            None => return Err(err),
        },
    };
    let server_names =
        certs::ServerNames::new(primary_ip, routable_local_addrs(false), Vec::new())?;
    std::fs::create_dir_all(&cert_dir)
        .map_err(|e| AccessError(format!("create {}: {e}", cert_dir.display())))?;
    certs::ensure_certs(&cert_dir, &server_names, &resolve_host_label(), false)?;
    iam::seed_generated_browser_mtls_owner_root(&cert_dir)?;
    Ok(Some(cert_dir))
}

#[cfg(target_os = "linux")]
pub mod backend_linux;

#[cfg(target_os = "macos")]
pub mod backend_macos;

/// Errors from the access subcommand — string-based on purpose: this is a
/// user-facing setup tool and most errors are meant to be printed and
/// exited on, not matched programmatically.
#[derive(Debug)]
pub struct AccessError(pub String);

impl fmt::Display for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AccessError {}

impl From<std::io::Error> for AccessError {
    fn from(e: std::io::Error) -> Self {
        AccessError(format!("io: {e}"))
    }
}

// `certs` (rcgen-based, pure-Rust) uses `?` on `rcgen::Error`; surface it as
// a AccessError. Available on all platforms, like the `certs` module itself.
impl From<rcgen::Error> for AccessError {
    fn from(e: rcgen::Error) -> Self {
        AccessError(format!("rcgen: {e}"))
    }
}

pub type AccessResult<T> = Result<T, AccessError>;

/// Parsed `intendant access <action> [flags]` invocation.
#[derive(Debug)]
pub struct AccessArgs {
    pub action: AccessAction,
    pub https_port: u16,
    pub cert_port: u16,
    pub ips: Vec<String>,
    pub hosts: Vec<String>,
    pub name: Option<String>,
    pub force: bool,
    /// Skip the interactive cert distribution server at the end of setup.
    /// Used by host orchestrators (e.g. the Windows batch script) that
    /// manage the distribution flow themselves and need setup to return
    /// as soon as the certs are written.
    pub no_serve_certs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessAction {
    Setup,
    Recert,
    Remove,
    List,
    ServeCerts,
    Help,
}

impl Default for AccessArgs {
    fn default() -> Self {
        Self {
            action: AccessAction::Help,
            https_port: intendant_core::net::DEFAULT_GATEWAY_PORT,
            cert_port: 9999,
            ips: Vec::new(),
            hosts: Vec::new(),
            name: None,
            force: false,
            no_serve_certs: false,
        }
    }
}

/// Top-level entry invoked from `main()` when argv[1] == "access".
pub async fn run(argv: Vec<String>) -> AccessResult<()> {
    let args = parse_args(&argv)?;
    match args.action {
        AccessAction::Help => {
            print_help();
            Ok(())
        }
        AccessAction::Setup => cmd_setup(args).await,
        AccessAction::Recert => cmd_recert(args).await,
        AccessAction::Remove => cmd_remove(args).await,
        AccessAction::List => cmd_list(args),
        AccessAction::ServeCerts => cmd_serve_certs(args).await,
    }
}

fn parse_args(argv: &[String]) -> AccessResult<AccessArgs> {
    let mut args = AccessArgs::default();

    let mut iter = argv.iter();
    let Some(first) = iter.next() else {
        return Ok(args);
    };

    args.action = match first.as_str() {
        "setup" => AccessAction::Setup,
        "recert" => AccessAction::Recert,
        "remove" => AccessAction::Remove,
        "list" => AccessAction::List,
        "serve-certs" => AccessAction::ServeCerts,
        "help" | "-h" | "--help" => return Ok(args),
        other => {
            return Err(AccessError(format!(
                "unknown access subcommand '{other}' (expected setup/recert/remove/list/serve-certs)"
            )));
        }
    };

    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--port" => {
                let v = iter
                    .next()
                    .ok_or_else(|| AccessError("missing value for --port".into()))?;
                args.https_port = v
                    .parse()
                    .map_err(|_| AccessError(format!("invalid --port value '{v}'")))?;
            }
            "--cert-port" => {
                let v = iter
                    .next()
                    .ok_or_else(|| AccessError("missing value for --cert-port".into()))?;
                args.cert_port = v
                    .parse()
                    .map_err(|_| AccessError(format!("invalid --cert-port value '{v}'")))?;
            }
            "--ip" => {
                let v = iter
                    .next()
                    .ok_or_else(|| AccessError("missing value for --ip".into()))?;
                args.ips.push(v.clone());
            }
            "--host" => {
                let v = iter
                    .next()
                    .ok_or_else(|| AccessError("missing value for --host".into()))?;
                args.hosts.push(v.clone());
            }
            "--name" => {
                let v = iter
                    .next()
                    .ok_or_else(|| AccessError("missing value for --name".into()))?;
                args.name = Some(v.clone());
            }
            "--force" => {
                args.force = true;
            }
            "--no-serve-certs" => {
                args.no_serve_certs = true;
            }
            "-h" | "--help" => {
                args.action = AccessAction::Help;
                return Ok(args);
            }
            other => {
                return Err(AccessError(format!("unknown flag '{other}'")));
            }
        }
    }

    Ok(args)
}

fn print_help() {
    println!("Intendant dashboard access setup");
    println!();
    println!("USAGE:");
    println!("    intendant access <action> [flags]");
    println!();
    println!("ACTIONS:");
    #[cfg(not(target_os = "windows"))]
    println!("    setup         Generate native dashboard mTLS certs and start enrollment");
    #[cfg(target_os = "windows")]
    println!("    setup         Generate native dashboard mTLS certs and print import steps");
    println!("    recert        Regenerate the server cert after access addresses change");
    println!("    remove        Remove the per-user access cert store");
    println!("    list          Show current setup state");
    #[cfg(not(target_os = "windows"))]
    println!("    serve-certs   Run strict HTTPS client cert enrollment");
    #[cfg(target_os = "windows")]
    println!("    serve-certs   Unsupported; setup prints manual PowerShell import steps");
    println!();
    println!("FLAGS:");
    println!("    --port <N>         Native dashboard HTTPS port to advertise (default 8765)");
    #[cfg(not(target_os = "windows"))]
    println!("    --cert-port <N>    Port for the HTTPS enrollment server (default 9999)");
    #[cfg(target_os = "windows")]
    println!("    --cert-port <N>    Accepted for compatibility; unused on Windows");
    println!("    --ip <IP>          Add an IP SAN; first --ip becomes the dashboard URL host");
    println!("    --host <DNS>       Add a DNS SAN");
    println!("    --name <LABEL>     Host label shown in cert CN and multi-host dashboard");
    println!("    --force            Skip idempotency checks (regenerate even if current)");
    #[cfg(not(target_os = "windows"))]
    println!("    --no-serve-certs   Skip the enrollment server at the end of setup");
    #[cfg(target_os = "windows")]
    println!("    --no-serve-certs   No-op; Windows setup never starts enrollment");
    println!();
    println!("NOTES:");
    println!("    Loopback SANs are always included: localhost, 127.0.0.1, ::1.");
    println!("    Detected local interface IPs are included. Public interface IPs are");
    println!("    allowed, but WAN exposure should use default mTLS, not only --tls.");
    #[cfg(target_os = "windows")]
    println!("    Windows setup never starts an enrollment server; it imports locally.");
}

async fn cmd_setup(args: AccessArgs) -> AccessResult<()> {
    let be = backend::select_backend();
    let server_names = resolve_server_names(&args, be.as_ref())?;
    let dashboard_host = url_host_for_ip(server_names.primary_ip);

    let cert_dir = be.cert_dir();
    std::fs::create_dir_all(&cert_dir)?;

    let label = args.name.clone().unwrap_or_else(|| {
        hostname()
            .ok()
            .map(|host| host.trim().to_string())
            .filter(|host| !host.is_empty() && !is_ip_label(host))
            .unwrap_or_else(|| server_names.primary_ip.to_string())
    });

    print_public_ip_warnings(&server_names);

    let state = certs::ensure_certs(&cert_dir, &server_names, &label, args.force)?;
    iam::seed_generated_browser_mtls_owner_root(&cert_dir)?;
    state::write_host_label(&cert_dir, &label)?;

    println!();
    println!("============================================================");
    println!("  Access certs ready");
    println!("============================================================");
    println!();
    println!("  Native access certs: {}", cert_dir.display());
    println!("  Start or restart the dashboard with:");
    println!("    intendant");
    println!("  That default requires enrolled browser/client certificates.");
    println!("  `intendant --tls` can serve the public shell and discovery without a");
    println!("  client certificate, but remote dashboard/API/WebSocket control still");
    println!("  requires enrolled direct mTLS. Loopback remains root.");
    println!("  No Developer ID-signed/notarized native release exists for this alpha.");
    println!();
    println!(
        "  Dashboard URL: https://{dashboard_host}:{}",
        args.https_port
    );
    println!();

    #[cfg(target_os = "windows")]
    {
        println!("{}", windows_import_instructions(&state.cert_dir));
    }

    #[cfg(not(target_os = "windows"))]
    {
        if args.no_serve_certs {
            // Host orchestrators can run strict enrollment separately when
            // they have an interactive operator channel for fingerprint
            // verification.
            println!("  Enrollment server was not started (--no-serve-certs).");
            println!("  Run `intendant access serve-certs` later to enroll devices.");
            println!();
            return Ok(());
        }

        // Start strict client enrollment (blocks until Ctrl+C).
        println!(
            "  Starting strict HTTPS enrollment on port {}.",
            args.cert_port
        );
        println!("  The enrollment page contains the device-specific install steps.");
        println!("  Press Ctrl+C here when all devices are enrolled.");
        println!();
        cert_server::serve(&state, args.cert_port, &dashboard_host, args.https_port).await?;
    }

    Ok(())
}

async fn cmd_recert(args: AccessArgs) -> AccessResult<()> {
    let be = backend::select_backend();
    let server_names = resolve_server_names(&args, be.as_ref())?;

    let cert_dir = be.cert_dir();
    if !cert_dir.join("ca.key").exists() {
        return Err(AccessError(format!(
            "no CA found in {} — run `intendant access setup` first",
            cert_dir.display()
        )));
    }

    print_public_ip_warnings(&server_names);
    certs::recert(&cert_dir, &server_names, args.force)?;

    println!(":: done — native access server cert refreshed");
    println!(":: restart any running `intendant` daemon to load it");
    println!(":: enrolled clients can keep using the same CA and client identity");

    Ok(())
}

async fn cmd_remove(_args: AccessArgs) -> AccessResult<()> {
    let be = backend::select_backend();
    let cert_dir = be.cert_dir();
    if cert_dir.exists() {
        std::fs::remove_dir_all(&cert_dir)?;
        println!(":: removed cert dir {}", cert_dir.display());
    }
    #[cfg(target_os = "windows")]
    println!(
        ":: generated files are removed; any certificates you manually imported into \
         CurrentUser\\Root or CurrentUser\\My remain until you remove them with certmgr.msc"
    );
    println!(":: done");
    Ok(())
}

fn cmd_list(_args: AccessArgs) -> AccessResult<()> {
    let be = backend::select_backend();
    let cert_dir = be.cert_dir();
    if !cert_dir.join("ca.crt").exists() {
        println!(":: no setup found (cert dir: {})", cert_dir.display());
        return Ok(());
    }
    let label = state::read_host_label(&cert_dir).unwrap_or_else(|_| "<unknown>".to_string());
    println!("  Cert dir:   {}", cert_dir.display());
    println!("  Host label: {label}");
    if let Ok(ip) = certs::current_cert_ip(&cert_dir) {
        println!("  Primary IP: {ip}");
    }
    Ok(())
}

async fn cmd_serve_certs(args: AccessArgs) -> AccessResult<()> {
    let be = backend::select_backend();
    let cert_dir = be.cert_dir();

    #[cfg(target_os = "windows")]
    {
        let _ = args;
        if !cert_dir.join("client.p12").exists() {
            return Err(AccessError(format!(
                "`intendant access serve-certs` is not available on Windows, and no client identity exists in {} — run `intendant access setup` first; setup prints exact manual PowerShell import commands",
                cert_dir.display()
            )));
        }
        return Err(AccessError(format!(
            "`intendant access serve-certs` is not available on Windows; import the locally generated identity instead:\n{}",
            windows_import_instructions(&cert_dir)
        )));
    }

    #[cfg(not(target_os = "windows"))]
    {
        if !cert_dir.join("client.p12").exists() {
            return Err(AccessError(format!(
                "no client.p12 found in {} — run `intendant access setup` first",
                cert_dir.display()
            )));
        }
        let state = certs::CertState {
            cert_dir: cert_dir.clone(),
            p12_password: state::read_p12_password(&cert_dir)?,
            label: state::read_host_label(&cert_dir).unwrap_or_default(),
        };
        let server_names = resolve_server_names(&args, be.as_ref())?;
        let dashboard_host = url_host_for_ip(server_names.primary_ip);
        cert_server::serve(&state, args.cert_port, &dashboard_host, args.https_port).await?;
        Ok(())
    }
}

fn resolve_server_names(
    args: &AccessArgs,
    be: &dyn backend::AccessBackend,
) -> AccessResult<certs::ServerNames> {
    let primary_ip_text = match args.ips.first() {
        Some(ip) => {
            println!(":: primary IP: {ip} (override)");
            ip.clone()
        }
        None => be.detect_primary_ip()?,
    };
    let primary_ip = primary_ip_text
        .parse()
        .map_err(|_| AccessError(format!("invalid --ip value '{primary_ip_text}'")))?;
    let mut ips = routable_local_addrs(false);
    for ip in &args.ips {
        ips.push(
            ip.parse()
                .map_err(|_| AccessError(format!("invalid --ip value '{ip}'")))?,
        );
    }
    certs::ServerNames::new(primary_ip, ips, args.hosts.clone())
}

fn url_host_for_ip(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    }
}

fn print_public_ip_warnings(server_names: &certs::ServerNames) {
    for ip in &server_names.ips {
        if is_public_ip(ip) {
            println!("!! public interface address included in server cert: {ip}");
            println!("!! WAN exposure should use default mTLS, not only `--tls`.");
        }
    }
}

fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let o = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                || (o[0] == 198 && (18..=19).contains(&o[1])))
        }
        std::net::IpAddr::V6(ip) => {
            let s = ip.segments();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || is_link_local_v6(ip)
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}

/// Copy/paste-ready PowerShell for importing the locally generated access CA
/// and client identity into the same Windows user's certificate stores. The
/// PKCS#12 password stays on disk and is converted directly into a
/// `SecureString`; it is never echoed into terminal history.
#[cfg(any(target_os = "windows", test))]
fn windows_import_instructions(cert_dir: &std::path::Path) -> String {
    let cert_dir = powershell_single_quoted(&cert_dir.to_string_lossy());
    format!(
        r#"  Windows certificate-store import (CurrentUser; no administrator required):
  Open PowerShell as this same Windows user and run exactly:

$certDir = {cert_dir}
Import-Certificate -FilePath (Join-Path $certDir 'ca.crt') -CertStoreLocation 'Cert:\CurrentUser\Root'
$pfxPassword = ConvertTo-SecureString -String ((Get-Content -Raw -LiteralPath (Join-Path $certDir 'p12_password')).Trim()) -AsPlainText -Force
Import-PfxCertificate -FilePath (Join-Path $certDir 'client.p12') -CertStoreLocation 'Cert:\CurrentUser\My' -Password $pfxPassword
Remove-Variable pfxPassword

  Close and reopen Edge or Chrome after import. Firefox may require its own
  certificate import unless enterprise-root integration is enabled.
  `serve-certs` is intentionally unavailable on Windows; use these local
  CurrentUser imports instead."#
    )
}

#[cfg(any(target_os = "windows", test))]
fn powershell_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn include_loopback_prepends_localhost() {
        let addrs = routable_local_addrs(true);
        assert_eq!(
            addrs.first(),
            Some(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "include_loopback should put 127.0.0.1 first"
        );
    }

    #[test]
    fn no_loopback_excludes_loopback_addrs() {
        // With loopback disabled, no returned address may be a loopback
        // address on any platform.
        let addrs = routable_local_addrs(false);
        assert!(
            addrs.iter().all(|ip| !ip.is_loopback()),
            "no-loopback result must not contain loopback addresses: {addrs:?}"
        );
    }

    #[test]
    fn returned_addrs_are_never_unspecified() {
        for include_loopback in [false, true] {
            let addrs = routable_local_addrs(include_loopback);
            assert!(
                addrs.iter().all(|ip| !ip.is_unspecified()),
                "0.0.0.0 / :: are not real bind targets: {addrs:?}"
            );
        }
    }

    #[test]
    fn host_label_prefers_human_label_over_ip_label() {
        let label = choose_host_label(
            vec!["192.168.64.61".into(), "vortex-deb-x11-intendant".into()],
            Some("fallback-host"),
        );
        assert_eq!(label, "vortex-deb-x11-intendant");
    }

    #[test]
    fn host_label_uses_hostname_before_stale_ip_label() {
        let label = choose_host_label(vec!["192.168.64.61".into()], Some("vortex-deb-x11"));
        assert_eq!(label, "vortex-deb-x11");
    }

    #[test]
    fn access_actions_parse_on_every_platform() {
        for (name, action) in [
            ("setup", AccessAction::Setup),
            ("recert", AccessAction::Recert),
            ("list", AccessAction::List),
            ("remove", AccessAction::Remove),
            ("serve-certs", AccessAction::ServeCerts),
        ] {
            let parsed = parse_args(&[name.to_string()]).unwrap();
            assert_eq!(parsed.action, action);
        }
    }

    #[test]
    fn windows_import_copy_uses_current_user_stores_without_echoing_password() {
        let instructions =
            windows_import_instructions(std::path::Path::new(r"C:\Users\O'Brien\Access"));
        assert!(instructions.contains(r"$certDir = 'C:\Users\O''Brien\Access'"));
        assert!(instructions.contains(r"'Cert:\CurrentUser\Root'"));
        assert!(instructions.contains(r"'Cert:\CurrentUser\My'"));
        assert!(instructions.contains("Get-Content -Raw -LiteralPath"));
        assert!(instructions.contains("ConvertTo-SecureString"));
        assert!(!instructions.contains("type p12_password"));
    }
}
