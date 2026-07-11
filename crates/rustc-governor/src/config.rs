//! Governor configuration: a tiny reader for the machine-wide
//! `/usr/local/etc/intendant-governor.toml` (path overridable via
//! `INTENDANT_GOVERNOR_CONFIG`, which is how the acceptance tests point the
//! binary at hermetic tempdir rigs).
//!
//! The file is real TOML, but the governor deliberately does not pull
//! `toml` + `serde` into a binary that fronts every rustc invocation: it
//! parses the flat subset the installer writes — `key = value` lines with
//! booleans, non-negative integers, double-quoted strings, and single-line
//! arrays of double-quoted strings, plus `#` comments and blank lines.
//! Unknown keys are ignored (forward compatibility), but their values must
//! still be syntactically well-formed — line noise anywhere makes the whole
//! file unparseable.
//!
//! Doctrine: a missing or unparseable config FAILS OPEN. The governor must
//! never break a build, so "can't read the rules" means "get out of the
//! way", not "guess". Every invocation re-reads the file (and in-flight
//! waiters re-check it once per poll tick), which is what makes
//! `enabled = false` a live kill switch needing no listener restarts.

use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/usr/local/etc/intendant-governor.toml";
pub const DEFAULT_PERMIT_DIR: &str = "/usr/local/var/intendant-governor";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The live kill switch. Defaults to true: a config file's existence
    /// expresses intent to govern; disabling is always explicit.
    pub enabled: bool,
    /// Directory holding the permit/demand flock files and governor.log.
    pub permit_dir: PathBuf,
    /// Permits reserved for interactive (non-CI) accounts.
    pub local_reserved: u32,
    /// Permits reserved for the CI accounts listed in `ci_users`.
    pub ci_reserved: u32,
    /// Usernames whose invocations are classed `ci`; everyone else is
    /// `local`.
    pub ci_users: Vec<String>,
    /// Explicit compiler path; when unset the governor resolves
    /// `$HOME/.cargo/bin/rustc`, then `rustc` from PATH.
    pub real_rustc: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            permit_dir: PathBuf::from(DEFAULT_PERMIT_DIR),
            local_reserved: 1,
            ci_reserved: 2,
            ci_users: vec!["_intendant-ci".to_string(), "ci".to_string()],
            real_rustc: None,
        }
    }
}

/// Path of the config file: the `INTENDANT_GOVERNOR_CONFIG` override, else
/// the machine-wide default.
pub fn config_path() -> PathBuf {
    std::env::var_os("INTENDANT_GOVERNOR_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

/// Read and parse the config. `None` means "no usable config" — missing or
/// unparseable file — and every caller must fail open on it.
pub fn load(path: &Path) -> Option<Config> {
    let text = std::fs::read_to_string(path).ok()?;
    parse(&text).ok()
}

pub fn parse(text: &str) -> Result<Config, String> {
    let mut cfg = Config::default();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let err = |msg: &str| format!("line {}: {msg}", idx + 1);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| err("expected `key = value`"))?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            return Err(err("empty key"));
        }
        match key {
            "enabled" => cfg.enabled = parse_bool(value).map_err(|e| err(&e))?,
            "permit_dir" => {
                cfg.permit_dir = PathBuf::from(parse_string(value).map_err(|e| err(&e))?)
            }
            "local_reserved" => cfg.local_reserved = parse_u32(value).map_err(|e| err(&e))?,
            "ci_reserved" => cfg.ci_reserved = parse_u32(value).map_err(|e| err(&e))?,
            "ci_users" => cfg.ci_users = parse_string_array(value).map_err(|e| err(&e))?,
            "real_rustc" => {
                cfg.real_rustc = Some(PathBuf::from(parse_string(value).map_err(|e| err(&e))?))
            }
            // Unknown keys are ignored for forward compatibility, but the
            // value must still be one of the shapes we understand — a file
            // this parser can't fully read is a file it must not act on.
            _ => validate_value(value).map_err(|e| err(&e))?,
        }
    }
    Ok(cfg)
}

/// Cut an *unquoted* value at a trailing `#` comment.
fn strip_bare_comment(value: &str) -> &str {
    value.split('#').next().unwrap_or(value)
}

fn rest_is_blank_or_comment(rest: &str) -> Result<(), String> {
    let rest = rest.trim_start();
    if rest.is_empty() || rest.starts_with('#') {
        Ok(())
    } else {
        Err(format!("trailing garbage `{rest}`"))
    }
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match strip_bare_comment(value).trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("expected true/false, got `{other}`")),
    }
}

fn parse_u32(value: &str) -> Result<u32, String> {
    let bare = strip_bare_comment(value).trim();
    bare.parse::<u32>()
        .map_err(|_| format!("expected a non-negative integer, got `{bare}`"))
}

