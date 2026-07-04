//! `intendant service` — keep a daemon running unattended, on any OS.
//!
//! Each platform uses its NATIVE supervisor when one exists — systemd
//! where a Linux box runs it, launchd on macOS, Task Scheduler on
//! Windows — and falls back to cron `@reboot` on systemd-less Linux.
//! systemd is one detected backend among four, never a requirement.
//! Where the native mechanism supervises well (systemd, launchd) the
//! daemon is exec'd directly and its native restart/log capture is
//! used; where it is weak (Task Scheduler, cron) the entry point is
//! `intendant service run`, a small built-in supervisor that restarts
//! the daemon with backoff and appends its output to a log file — which
//! is also where the claim phrase lands on those backends.
//!
//! `install` always says where to watch for the claim phrase; that line
//! is the contract the install scripts and the landing advisor lean on.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

const SYSTEMD_UNIT_NAME: &str = "intendant.service";
const LAUNCHD_LABEL: &str = "dev.intendant.daemon";
const WINDOWS_TASK_NAME: &str = "Intendant Daemon";
const CRON_MARKER: &str = "# intendant-service-v1";
const LOG_ROTATE_BYTES: u64 = 32 * 1024 * 1024;
/// Restart backoff: quick first retries, capped so a crash-looping
/// daemon cannot melt a box; a run that survives this long resets it.
const BACKOFF_START_SECS: u64 = 3;
const BACKOFF_CAP_SECS: u64 = 60;
const BACKOFF_RESET_UPTIME_SECS: u64 = 300;

/// The environment a service definition must carry over: the rendezvous
/// wiring is how a booted daemon finds its way back to being claimable.
const CARRIED_ENV_KEYS: [&str; 3] = [
    "INTENDANT_CONNECT_RENDEZVOUS_URL",
    "INTENDANT_CONNECT_DAEMON_ID",
    "INTENDANT_CONNECT_TOKEN",
];

pub fn run_service_cli(args: &[String]) -> i32 {
    let action = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1.min(args.len())..];
    let outcome = match action {
        "install" => cli_install(rest),
        "uninstall" => cli_uninstall(),
        "status" => cli_status(),
        "run" => return cli_run(rest),
        _ => Err(concat!(
            "usage: intendant service <install|uninstall|status> …\n",
            "  install [--now] [-- <daemon args>]   install a boot service (native supervisor:\n",
            "                                       systemd / launchd / Task Scheduler / cron @reboot)\n",
            "  uninstall                            remove it and stop the daemon\n",
            "  status                               report what is installed and whether it runs\n",
            "  run --log <path> [--env K=V]… -- <daemon args>\n",
            "                                       (internal) the portable supervisor entry point"
        )
        .to_string()),
    };
    match outcome {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            2
        }
    }
}

/* ── Backend detection ── */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Clone, Copy)]
enum Backend {
    /// system == true installs the machine-wide unit (root); false the
    /// `--user` unit with lingering.
    Systemd { system: bool },
    Launchd,
    /// boot == true is the elevated path (S4U task at machine boot);
    /// false a logon-triggered task for unelevated installs.
    WindowsTask { boot: bool },
    CronReboot,
}

fn detect_backend() -> Result<Backend, String> {
    if cfg!(windows) {
        return Ok(Backend::WindowsTask {
            boot: windows_is_elevated(),
        });
    }
    if cfg!(target_os = "macos") {
        if crate::platform::is_root() {
            return Err(
                "run `intendant service install` as your own user on macOS — it installs a per-user LaunchAgent".to_string(),
            );
        }
        return Ok(Backend::Launchd);
    }
    // Linux and other Unix: systemd when it is genuinely PID 1 (the
    // directory exists only then), else cron @reboot + the built-in
    // supervisor. systemd is never assumed.
    if std::path::Path::new("/run/systemd/system").exists() && command_exists("systemctl") {
        return Ok(Backend::Systemd {
            system: crate::platform::is_root(),
        });
    }
    if command_exists("crontab") {
        return Ok(Backend::CronReboot);
    }
    Err(format!(
        "no supported service mechanism found (no systemd, no crontab). Run the supervisor under your init system of choice:\n  {} service run --log {} -- --no-tui …",
        current_exe_display(),
        default_log_path().display()
    ))
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn windows_is_elevated() -> bool {
    // `net session` succeeds only in an elevated shell — the standard
    // dependency-free probe.
    Command::new("net")
        .arg("session")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn windows_is_elevated() -> bool {
    false
}

/* ── Shared facts ── */

fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot resolve the intendant binary path: {e}"))
}

fn current_exe_display() -> String {
    current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "intendant".to_string())
}

