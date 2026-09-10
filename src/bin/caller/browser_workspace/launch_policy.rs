//! Native launch boundaries: seal owner authority before dotenv, carry it on
//! daemon succession, and keep navigation out of Chromium's switch parser.
//! Source labels are resolver provenance, not executable attestation.
use std::ffi::OsStr;
use std::process::Command;
use std::sync::OnceLock;

const HIDE_TESTING_NOTICE_ENV: &str = "INTENDANT_BROWSER_WORKSPACE_HIDE_TESTING_NOTICE";
static HIDE_TESTING_NOTICE: OnceLock<bool> = OnceLock::new();

pub(crate) fn initialize() -> Result<(), &'static str> {
    let enabled = parse(std::env::var_os(HIDE_TESTING_NOTICE_ENV).as_deref())?;
    HIDE_TESTING_NOTICE
        .set(enabled)
        .map_err(|_| "browser testing notice policy already initialized")
}

fn parse(raw: Option<&OsStr>) -> Result<bool, &'static str> {
    match raw {
        None => Ok(false),
        Some(value) if value == "0" => Ok(false),
        Some(value) if value == "1" => Ok(true),
        _ => Err("INTENDANT_BROWSER_WORKSPACE_HIDE_TESTING_NOTICE must be unset, 0, or 1"),
    }
}

pub(crate) fn current() -> bool {
    HIDE_TESTING_NOTICE.get().copied().unwrap_or(false)
}

pub(crate) fn preserve_for_successor(command: &mut Command) {
    // Always override inheritance, including the originally-unset false case.
    command.env(HIDE_TESTING_NOTICE_ENV, if current() { "1" } else { "0" });
}

pub(crate) fn apply_testing_notice_policy(command: &mut Command, source: &str, enabled: bool) {
    if enabled && source == "intendant-managed-cache" {
        // Real test mode, with other Chromium side effects; see configuration.md.
        command.arg("--test-type=gpu");
    }
}

pub(crate) fn navigation(raw: Option<&str>) -> Result<Option<&str>, &'static str> {
    let Some(raw) = raw else { return Ok(None) };
    // URL parsers discard some control characters: reject instead of forwarding
    // different bytes to Chromium. Preserve ordinary surrounding-space trimming.
    if raw.chars().any(char::is_control) {
        return Err("browser launch URL contains control characters");
    }
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.starts_with('-') || url::Url::parse(raw).is_err() {
        return Err("browser launch URL must be an absolute URL, not a switch");
    }
    Ok(Some(raw))
}