/// Scan one double-quoted string starting at `s[0]`; returns the parsed
/// value and the unconsumed remainder.
fn scan_string(s: &str) -> Result<(String, &str), String> {
    let mut chars = s.char_indices();
    if !matches!(chars.next(), Some((_, '"'))) {
        return Err("expected opening `\"`".to_string());
    }
    let mut out = String::new();
    let mut escaped = false;
    for (i, c) in chars {
        if escaped {
            out.push(match c {
                '"' => '"',
                '\\' => '\\',
                'n' => '\n',
                't' => '\t',
                other => return Err(format!("unsupported escape `\\{other}`")),
            });
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Ok((out, &s[i + 1..]));
        } else {
            out.push(c);
        }
    }
    Err("unterminated string".to_string())
}

fn parse_string(value: &str) -> Result<String, String> {
    let (s, rest) = scan_string(value.trim_start())?;
    rest_is_blank_or_comment(rest)?;
    Ok(s)
}

/// Single-line `["a", "b"]` arrays (a trailing comma is tolerated).
fn parse_string_array(value: &str) -> Result<Vec<String>, String> {
    let mut rest = value
        .trim_start()
        .strip_prefix('[')
        .ok_or_else(|| "expected `[`".to_string())?;
    let mut out = Vec::new();
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix(']') {
            rest_is_blank_or_comment(after)?;
            return Ok(out);
        }
        let (s, after) = scan_string(rest)?;
        out.push(s);
        rest = after.trim_start();
        match rest.strip_prefix(',') {
            Some(after_comma) => rest = after_comma,
            None => {
                // Without a separating comma the next token must close the
                // array.
                let after = rest
                    .strip_prefix(']')
                    .ok_or_else(|| "expected `,` or `]`".to_string())?;
                rest_is_blank_or_comment(after)?;
                return Ok(out);
            }
        }
    }
}

/// Syntax check for values of keys this version doesn't know.
fn validate_value(value: &str) -> Result<(), String> {
    let t = value.trim_start();
    if t.starts_with('"') {
        parse_string(value).map(|_| ())
    } else if t.starts_with('[') {
        parse_string_array(value).map(|_| ())
    } else {
        let bare = strip_bare_comment(value).trim();
        if bare == "true" || bare == "false" || bare.parse::<i64>().is_ok() {
            Ok(())
        } else {
            Err(format!("unrecognized value `{bare}`"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_canonical_installer_config() {
        let text = r#"
# Machine-wide rustc concurrency governor — live config.
enabled = true
permit_dir = "/usr/local/var/intendant-governor"
local_reserved = 1
ci_reserved = 2   # per-box sizing
ci_users = ["_intendant-ci", "ci"]
"#;
        let cfg = parse(text).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn empty_file_yields_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn all_keys_override_defaults() {
        let text = concat!(
            "enabled = false\n",
            "permit_dir = \"/tmp/x\" # trailing comment\n",
            "local_reserved = 3\n",
            "ci_reserved = 0\n",
            "ci_users = [\"a\", \"b\",]\n",
            "real_rustc = \"/opt/rustc\"\n",
        );
        let cfg = parse(text).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.permit_dir, PathBuf::from("/tmp/x"));
        assert_eq!(cfg.local_reserved, 3);
        assert_eq!(cfg.ci_reserved, 0);
        assert_eq!(cfg.ci_users, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(cfg.real_rustc, Some(PathBuf::from("/opt/rustc")));
    }

    #[test]
    fn string_escapes_are_decoded() {
        let cfg = parse("permit_dir = \"/tmp/a\\\\b\\\"c\"\n").unwrap();
        assert_eq!(cfg.permit_dir, PathBuf::from("/tmp/a\\b\"c"));
    }

    #[test]
    fn unknown_keys_with_valid_values_are_ignored() {
        let cfg = parse("future_knob = [\"x\"]\nother = 7\nflag = true\n").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn garbage_is_unparseable() {
        // Each of these must fail parsing — the binary fails OPEN on them.
        for bad in [
            "enabled = maybe\n",
            "local_reserved = -1\n",
            "ci_users = [\"unterminated\n",
            "not = [valid\n",
            "just some prose\n",
            "permit_dir = /unquoted/path\n",
            "= 3\n",
            "enabled = true trailing\n",
            "ci_users = [\"a\" \"b\"]\n",
        ] {
            assert!(parse(bad).is_err(), "expected parse failure for {bad:?}");
        }
    }

    #[test]
    fn comments_and_blanks_are_skipped() {
        let cfg = parse("\n   \n# full-line comment\n  # indented comment\n").unwrap();
        assert_eq!(cfg, Config::default());
    }
}
