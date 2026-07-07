use std::path::{Path, PathBuf};

/// Escape a path for a double-quoted Seatbelt profile string literal.
/// Paths that cannot be represented safely (non-UTF-8 or control bytes)
/// are refused — the caller fails loudly rather than producing a profile
/// that means something else.
#[cfg(target_os = "macos")]
pub(crate) fn seatbelt_path_literal(path: &Path) -> Result<String, String> {
    let Some(text) = path.to_str() else {
        return Err(format!(
            "sandbox path {} is not valid UTF-8",
            path.display()
        ));
    };
    if text.chars().any(|c| c.is_control()) {
        return Err(format!("sandbox path {text:?} contains control characters"));
    }
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Seatbelt deny rules for the user-secret directories the runtime's
/// `validate_path` denylist protects (`~/.ssh`, `~/.gnupg`). The denylist
/// only guards structured tool arguments (editFile/inspectPath) — command
/// strings run by executeCommand bypass it entirely, and no string
/// inspection can close that honestly. This clause closes it at the OS
/// level: appended LAST to a profile it wins over every allow
/// (last-match-wins), so secrets stay denied even when a write path or
/// `(allow default)` would otherwise cover them. `/proc`, `/sys`, and
/// `/etc/shadow` from the denylist are Linux paths with no macOS
/// counterpart, and `/dev` cannot be blanket-denied (every process needs
/// its tty and /dev/null). Returns an empty clause when there is no
/// resolvable home directory — nothing exists to protect.
///
/// Linux has no equivalent: Landlock is allowlist-only and cannot
/// subtract read access from a granted tree, so there the denylist on
/// structured tools plus the write sandbox remain the whole story (see
/// docs/src/architecture.md).
#[cfg(target_os = "macos")]
pub(crate) fn seatbelt_sensitive_deny_clause() -> Result<String, String> {
    let Some(home) = dirs::home_dir() else {
        return Ok(String::new());
    };
    let home = std::fs::canonicalize(&home).unwrap_or(home);
    let literals = [home.join(".ssh"), home.join(".gnupg")]
        .iter()
        .map(|path| seatbelt_path_literal(path).map(|literal| format!("(subpath {literal})")))
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    Ok(format!("(deny file-read* file-write* {literals})\n"))
}

/// The always-on macOS profile for the agent runtime when no write
/// sandbox is configured: everything allowed except the user-secret
/// directories. This is the executeCommand twin of `validate_path` —
/// policy the structured tools already enforce, extended to the whole
/// process tree.
#[cfg(target_os = "macos")]
pub(crate) fn seatbelt_sensitive_only_profile() -> Result<String, String> {
    Ok(format!(
        "(version 1)\n\
         (allow default)\n\
         {}",
        seatbelt_sensitive_deny_clause()?
    ))
}

/// Configuration for Landlock filesystem sandboxing.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SandboxConfig {
    /// Paths the sandboxed process may read.
    pub read_paths: Vec<PathBuf>,
    /// Paths the sandboxed process may write (implies read).
    pub write_paths: Vec<PathBuf>,
    /// Whether sandboxing is enabled.
    pub enabled: bool,
}

#[allow(dead_code)]
impl SandboxConfig {
    /// Build a default config for the given project.
    /// - Read: `/` (everything)
    /// - Write: project root, the OS scratch dir, log directory, home `.intendant`
    pub fn default_for_project(project_root: &Path, log_dir: &Path) -> Self {
        // The scratch dir stays the literal `/tmp` on Unix (macOS `TMPDIR`
        // aliases are composed into the Seatbelt profile separately); on
        // Windows the equivalent is the profile-local `%TEMP%`, which has
        // no fixed root.
        let scratch = if cfg!(windows) {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        };
        let mut write_paths = vec![project_root.to_path_buf(), scratch, log_dir.to_path_buf()];

        // Allow writes to the daemon state root (~/.intendant by default,
        // $INTENDANT_HOME when overridden).
        write_paths.push(crate::platform::intendant_home());

        Self {
            read_paths: vec![PathBuf::from("/")],
            write_paths,
            enabled: true,
        }
    }

    /// Build a maximally restrictive config for untrusted live audio agents.
    /// - Read: `/` (for shared libraries, system config)
    /// - Write: ONLY the session log dir and quarantine dir
    /// - No project root, no /tmp, no ~/.intendant
    ///
    /// Note: currently for documentation/future use. In-process live audio
    /// tasks use code-level isolation (zero tools, restricted write paths)
    /// rather than process-level Landlock.
    pub fn untrusted_live_audio(session_log_dir: &Path, quarantine_dir: &Path) -> Self {
        Self {
            read_paths: vec![PathBuf::from("/")],
            write_paths: vec![session_log_dir.to_path_buf(), quarantine_dir.to_path_buf()],
            enabled: true,
        }
    }