fn default_log_path() -> PathBuf {
    crate::platform::home_dir()
        .join(".intendant")
        .join("logs")
        .join("service.log")
}

fn supervisor_pidfile() -> PathBuf {
    crate::platform::home_dir()
        .join(".intendant")
        .join("service-supervisor.pid")
}

fn carried_env(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    CARRIED_ENV_KEYS
        .iter()
        .filter_map(|key| {
            get(key)
                .filter(|value| !value.trim().is_empty())
                .map(|value| (key.to_string(), value))
        })
        .collect()
}

/* ── Quoting (each format has its own rules; get them right once) ── */

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// systemd ExecStart quoting: double quotes with `\`/`"` escaped, and
/// the two characters systemd itself expands (`$` specifier-expands
/// environment, `%` expands unit specifiers) doubled.
fn systemd_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("$$"),
            '%' => out.push_str("%%"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Windows command-line argument quoting (CommandLineToArgvW rules):
/// backslashes double only before a quote; quotes are backslash-escaped.
fn windows_arg_quote(value: &str) -> String {
    if !value.is_empty() && !value.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        return value.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for ch in value.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/* ── Definition generators (pure — unit-tested) ── */

fn systemd_unit(
    exe: &str,
    daemon_args: &[String],
    envs: &[(String, String)],
    home: &str,
    system: bool,
) -> String {
    let mut exec = systemd_quote(exe);
    for arg in daemon_args {
        exec.push(' ');
        exec.push_str(&systemd_quote(arg));
    }
    let env_lines: String = envs
        .iter()
        .map(|(k, v)| format!("Environment={}\n", systemd_quote(&format!("{k}={v}"))))
        .collect();
    let wanted_by = if system { "multi-user.target" } else { "default.target" };
    format!(
        "[Unit]\nDescription=Intendant daemon\nWants=network-online.target\nAfter=network-online.target\n\n[Service]\nExecStart={exec}\nWorkingDirectory={home}\n{env_lines}Restart=on-failure\nRestartSec={BACKOFF_START_SECS}\n\n[Install]\nWantedBy={wanted_by}\n"
    )
}

fn launchd_plist(
    exe: &str,
    daemon_args: &[String],
    envs: &[(String, String)],
    home: &str,
    log: &str,
) -> String {
    let mut program_args = format!("    <string>{}</string>\n", xml_escape(exe));
    for arg in daemon_args {
        program_args.push_str(&format!("    <string>{}</string>\n", xml_escape(arg)));
    }
    let env_dict = if envs.is_empty() {
        String::new()
    } else {
        let entries: String = envs
            .iter()
            .map(|(k, v)| {
                format!(
                    "    <key>{}</key>\n    <string>{}</string>\n",
                    xml_escape(k),
                    xml_escape(v)
                )
            })
            .collect();
        format!("  <key>EnvironmentVariables</key>\n  <dict>\n{entries}  </dict>\n")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{program_args}  </array>
{env_dict}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>WorkingDirectory</key>
  <string>{home}</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        home = xml_escape(home),
        log = xml_escape(log),
    )
}

/// Task Scheduler definition. Boot installs run at machine start as the
/// current user without a stored password (S4U — no interactive
/// desktop, which a headless daemon does not need); unelevated installs
/// get a logon-triggered task. Restart-on-failure is redundant with the
/// supervisor but harmless — it also revives the supervisor itself.
fn schtasks_xml(exe: &str, run_args: &[String], user_id: &str, boot: bool) -> String {
    let arguments = run_args
        .iter()
        .map(|a| windows_arg_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let (trigger, principal) = if boot {
        (
            "    <BootTrigger><Enabled>true</Enabled></BootTrigger>\n".to_string(),
            format!(
                "    <Principal id=\"Author\">\n      <UserId>{}</UserId>\n      <LogonType>S4U</LogonType>\n      <RunLevel>LeastPrivilege</RunLevel>\n    </Principal>\n",
                xml_escape(user_id)
            ),
        )
    } else {
        (
            format!(
                "    <LogonTrigger><Enabled>true</Enabled><UserId>{}</UserId></LogonTrigger>\n",
                xml_escape(user_id)
            ),
            format!(
                "    <Principal id=\"Author\">\n      <UserId>{}</UserId>\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>LeastPrivilege</RunLevel>\n    </Principal>\n",
                xml_escape(user_id)
            ),
        )
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
{trigger}  </Triggers>
  <Principals>
{principal}  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        command = xml_escape(exe),
        arguments = xml_escape(&arguments),
    )
}

fn cron_line(exe: &str, run_args: &[String]) -> String {
    let mut line = format!("@reboot {}", sh_quote(exe));
    for arg in run_args {
        line.push(' ');
        line.push_str(&sh_quote(arg));
    }
    format!("{line} {CRON_MARKER}")
}

/// The `service run` invocation a supervisor-backed definition points
/// at: log destination, carried env (Task Scheduler XML has no env
/// block, so the supervisor sets it), then the daemon args.
fn supervisor_run_args(
    log: &str,
    envs: &[(String, String)],
    daemon_args: &[String],
) -> Vec<String> {
    let mut args = vec!["service".to_string(), "run".to_string(), "--log".to_string(), log.to_string()];
    for (k, v) in envs {
        args.push("--env".to_string());
        args.push(format!("{k}={v}"));
    }
    args.push("--".to_string());
    args.extend(daemon_args.iter().cloned());
    args
}

/* ── install ── */

fn cli_install(rest: &[String]) -> Result<(), String> {
    let mut now = false;
    let mut daemon_args: Vec<String> = Vec::new();
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--now" => now = true,
            "--" => {
                daemon_args = iter.cloned().collect();
                break;
            }
            other => return Err(format!("unknown service install argument: {other}")),
        }
    }
    if daemon_args.is_empty() {
        daemon_args = vec!["--no-tui".to_string()];
    }

    let backend = detect_backend()?;
    let exe = current_exe()?.display().to_string();
    let home = crate::platform::home_dir();
    let home_str = home.display().to_string();
    let log = default_log_path();
    let log_str = log.display().to_string();
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create log directory {}: {e}", parent.display()))?;
    }
    let envs = carried_env(|key| std::env::var(key).ok());
    // Captured before any backend starts the daemon, so the first-boot
    // probe scans only log bytes this run produced (a reinstall over an
    // old crash log must not false-positive).
    let pre_log_len = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);

    match backend {
        Backend::Systemd { system } => {
            let unit = systemd_unit(&exe, &daemon_args, &envs, &home_str, system);
            let unit_path = if system {
                PathBuf::from("/etc/systemd/system").join(SYSTEMD_UNIT_NAME)
            } else {
                home.join(".config/systemd/user").join(SYSTEMD_UNIT_NAME)
            };
            write_private(&unit_path, &unit)?;
            let scope: &[&str] = if system { &[] } else { &["--user"] };
            run_ok("systemctl", &[scope, &["daemon-reload"]].concat())?;
            let enable: Vec<&str> = if now {
                [scope, &["enable", "--now", "intendant"]].concat()
            } else {
                [scope, &["enable", "intendant"]].concat()
            };
            run_ok("systemctl", &enable)?;
            if !system {
                // Without lingering, user units die at logout — the exact
                // thing a service install exists to prevent.
                let user = std::env::var("USER").unwrap_or_default();
                if !user.is_empty()
                    && run_ok("loginctl", &["enable-linger", &user]).is_err()
                {
                    println!(
                        "note: could not enable lingering; run 'sudo loginctl enable-linger {user}' or the daemon stops at logout."
                    );
                }
            }
            println!("installed systemd {} unit {}", if system { "system" } else { "user" }, unit_path.display());
            println!(
                "claim phrase / logs: journalctl {}-u intendant -f",
                if system { "" } else { "--user " }
            );
        }
        Backend::Launchd => {
            let plist = launchd_plist(&exe, &daemon_args, &envs, &home_str, &log_str);
            let plist_path = home
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist"));
            write_private(&plist_path, &plist)?;
            let target = format!("gui/{}", crate::platform::unix_uid());
            // Re-bootstrap after an uninstall/reinstall cycle: boot out any
            // stale registration first (ignore failure — usually not loaded).
            let _ = run_ok("launchctl", &["bootout", &format!("{target}/{LAUNCHD_LABEL}")]);
            let plist_str = plist_path.display().to_string();
            if run_ok("launchctl", &["bootstrap", &target, &plist_str]).is_err() {
                // Older macOS fallback.
                run_ok("launchctl", &["load", "-w", &plist_str])?;
            }
            if now {
                let _ = run_ok(
                    "launchctl",
                    &["kickstart", &format!("{target}/{LAUNCHD_LABEL}")],
                );
            }
            println!("installed LaunchAgent {}", plist_path.display());
            println!("claim phrase / logs: tail -f {log_str}");
        }
        Backend::WindowsTask { boot } => {
            let user_id = windows_user_id();
            let run_args = supervisor_run_args(&log_str, &envs, &daemon_args);
            let xml = schtasks_xml(&exe, &run_args, &user_id, boot);
            let xml_path = home.join(".intendant").join("service-task.xml");
            write_private(&xml_path, &xml)?;
            run_ok(
                "schtasks",
                &["/create", "/tn", WINDOWS_TASK_NAME, "/xml", &xml_path.display().to_string(), "/f"],
            )?;
            if now {
                run_ok("schtasks", &["/run", "/tn", WINDOWS_TASK_NAME])?;
            }
            println!(
                "installed scheduled task \"{WINDOWS_TASK_NAME}\" ({})",
                if boot {
                    "starts at boot"
                } else {
                    "starts at logon — rerun elevated for at-boot start"
                }
            );
            println!("claim phrase / logs: Get-Content -Wait {log_str}");
        }
        Backend::CronReboot => {
            let run_args = supervisor_run_args(&log_str, &envs, &daemon_args);
            let line = cron_line(&exe, &run_args);
            let existing = Command::new("crontab")
                .arg("-l")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let mut table: String = existing
                .lines()
                .filter(|l| !l.contains(CRON_MARKER))
                .map(|l| format!("{l}\n"))
                .collect();
            table.push_str(&line);
            table.push('\n');
            pipe_to("crontab", &["-"], &table)?;
            println!("installed cron @reboot entry (no systemd on this box; using the built-in supervisor)");
            if now {
                spawn_supervisor_detached(&exe, &run_args)?;
                println!("supervisor started");
            }
            println!("claim phrase / logs: tail -f {log_str}");
        }
    }
    if now {
        first_boot_probe(&backend, &log, pre_log_len)?;
    }
    Ok(())
}