pub(crate) fn append_navigation(
    command: &mut Command,
    raw: Option<&str>,
) -> Result<(), &'static str> {
    let url = navigation(raw)?.unwrap_or("about:blank");
    command.arg("--").arg(url);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_values_are_strict() {
        assert!(!parse(None).unwrap());
        assert!(!parse(Some(OsStr::new("0"))).unwrap());
        assert!(parse(Some(OsStr::new("1"))).unwrap());
        for raw in ["", "true", "yes", "on", "2", " 1", "--no-sandbox"] {
            assert!(parse(Some(OsStr::new(raw))).is_err());
        }
    }

    // This same test is a subprocess fixture, so dotenv really mutates the
    // child's environment, never the test runner's or a real machine file.
    #[test]
    fn sealed_owner_survives_dotenv_and_multiple_successors() {
        const HOP: &str = "NOTICE_TEST_HOP";
        const OVERLAY: &str = "NOTICE_TEST_OVERLAY";
        const EXPECTED: &str = "NOTICE_TEST_EXPECTED";
        let test_name = std::thread::current().name().unwrap().to_owned();
        let child_command = || {
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args(["--exact", &test_name, "--nocapture"]);
            cmd
        };
        if let Ok(hop) = std::env::var(HOP) {
            let hop: usize = hop.parse().unwrap();
            initialize().unwrap();
            let expected = std::env::var(EXPECTED).unwrap() == "1";
            assert_eq!(current(), expected);
            let overlay = std::env::var(OVERLAY).unwrap();
            let dotenv = if overlay == "absent" {
                String::new()
            } else {
                format!("{HIDE_TESTING_NOTICE_ENV}={overlay}\n")
            };
            dotenvy::from_read(std::io::Cursor::new(dotenv)).unwrap();
            assert_eq!(current(), expected);
            if hop == 0 && !expected && overlay != "absent" {
                // For originally unset owner cases, the real dotenv load
                // fills the ambient value, reproducing the review finding.
                if std::env::var("NOTICE_TEST_UNSET").as_deref() == Ok("1") {
                    assert_eq!(std::env::var(HIDE_TESTING_NOTICE_ENV).unwrap(), overlay);
                }
            }
            if hop < 3 {
                let mut cmd = child_command();
                // Model any later ambient overlay as well, for explicit 0/1.
                cmd.env(HIDE_TESTING_NOTICE_ENV, &overlay);
                preserve_for_successor(&mut cmd);
                assert_eq!(
                    cmd.get_envs()
                        .find(|(k, _)| *k == HIDE_TESTING_NOTICE_ENV)
                        .unwrap()
                        .1,
                    Some(OsStr::new(if expected { "1" } else { "0" }))
                );
                assert!(cmd
                    .env(HOP, (hop + 1).to_string())
                    .status()
                    .unwrap()
                    .success());
            }
            return;
        }
        for owner in [None, Some("0"), Some("1")] {
            for overlay in ["absent", "1", "invalid", "0"] {
                let mut cmd = child_command();
                cmd.env_clear()
                    .env(HOP, "0")
                    .env(OVERLAY, overlay)
                    .env(EXPECTED, owner.unwrap_or("0"))
                    .env("NOTICE_TEST_UNSET", if owner.is_none() { "1" } else { "0" });
                if let Some(owner) = owner {
                    cmd.env(HIDE_TESTING_NOTICE_ENV, owner);
                }
                assert!(cmd.status().unwrap().success());
            }
        }
    }

    #[test]
    fn navigation_and_source_argv_boundary() {
        for source in [
            "intendant-managed-cache",
            "managed-cache",
            "env:INTENDANT_BROWSER_WORKSPACE_EXECUTABLE",
            "env:INTENDANT_BROWSER_EXECUTABLE",
            "system-browser",
            "system-browser-provider",
            "system-browser-env-opt-in",
        ] {
            for enabled in [false, true] {
                for raw in [
                    "--test-type=gpu",
                    " --no-sandbox ",
                    "-incognito",
                    "--",
                    "example.com",
                    "https://",
                    "http://[bad]",
                    "https://example.com/\n--test-type=gpu",
                    "\0",
                ] {
                    let mut cmd = Command::new("unused-browser");
                    apply_testing_notice_policy(&mut cmd, source, enabled);
                    let before: Vec<_> = cmd.get_args().map(|s| s.to_owned()).collect();
                    assert!(append_navigation(&mut cmd, Some(raw)).is_err());
                    assert_eq!(cmd.get_args().collect::<Vec<_>>(), before);
                }
                for raw in [
                    None,
                    Some(""),
                    Some("  "),
                    Some("about:blank"),
                    Some("https://example.com/a?q=hello%20world&switch=--test-type=gpu#x"),
                    Some("custom+app://open/item?x=1&y=2"),
                    Some("mailto:user@example.com"),
                    Some("file:///tmp/profile%20with%20spaces"),
                    Some("chrome://version/"),
                    Some(" https://example.com/ "),
                ] {
                    let mut cmd = Command::new("unused-browser");
                    cmd.arg("--user-data-dir=/test/profile with spaces");
                    apply_testing_notice_policy(&mut cmd, source, enabled);
                    append_navigation(&mut cmd, raw).unwrap();
                    let mut expected = vec!["--user-data-dir=/test/profile with spaces"];
                    if enabled && source == "intendant-managed-cache" {
                        expected.push("--test-type=gpu");
                    }
                    expected.extend([
                        "--",
                        raw.map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("about:blank"),
                    ]);
                    assert_eq!(cmd.get_args().collect::<Vec<_>>(), expected);
                }
            }
        }
    }
}