    /// Generate a Seatbelt (sandbox-exec) profile mirroring this config's
    /// Landlock posture on macOS: reads stay open (`read_paths` is `/` for
    /// the agent runtime), writes are denied everywhere except
    /// `write_paths` plus the scratch locations every Unix process assumes
    /// (`/dev` tty nodes, `/tmp`, `/var/tmp`, the per-user `TMPDIR`).
    /// Seatbelt rules are last-match-wins and evaluate REAL paths, so
    /// write paths are canonicalized first — a rule on a symlinked root
    /// (`/tmp`, `/var`, `/etc`) would otherwise never match.
    #[cfg(target_os = "macos")]
    pub fn seatbelt_write_only_profile(&self) -> Result<String, String> {
        let mut write_literals: Vec<String> = Vec::new();
        for path in ["/dev", "/private/tmp", "/private/var/tmp"] {
            write_literals.push(seatbelt_path_literal(Path::new(path))?);
        }
        if let Ok(tmpdir) = std::env::var("TMPDIR") {
            let canonical =
                std::fs::canonicalize(&tmpdir).unwrap_or_else(|_| PathBuf::from(&tmpdir));
            write_literals.push(seatbelt_path_literal(&canonical)?);
        }
        for path in &self.write_paths {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            write_literals.push(seatbelt_path_literal(&canonical)?);
        }
        let subpaths = write_literals
            .iter()
            .map(|literal| format!("(subpath {literal})"))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(format!(
            "(version 1)\n\
             (allow default)\n\
             (deny file-write*)\n\
             (allow file-write* {subpaths})\n\
             {sensitive}",
            sensitive = seatbelt_sensitive_deny_clause()?,
        ))
    }