/// How long after `--now` before requiring the started daemon to still be
/// alive. Long enough for an instant crash to be visible (systemd flips to
/// auto-restart immediately; the supervisor logs its restart line before
/// its first 3s backoff), short enough not to stall the installer.
const FIRST_BOOT_PROBE_DELAY_MS: u64 = 2500;

/// `install --now` used to report success — and print where the claim
/// phrase lands — while the daemon it had just started was crash-looping
/// on a first-boot misconfiguration; the user then tails a log that never
/// shows a claim phrase. After starting, wait briefly and fail loudly on
/// positive evidence of death: a failed/auto-restarting unit, a pid-less
/// LaunchAgent, or supervisor restart lines from this run. Absence of
/// evidence (a slow start) stays green — this must never flake a healthy
/// install.
fn first_boot_probe(backend: &Backend, log: &Path, pre_log_len: u64) -> Result<(), String> {
    std::thread::sleep(std::time::Duration::from_millis(FIRST_BOOT_PROBE_DELAY_MS));
    let failure = match backend {
        Backend::Systemd { system } => {
            let scope: &[&str] = if *system { &[] } else { &["--user"] };
            let show = run_capture(
                "systemctl",
                &[
                    scope,
                    &[
                        "show",
                        "-p",
                        "ActiveState",
                        "-p",
                        "SubState",
                        "-p",
                        "NRestarts",
                        "--value",
                        "intendant",
                    ],
                ]
                .concat(),
            );
            let mut lines = show.lines();
            let active = lines.next().unwrap_or("").trim().to_string();
            let sub = lines.next().unwrap_or("").trim().to_string();
            let restarts: u32 = lines.next().unwrap_or("").trim().parse().unwrap_or(0);
            if systemd_first_boot_failed(&active, &sub, restarts) {
                let tail = run_capture(
                    "journalctl",
                    &[scope, &["-u", "intendant", "-n", "8", "--no-pager", "-o", "cat"]].concat(),
                );
                Some(format!(
                    "unit is {active} ({sub}) after {restarts} restart(s); last log lines:\n{}",
                    tail.trim_end()
                ))
            } else {
                None
            }
        }
        Backend::Launchd => {
            let target = format!("gui/{}/{}", crate::platform::unix_uid(), LAUNCHD_LABEL);
            let out = run_capture("launchctl", &["print", &target]);
            if out.contains("pid = ") {
                None
            } else {
                Some(format!(
                    "LaunchAgent has no running pid; last log lines:\n{}",
                    log_tail_since(log, pre_log_len)
                ))
            }
        }
        Backend::WindowsTask { .. } | Backend::CronReboot => {
            let fresh = log_tail_since(log, pre_log_len);
            if supervisor_first_boot_failed(&fresh) {
                Some(format!(
                    "the daemon exited on first boot; supervisor log:\n{fresh}"
                ))
            } else {
                None
            }
        }
    };
    match failure {
        Some(detail) => Err(format!(
            "service installed, but the daemon did not stay up — {detail}\n\
             The service stays installed: fix the cause and the supervisor restarts it \
             (or rerun `intendant service install --now`)."
        )),
        None => {
            println!("daemon is up (first-boot check passed)");
            Ok(())
        }
    }
}

/// Positive-evidence verdict for the systemd backend. "activating" with a
/// clean SubState and zero restarts is a slow start, not a failure; a
/// currently-active unit is healthy even if it needed a restart to get
/// there — the supervisor did its job.
fn systemd_first_boot_failed(active: &str, sub: &str, restarts: u32) -> bool {
    if active == "active" {
        return false;
    }
    active == "failed" || sub == "auto-restart" || restarts > 0
}

/// The built-in supervisor's log is the only first-boot signal for the
/// schtasks/cron backends; only lines from this run count (see
/// `pre_log_len`). Matches the two lines `service run` writes when the
/// child dies or cannot spawn.
fn supervisor_first_boot_failed(fresh_log: &str) -> bool {
    fresh_log.contains("restarting in") || fresh_log.contains("spawn failed")
}

/// Last few log lines written after `offset` (a size captured before the
/// service started). Falls back to the whole file when it was rotated or
/// truncated underneath the offset.
fn log_tail_since(log: &Path, offset: u64) -> String {
    let bytes = std::fs::read(log).unwrap_or_default();
    let start = if (offset as usize) <= bytes.len() {
        offset as usize
    } else {
        0
    };
    let text = String::from_utf8_lossy(&bytes[start..]);
    let lines: Vec<&str> = text.lines().collect();
    let keep = lines.len().saturating_sub(10);
    lines[keep..].join("\n")
}