    /// Apply Landlock restrictions to the current process.
    /// Returns Ok(true) if restrictions were applied, Ok(false) if Landlock
    /// is not supported by the kernel, Err on actual errors.
    pub fn apply_to_current_process(&self) -> Result<bool, String> {
        if !self.enabled {
            return Ok(false);
        }

        #[cfg(target_os = "linux")]
        {
            use landlock::{
                AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
            };

            let abi = ABI::V5;

            let read_access = AccessFs::from_read(abi);
            let write_access = AccessFs::from_read(abi) | AccessFs::from_write(abi);

            let mut ruleset_created = Ruleset::default()
                .handle_access(write_access)
                .map_err(|e| format!("Landlock ruleset creation failed: {}", e))?
                .create()
                .map_err(|e| format!("Landlock ruleset create failed: {}", e))?;

            // Add read-only paths
            for path in &self.read_paths {
                if path.exists() {
                    if let Ok(fd) = PathFd::new(path) {
                        let rule = PathBeneath::new(fd, read_access);
                        ruleset_created = ruleset_created
                            .add_rule(rule)
                            .map_err(|e| format!("Landlock add read rule failed: {}", e))?;
                    }
                }
            }

            // Add read-write paths
            for path in &self.write_paths {
                if path.exists() {
                    if let Ok(fd) = PathFd::new(path) {
                        let rule = PathBeneath::new(fd, write_access);
                        ruleset_created = ruleset_created
                            .add_rule(rule)
                            .map_err(|e| format!("Landlock add write rule failed: {}", e))?;
                    }
                }
            }

            let status = ruleset_created
                .restrict_self()
                .map_err(|e| format!("Landlock restrict_self failed: {}", e))?;

            Ok(status.ruleset != landlock::RulesetStatus::NotEnforced)
        }

        #[cfg(not(target_os = "linux"))]
        {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_write_only_profile_embeds_canonical_write_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config = SandboxConfig {
            read_paths: vec![PathBuf::from("/")],
            write_paths: vec![project.clone()],
            enabled: true,
        };
        let profile = config.seatbelt_write_only_profile().unwrap();
        assert!(profile.contains("(allow default)"));
        assert!(profile.contains("(deny file-write*)"));
        // TempDir lives under the /var/folders symlink; the profile must
        // carry the real /private/var path or the rule would never match.
        let canonical = std::fs::canonicalize(&project).unwrap();
        assert!(
            profile.contains(&format!("(subpath \"{}\")", canonical.display())),
            "profile missing canonicalized project path: {profile}"
        );
        assert!(profile.contains("(subpath \"/dev\")"));
    }

    /// Run the generated profile through the real Seatbelt compiler and
    /// kernel: writes inside the configured path succeed, writes outside
    /// are denied, reads stay open — the Linux Landlock posture.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_write_only_profile_enforces_like_landlock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let allowed = tmp.path().join("allowed");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let config = SandboxConfig {
            read_paths: vec![PathBuf::from("/")],
            write_paths: vec![allowed.clone()],
            enabled: true,
        };
        // TMPDIR is allowed wholesale in the profile (runtime scratch), and
        // TempDir lives under it — probe with TMPDIR pointed elsewhere so
        // the `outside` write exercises the deny rule.
        let profile = {
            let saved = std::env::var("TMPDIR").ok();
            std::env::remove_var("TMPDIR");
            let profile = config.seatbelt_write_only_profile().unwrap();
            if let Some(saved) = saved {
                std::env::set_var("TMPDIR", saved);
            }
            profile
        };
        let script = format!(
            "echo in > {allowed}/probe.txt && echo WRITE_IN_OK; \
             echo out > {outside}/probe.txt 2>/dev/null || echo WRITE_OUT_DENIED; \
             head -c 1 /etc/hosts > /dev/null && echo READ_OK",
            allowed = allowed.display(),
            outside = outside.display(),
        );
        let output = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sandbox-exec runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("WRITE_IN_OK"), "{stdout} / {profile}");
        assert!(stdout.contains("WRITE_OUT_DENIED"), "{stdout}");
        assert!(stdout.contains("READ_OK"), "{stdout}");
        assert!(!outside.join("probe.txt").exists());
    }

    /// The always-on macOS runtime profile: user-secret directories are
    /// denied to the whole process tree — including plain shell commands,
    /// the executeCommand lane validate_path never sees — while normal
    /// reads and writes stay open. Probed through the real kernel.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_sensitive_only_profile_blocks_secret_dirs_for_exec() {
        let home = dirs::home_dir().expect("home dir");
        if !home.join(".ssh").exists() {
            eprintln!("skipping: no ~/.ssh on this machine");
            return;
        }
        let profile = seatbelt_sensitive_only_profile().unwrap();
        assert!(profile.contains("(allow default)"));
        let tmp = tempfile::TempDir::new().unwrap();
        let script = format!(
            "ls {ssh} 2>/dev/null && echo SSH_LISTED || echo SSH_DENIED; \
             echo w > {tmp}/w.txt && echo WRITE_OK; \
             head -c 1 /etc/hosts > /dev/null && echo READ_OK",
            ssh = home.join(".ssh").display(),
            tmp = tmp.path().display(),
        );
        let output = std::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sandbox-exec runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("SSH_DENIED"), "{stdout} / {profile}");
        assert!(stdout.contains("WRITE_OK"), "{stdout}");
        assert!(stdout.contains("READ_OK"), "{stdout}");
    }

    /// The write-only profile carries the sensitive clause appended last,
    /// so ~/.ssh stays denied even when a configured write path (e.g. a
    /// project rooted at $HOME) would otherwise cover it.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_write_only_profile_keeps_secrets_denied_inside_write_paths() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let config = SandboxConfig {
            read_paths: vec![PathBuf::from("/")],
            write_paths: vec![home.clone()],
            enabled: true,
        };
        let profile = config.seatbelt_write_only_profile().unwrap();
        let canonical_home = std::fs::canonicalize(&home).unwrap_or(home);
        let deny_line = profile
            .lines()
            .find(|line| line.starts_with("(deny file-read* file-write*"))
            .expect("sensitive deny clause present");
        assert!(deny_line.contains(&format!("{}/.ssh", canonical_home.display())));
        // Appended last: the deny clause is the final rule, after the
        // write-path allow that covers $HOME.
        assert_eq!(profile.trim_end().lines().last().unwrap(), deny_line);
    }

    #[test]
    fn default_config_includes_project_and_tmp() {
        let config = SandboxConfig::default_for_project(
            Path::new("/home/user/project"),
            Path::new("/tmp/logs"),
        );
        assert!(config.enabled);
        assert!(config
            .write_paths
            .contains(&PathBuf::from("/home/user/project")));
        let scratch = if cfg!(windows) {
            std::env::temp_dir()
        } else {
            PathBuf::from("/tmp")
        };
        assert!(config.write_paths.contains(&scratch));
        assert!(config.write_paths.contains(&PathBuf::from("/tmp/logs")));
        assert!(config.read_paths.contains(&PathBuf::from("/")));
    }

    #[test]
    fn disabled_config_skips_apply() {
        let mut config =
            SandboxConfig::default_for_project(Path::new("/tmp/test"), Path::new("/tmp/logs"));
        config.enabled = false;
        assert_eq!(config.apply_to_current_process().unwrap(), false);
    }

    #[test]
    fn config_has_write_paths() {
        let config = SandboxConfig::default_for_project(
            Path::new("/home/user/myproject"),
            Path::new("/var/log/intendant"),
        );
        assert!(config.write_paths.len() >= 3);
    }
}