fn windows_user_id() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.trim().is_empty() => format!("{domain}\\{user}"),
        _ => user,
    }
}

fn write_private(path: &std::path::Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
    // The definition may carry the connect token; keep it owner-only
    // where the OS can express that.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Run a command and return its stdout (lossy) regardless of exit status —
/// probes like `systemctl show` speak through stdout while exiting nonzero
/// for non-active units.
fn run_capture(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn run_ok(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{program} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn pipe_to(program: &str, args: &[&str], stdin: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| format!("{program}: no stdin"))?
        .write_all(stdin.as_bytes())
        .map_err(|e| format!("{program}: {e}"))?;
    let status = child.wait().map_err(|e| format!("{program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {} failed", args.join(" ")))
    }
}

fn spawn_supervisor_detached(exe: &str, run_args: &[String]) -> Result<(), String> {
    let mut cmd = Command::new(exe);
    cmd.args(run_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(|e| format!("start supervisor: {e}"))?;
    Ok(())
}

/* ── uninstall / status ── */

fn cli_uninstall() -> Result<(), String> {
    let home = crate::platform::home_dir();
    let mut found = false;

    let system_unit = PathBuf::from("/etc/systemd/system").join(SYSTEMD_UNIT_NAME);
    let user_unit = home.join(".config/systemd/user").join(SYSTEMD_UNIT_NAME);
    for (unit_path, scope) in [(system_unit, None), (user_unit, Some("--user"))] {
        if !unit_path.exists() {
            continue;
        }
        found = true;
        let scope_args: &[&str] = match scope {
            Some(s) => &[s],
            None => &[],
        };
        let _ = run_ok("systemctl", &[scope_args, &["disable", "--now", "intendant"]].concat());
        let _ = std::fs::remove_file(&unit_path);
        let _ = run_ok("systemctl", &[scope_args, &["daemon-reload"]].concat());
        println!("removed {}", unit_path.display());
    }

    let plist_path = home
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    if plist_path.exists() {
        found = true;
        #[cfg(unix)]
        {
            let target = format!("gui/{}/{}", crate::platform::unix_uid(), LAUNCHD_LABEL);
            let _ = run_ok("launchctl", &["bootout", &target]);
        }
        let _ = std::fs::remove_file(&plist_path);
        println!("removed {}", plist_path.display());
    }

    if cfg!(windows) {
        if run_ok("schtasks", &["/query", "/tn", WINDOWS_TASK_NAME]).is_ok() {
            found = true;
            let _ = run_ok("schtasks", &["/end", "/tn", WINDOWS_TASK_NAME]);
            run_ok("schtasks", &["/delete", "/tn", WINDOWS_TASK_NAME, "/f"])?;
            println!("removed scheduled task \"{WINDOWS_TASK_NAME}\"");
            stop_supervisor_by_pidfile();
        }
    } else if command_exists("crontab") {
        let existing = Command::new("crontab")
            .arg("-l")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        if existing.contains(CRON_MARKER) {
            found = true;
            let table: String = existing
                .lines()
                .filter(|l| !l.contains(CRON_MARKER))
                .map(|l| format!("{l}\n"))
                .collect();
            pipe_to("crontab", &["-"], &table)?;
            println!("removed cron @reboot entry");
            stop_supervisor_by_pidfile();
        }
    }

    if found {
        Ok(())
    } else {
        Err("nothing installed by `intendant service install` was found".to_string())
    }
}

fn stop_supervisor_by_pidfile() {
    let pidfile = supervisor_pidfile();
    let Some(pid) = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return;
    };
    #[cfg(unix)]
    crate::platform::kill_process_group(pid);
    #[cfg(windows)]
    {
        let _ = run_ok("taskkill", &["/pid", &pid.to_string(), "/t", "/f"]);
    }
    let _ = std::fs::remove_file(&pidfile);
    println!("stopped supervisor (pid {pid})");
}

fn cli_status() -> Result<(), String> {
    let home = crate::platform::home_dir();
    let mut found = false;

    for (unit_path, scope) in [
        (PathBuf::from("/etc/systemd/system").join(SYSTEMD_UNIT_NAME), None),
        (home.join(".config/systemd/user").join(SYSTEMD_UNIT_NAME), Some("--user")),
    ] {
        if !unit_path.exists() {
            continue;
        }
        found = true;
        let scope_args: &[&str] = match scope {
            Some(s) => &[s],
            None => &[],
        };
        let active = run_ok("systemctl", &[scope_args, &["is-active", "--quiet", "intendant"]].concat()).is_ok();
        println!(
            "systemd unit {} — {}",
            unit_path.display(),
            if active { "active" } else { "inactive" }
        );
    }

    let plist_path = home
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    if plist_path.exists() {
        found = true;
        #[cfg(unix)]
        {
            let target = format!("gui/{}/{}", crate::platform::unix_uid(), LAUNCHD_LABEL);
            let active = run_ok("launchctl", &["print", &target]).is_ok();
            println!(
                "LaunchAgent {} — {}",
                plist_path.display(),
                if active { "loaded" } else { "not loaded" }
            );
        }
    }

    if cfg!(windows) && run_ok("schtasks", &["/query", "/tn", WINDOWS_TASK_NAME]).is_ok() {
        found = true;
        println!("scheduled task \"{WINDOWS_TASK_NAME}\" — installed");
    }

    if !cfg!(windows) && command_exists("crontab") {
        let existing = Command::new("crontab")
            .arg("-l")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        if existing.contains(CRON_MARKER) {
            found = true;
            let alive = std::fs::read_to_string(supervisor_pidfile())
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(crate::platform::process_alive)
                .unwrap_or(false);
            println!(
                "cron @reboot entry — supervisor {}",
                if alive { "running" } else { "not running" }
            );
        }
    }

    if found {
        println!("log: {}", default_log_path().display());
        Ok(())
    } else {
        Err("nothing installed by `intendant service install` was found".to_string())
    }
}

/* ── The portable supervisor (`service run`) ── */

fn next_backoff(previous_secs: u64, child_uptime_secs: u64) -> u64 {
    if child_uptime_secs >= BACKOFF_RESET_UPTIME_SECS {
        BACKOFF_START_SECS
    } else {
        (previous_secs * 2).min(BACKOFF_CAP_SECS)
    }
}

fn cli_run(rest: &[String]) -> i32 {
    let mut log_path: Option<PathBuf> = None;
    let mut envs: Vec<(String, String)> = Vec::new();
    let mut daemon_args: Vec<String> = Vec::new();
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--log" => match iter.next() {
                Some(path) => log_path = Some(PathBuf::from(path)),
                None => {
                    eprintln!("error: --log requires a path");
                    return 2;
                }
            },
            "--env" => match iter.next().and_then(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))) {
                Some(pair) => envs.push(pair),
                None => {
                    eprintln!("error: --env requires KEY=VALUE");
                    return 2;
                }
            },
            "--" => {
                daemon_args = iter.cloned().collect();
                break;
            }
            other => {
                eprintln!("error: unknown service run argument: {other}");
                return 2;
            }
        }
    }
    let log_path = log_path.unwrap_or_else(default_log_path);
    if daemon_args.is_empty() {
        daemon_args = vec!["--no-tui".to_string()];
    }
    let exe = match current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    crate::platform::detach_for_supervision();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let pidfile = supervisor_pidfile();
    if let Some(parent) = pidfile.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&pidfile, format!("{}\n", std::process::id()));

    let mut backoff = BACKOFF_START_SECS;
    loop {
        // Rotate at spawn boundaries so one file handle serves a whole
        // daemon lifetime.
        if std::fs::metadata(&log_path).map(|m| m.len() > LOG_ROTATE_BYTES).unwrap_or(false) {
            let _ = std::fs::rename(&log_path, log_path.with_extension("log.old"));
        }
        let log_file = match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(file) => file,
            Err(error) => {
                eprintln!("error: open {}: {error}", log_path.display());
                return 1;
            }
        };
        let log_line = |file: &std::fs::File, message: &str| {
            let mut file = file;
            // One pre-formatted write_all: write_fmt would issue several
            // small writes on an unbuffered File and the daemon's own
            // output (same fd) tears the line apart mid-timestamp.
            let line = format!(
                "[{} supervisor] {message}\n",
                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
            );
            let _ = file.write_all(line.as_bytes());
        };
        let (Ok(out), Ok(err)) = (log_file.try_clone(), log_file.try_clone()) else {
            eprintln!("error: cannot clone log handle");
            return 1;
        };
        let started = std::time::Instant::now();
        let mut command = Command::new(&exe);
        command
            .args(&daemon_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err));
        for (key, value) in &envs {
            command.env(key, value);
        }
        match command.spawn() {
            Ok(mut child) => {
                log_line(&log_file, &format!("daemon started (pid {})", child.id()));
                let status = child.wait();
                let uptime = started.elapsed().as_secs();
                backoff = next_backoff(backoff, uptime);
                log_line(
                    &log_file,
                    &format!(
                        "daemon exited ({}) after {uptime}s — restarting in {backoff}s",
                        status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string())
                    ),
                );
            }
            Err(error) => {
                backoff = next_backoff(backoff, 0);
                log_line(&log_file, &format!("spawn failed: {error} — retrying in {backoff}s"));
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(backoff));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn systemd_first_boot_verdict_requires_positive_evidence() {
        // Healthy steady state.
        assert!(!systemd_first_boot_failed("active", "running", 0));
        // Recovered after one crash: currently up — the supervisor did
        // its job; the install must not be declared failed.
        assert!(!systemd_first_boot_failed("active", "running", 1));
        // Slow start: activating without any crash signal stays green.
        assert!(!systemd_first_boot_failed("activating", "start", 0));
        // The crash-loop shapes the probe exists for (the fresh-box mTLS
        // boot loop showed exactly activating/auto-restart).
        assert!(systemd_first_boot_failed("activating", "auto-restart", 0));
        assert!(systemd_first_boot_failed("activating", "auto-restart", 3));
        assert!(systemd_first_boot_failed("failed", "failed", 0));
        assert!(systemd_first_boot_failed("inactive", "dead", 1));
    }

    #[test]
    fn supervisor_first_boot_verdict_matches_run_loop_lines() {
        // Shapes `cli_run` actually writes.
        assert!(supervisor_first_boot_failed(
            "[2026-07-04T10:10:10Z] daemon exited (status 1) after 0s — restarting in 3s"
        ));
        assert!(supervisor_first_boot_failed(
            "[2026-07-04T10:10:10Z] spawn failed: No such file — retrying in 3s"
        ));
        assert!(!supervisor_first_boot_failed(
            "[2026-07-04T10:10:10Z] daemon started (pid 4242)"
        ));
        assert!(!supervisor_first_boot_failed(""));
    }

    #[test]
    fn log_tail_since_reads_only_this_run() {
        let dir = std::env::temp_dir().join(format!("intendant-svc-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("svc.log");
        std::fs::write(&log, "old crash — restarting in 3s\n").unwrap();
        let pre = std::fs::metadata(&log).unwrap().len();
        // Nothing new after the offset: an old crash log must not count.
        assert_eq!(log_tail_since(&log, pre), "");
        let mut all = std::fs::read(&log).unwrap();
        all.extend_from_slice(b"daemon started (pid 7)\n");
        std::fs::write(&log, &all).unwrap();
        assert_eq!(log_tail_since(&log, pre), "daemon started (pid 7)");
        // Rotated/truncated underneath the offset: fall back to the file.
        std::fs::write(&log, b"fresh line\n").unwrap();
        assert_eq!(log_tail_since(&log, pre), "fresh line");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn systemd_unit_quotes_and_targets_correctly() {
        let unit = systemd_unit(
            "/opt/intendant/target/release/intendant",
            &args(&["--no-tui", "--owner", "fp with space", "50%$x"]),
            &[("INTENDANT_CONNECT_TOKEN".to_string(), "tok\"quote".to_string())],
            "/home/box",
            true,
        );
        assert!(unit.contains(
            r#"ExecStart="/opt/intendant/target/release/intendant" "--no-tui" "--owner" "fp with space" "50%%$$x""#
        ));
        assert!(unit.contains(r#"Environment="INTENDANT_CONNECT_TOKEN=tok\"quote""#));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        let user_unit = systemd_unit("/x", &args(&["--no-tui"]), &[], "/home/box", false);
        assert!(user_unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_plist_escapes_and_keeps_alive() {
        let plist = launchd_plist(
            "/Users/ada/intendant/target/release/intendant",
            &args(&["--no-tui", "--owner", "a<b&c"]),
            &[("INTENDANT_CONNECT_DAEMON_ID".to_string(), "box<1>".to_string())],
            "/Users/ada",
            "/Users/ada/.intendant/logs/service.log",
        );
        assert!(plist.contains("<string>a&lt;b&amp;c</string>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(plist.contains("<string>box&lt;1&gt;</string>"));
        assert!(plist.contains("<key>StandardOutPath</key>"));
        assert!(plist.contains(LAUNCHD_LABEL));
        // No env -> no empty dict.
        let bare = launchd_plist("/x", &args(&["--no-tui"]), &[], "/Users/ada", "/tmp/l.log");
        assert!(!bare.contains("EnvironmentVariables"));
    }

    #[test]
    fn schtasks_xml_boot_vs_logon_and_escaping() {
        let run_args = supervisor_run_args(
            r"C:\Users\ada\.intendant\logs\service.log",
            &[("INTENDANT_CONNECT_TOKEN".to_string(), "t&t".to_string())],
            &args(&["--no-tui", "--owner", "fp with space"]),
        );
        let boot = schtasks_xml(r"C:\intendant\intendant.exe", &run_args, r"BOX\ada", true);
        assert!(boot.contains("<BootTrigger>"));
        assert!(boot.contains("<LogonType>S4U</LogonType>"));
        assert!(boot.contains(r"<UserId>BOX\ada</UserId>"));
        assert!(boot.contains("<RestartOnFailure>"));
        // The supervisor invocation carries env + the quoted daemon args;
        // & is XML-escaped inside the Arguments element.
        assert!(boot.contains("service run --log"));
        assert!(boot.contains("--env INTENDANT_CONNECT_TOKEN=t&amp;t"));
        assert!(boot.contains("&quot;fp with space&quot;"));
        let logon = schtasks_xml(r"C:\intendant\intendant.exe", &run_args, r"BOX\ada", false);
        assert!(logon.contains("<LogonTrigger>"));
        assert!(logon.contains("<LogonType>InteractiveToken</LogonType>"));
    }

    #[test]
    fn cron_line_is_shell_safe_and_marked() {
        let run_args = supervisor_run_args(
            "/home/a b/.intendant/logs/service.log",
            &[],
            &args(&["--no-tui", "--owner", "o'brien"]),
        );
        let line = cron_line("/home/a b/intendant", &run_args);
        assert!(line.starts_with("@reboot '/home/a b/intendant' 'service' 'run'"));
        assert!(line.contains(r"'o'\''brien'"));
        assert!(line.ends_with(CRON_MARKER));
    }

    #[test]
    fn windows_arg_quoting_follows_argv_rules() {
        assert_eq!(windows_arg_quote("plain"), "plain");
        assert_eq!(windows_arg_quote("with space"), "\"with space\"");
        assert_eq!(windows_arg_quote(r#"say "hi""#), r#""say \"hi\"""#);
        // Trailing backslash before the closing quote must double.
        assert_eq!(windows_arg_quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(windows_arg_quote(""), "\"\"");
    }

    #[test]
    fn backoff_doubles_caps_and_resets() {
        assert_eq!(next_backoff(3, 0), 6);
        assert_eq!(next_backoff(6, 10), 12);
        assert_eq!(next_backoff(48, 0), 60);
        assert_eq!(next_backoff(60, 0), 60);
        // A long healthy run resets the ladder.
        assert_eq!(next_backoff(60, BACKOFF_RESET_UPTIME_SECS), BACKOFF_START_SECS);
    }

    #[test]
    fn carried_env_takes_only_set_connect_keys() {
        let envs = carried_env(|key| match key {
            "INTENDANT_CONNECT_RENDEZVOUS_URL" => Some("https://r.example".to_string()),
            "INTENDANT_CONNECT_DAEMON_ID" => Some("  ".to_string()), // blank -> skipped
            _ => None,
        });
        assert_eq!(
            envs,
            vec![(
                "INTENDANT_CONNECT_RENDEZVOUS_URL".to_string(),
                "https://r.example".to_string()
            )]
        );
    }
}
