//! Space-dir resolution (Track C, C1): one seam decides which
//! coordination space a process writes into.
//!
//! Default derivation is `<intendant-home>/coordination/<space-key>`
//! with the worktree-normalized key from `space_key`. The
//! `INTENDANT_COORDINATION_DIR` override names a space dir directly —
//! the parent exports it to sub-agent / external-agent children so an
//! isolated worktree child lands in the PARENT's space (worktree
//! normalization already agrees for same-repo worktrees; the override
//! covers detached temp clones and deliberate space grouping). Env is
//! read only at the process edge (`env_override`); everything below
//! takes explicit paths (the repo's hermeticity rule).
use std::path::{Path, PathBuf};

pub(crate) const COORDINATION_DIR_ENV: &str = "INTENDANT_COORDINATION_DIR";

/// The coordination root under a resolved intendant home (the
/// `~/.intendant` directory itself, already override-aware upstream).
pub(crate) fn coordination_root(intendant_home: &Path) -> PathBuf {
    intendant_home.join("coordination")
}

/// Derived space dir + key for a project root.
pub(crate) fn space_dir_under(intendant_home: &Path, project_root: &Path) -> (PathBuf, String) {
    let key = super::space_key(project_root);
    (coordination_root(intendant_home).join(&key), key)
}

/// Resolution order: explicit override (already read from env at the
/// edge) wins; otherwise derive. The space label for an override is
/// its basename, sanitized only if it strays outside the grammar
/// (space-key output can exceed `sanitize_key`'s 64-char clamp, so a
/// well-formed key must pass through untouched).
pub(crate) fn resolve_space_dir(
    override_dir: Option<&Path>,
    intendant_home: &Path,
    project_root: &Path,
) -> (PathBuf, String) {
    match override_dir {
        Some(dir) => {
            let raw = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let grammar_ok = !raw.is_empty()
                && raw.len() <= 96
                && raw
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            let label = if grammar_ok {
                raw
            } else {
                super::sanitize_key(&raw)
            };
            (dir.to_path_buf(), label)
        }
        None => space_dir_under(intendant_home, project_root),
    }
}

/// The process-edge env read. Tests never touch this — they pass
/// explicit overrides to `resolve_space_dir`.
pub(crate) fn env_override() -> Option<PathBuf> {
    std::env::var_os(COORDINATION_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub(crate) const DIR_CLI_USAGE: &str = "usage: intendant coordination dir [--root <path>]";

/// Argv parse for the keyless `intendant coordination …` administrative
/// subcommand (§3.6/R5 of the ruled protocol — `dir` was the founding
/// verb; no daemon reach, no IAM surface). Input is everything after
/// the `coordination` word. `Ok(None)` = resolve for the cwd,
/// `Ok(Some)` = resolve for the explicit root. The single output line
/// feeds scripts, so any unrecognized noise is a usage error rather
/// than a plausible-but-wrong line.
pub(crate) fn parse_dir_cli(argv: &[String]) -> Result<Option<PathBuf>, String> {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    match argv.as_slice() {
        ["dir"] => Ok(None),
        ["dir", "--root", root] if !root.is_empty() => Ok(Some(PathBuf::from(root))),
        _ => Err(DIR_CLI_USAGE.to_string()),
    }
}

pub(crate) const MESSAGES_CLI_USAGE: &str =
    "usage: intendant coordination messages [--root <path>]";
pub(crate) const READ_CLI_USAGE: &str =
    "usage: intendant coordination read <writer> <id> [--root <path>]";
pub(crate) const SEND_CLI_USAGE: &str = "usage: intendant coordination send [--to <writer>] \
     [--ttl-s <secs>] [--as <writer>] [--root <path>] [--] [body…]   \
     (no body argv: the body is read from stdin)";
pub(crate) const DELETE_CLI_USAGE: &str =
    "usage: intendant coordination delete <id> [--as <writer>] [--root <path>]";

/// The umbrella usage: printed for an unknown or missing verb (each
/// verb's own noise prints its own line above, the `parse_dir_cli`
/// precedent).
pub(crate) const CLI_USAGE: &str = "usage: intendant coordination <verb>\n\
       dir      [--root <path>]                    print the coordination-space directory\n\
       messages [--root <path>]                    list message metadata (summaries — never bodies)\n\
       read <writer> <id> [--root <path>]          print one message: summary line, then the body\n\
       send [--to <writer>] [--ttl-s <secs>] [--as <writer>] [--root <path>] [--] [body…]\n\
                                                   leave a message (no body argv: body from stdin)\n\
       delete <id> [--as <writer>] [--root <path>] delete your own message";

/// The refusal printed when `send`/`delete` cannot resolve a writer
/// identity. Refuse rather than mint: a fresh `guest-<ulid>` per
/// invocation would fragment writer dirs against the 128-dir space cap
/// (§1.6), so unsupervised callers mint ONE guest id and reuse it.
pub(crate) const WRITER_IDENTITY_USAGE: &str = "no writer identity: pass --as <writer> — \
     unsupervised shells mint ONE guest id (--as guest-<your-id>) and reuse it for their \
     lifetime (a fresh guest per send would fragment the space's writer-dir cap); \
     supervised sessions inherit INTENDANT_SESSION_ID";

/// One parsed `intendant coordination …` invocation (Track C, C3): the
/// C1 `dir` verb plus the message-lane verbs, all keyless and
/// daemonless — the parser is pure argv → value; env, cwd, and stdin
/// are read only at the process edge (`cli::run`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CoordinationCli {
    Dir {
        root: Option<PathBuf>,
    },
    Messages {
        root: Option<PathBuf>,
    },
    Read {
        writer: String,
        id: String,
        root: Option<PathBuf>,
    },
    Send {
        to: Option<String>,
        ttl_s: Option<u32>,
        as_writer: Option<String>,
        root: Option<PathBuf>,
        /// `None` = no positional body words — the edge reads stdin.
        body: Option<String>,
    },
    Delete {
        id: String,
        as_writer: Option<String>,
        root: Option<PathBuf>,
    },
}

/// Shared `--flag value` walker for the verb grammars: flags may appear
/// in any order, at most once each, values non-empty. With `body_tail`,
/// the first non-flag token (or a bare `--`) ends flag parsing and
/// everything after is body verbatim — flags never hide inside message
/// text. Errors are unit; each verb maps them to its own usage line.
fn split_flags<'a>(
    argv: &[&'a str],
    allowed: &[&str],
    body_tail: bool,
) -> Result<(std::collections::BTreeMap<&'a str, &'a str>, Vec<&'a str>), ()> {
    let mut flags = std::collections::BTreeMap::new();
    let mut positionals = Vec::new();
    let mut in_tail = false;
    let mut iter = argv.iter();
    while let Some(&tok) = iter.next() {
        if !in_tail && body_tail && tok == "--" {
            in_tail = true;
            continue;
        }
        if !in_tail && tok.starts_with("--") {
            if !allowed.contains(&tok) {
                return Err(());
            }
            let Some(&value) = iter.next() else {
                return Err(());
            };
            if value.is_empty() || flags.insert(tok, value).is_some() {
                return Err(());
            }
            continue;
        }
        if body_tail {
            in_tail = true;
        }
        positionals.push(tok);
    }
    Ok((flags, positionals))
}

/// Full argv parse for `intendant coordination …` (everything after the
/// `coordination` word). Same contract as [`parse_dir_cli`], which the
/// `dir` arm delegates to: usage on any noise, per-verb usage lines,
/// exit 2 at the edge.
pub(crate) fn parse_cli(argv: &[String]) -> Result<CoordinationCli, String> {
    let toks: Vec<&str> = argv.iter().map(String::as_str).collect();
    let Some((&verb, rest)) = toks.split_first() else {
        return Err(CLI_USAGE.to_string());
    };
    match verb {
        "dir" => parse_dir_cli(argv).map(|root| CoordinationCli::Dir { root }),
        "messages" => {
            let (flags, positionals) = split_flags(rest, &["--root"], false)
                .map_err(|()| MESSAGES_CLI_USAGE.to_string())?;
            if !positionals.is_empty() {
                return Err(MESSAGES_CLI_USAGE.to_string());
            }
            Ok(CoordinationCli::Messages {
                root: flags.get("--root").copied().map(PathBuf::from),
            })
        }
        "read" => {
            let (flags, positionals) =
                split_flags(rest, &["--root"], false).map_err(|()| READ_CLI_USAGE.to_string())?;
            let [writer, id] = positionals.as_slice() else {
                return Err(READ_CLI_USAGE.to_string());
            };
            Ok(CoordinationCli::Read {
                writer: (*writer).to_string(),
                id: (*id).to_string(),
                root: flags.get("--root").copied().map(PathBuf::from),
            })
        }
        "send" => {
            let (flags, body_words) =
                split_flags(rest, &["--to", "--ttl-s", "--as", "--root"], true)
                    .map_err(|()| SEND_CLI_USAGE.to_string())?;
            let ttl_s = match flags.get("--ttl-s") {
                None => None,
                Some(raw) => Some(raw.parse::<u32>().map_err(|_| SEND_CLI_USAGE.to_string())?),
            };
            Ok(CoordinationCli::Send {
                to: flags.get("--to").copied().map(str::to_string),
                ttl_s,
                as_writer: flags.get("--as").copied().map(str::to_string),
                root: flags.get("--root").copied().map(PathBuf::from),
                body: (!body_words.is_empty()).then(|| body_words.join(" ")),
            })
        }
        "delete" => {
            let (flags, positionals) = split_flags(rest, &["--as", "--root"], false)
                .map_err(|()| DELETE_CLI_USAGE.to_string())?;
            let [id] = positionals.as_slice() else {
                return Err(DELETE_CLI_USAGE.to_string());
            };
            Ok(CoordinationCli::Delete {
                id: (*id).to_string(),
                as_writer: flags.get("--as").copied().map(str::to_string),
                root: flags.get("--root").copied().map(PathBuf::from),
            })
        }
        _ => Err(CLI_USAGE.to_string()),
    }
}

/// Writer identity for `send`/`delete` (ruled §1.3/R9): an explicit
/// `--as` wins verbatim (the store validates the grammar and refuses
/// the reserved `daemon` name); otherwise a supervised session id maps
/// through the one session→writer rule. `None` = refuse at the edge
/// with [`WRITER_IDENTITY_USAGE`] — never mint. Pure: the process edge
/// reads `INTENDANT_SESSION_ID` and passes it in.
pub(crate) fn resolve_writer_identity(
    as_writer: Option<&str>,
    session_id: Option<&str>,
) -> Option<String> {
    if let Some(writer) = as_writer.map(str::trim).filter(|w| !w.is_empty()) {
        return Some(writer.to_string());
    }
    session_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(super::lifecycle::writer_id_for_session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_and_override_agree_on_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("state");
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        let (derived, key) = resolve_space_dir(None, &home, &project);
        assert_eq!(derived, home.join("coordination").join(&key));
        assert!(key.starts_with("proj-"), "{key}");

        // Override wins wholesale and reuses its basename as the label.
        let (dir, label) = resolve_space_dir(Some(&derived), &home, tmp.path());
        assert_eq!(dir, derived);
        assert_eq!(label, key, "well-formed key passes through unclamped");
    }

    #[test]
    fn hostile_override_basename_is_sanitized() {
        let tmp = tempfile::tempdir().unwrap();
        let odd = tmp.path().join("Weird Space！");
        let (_, label) = resolve_space_dir(Some(&odd), tmp.path(), tmp.path());
        assert_eq!(label, "weird-space");
    }

    #[test]
    fn dir_cli_parses_the_one_verb_and_refuses_noise() {
        let args = |raw: &[&str]| raw.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_dir_cli(&args(&["dir"])).unwrap(), None);
        assert_eq!(
            parse_dir_cli(&args(&["dir", "--root", "/some/proj"])).unwrap(),
            Some(PathBuf::from("/some/proj"))
        );
        for bad in [
            &[] as &[&str],
            &["gc"],
            &["dir", "--root"],
            &["dir", "--root", ""],
            &["dir", "extra"],
            &["dir", "--root", "/x", "trailing"],
        ] {
            let err = parse_dir_cli(&args(bad)).unwrap_err();
            assert_eq!(err, DIR_CLI_USAGE, "{bad:?}");
        }
    }

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_dispatch_covers_all_verbs_and_refuses_unknowns() {
        assert_eq!(
            parse_cli(&args(&["dir"])).unwrap(),
            CoordinationCli::Dir { root: None }
        );
        assert_eq!(
            parse_cli(&args(&["dir", "--root", "/p"])).unwrap(),
            CoordinationCli::Dir {
                root: Some(PathBuf::from("/p"))
            }
        );
        // The dir arm keeps parse_dir_cli's own usage line on its noise.
        assert_eq!(
            parse_cli(&args(&["dir", "extra"])).unwrap_err(),
            DIR_CLI_USAGE
        );
        for unknown in [&[] as &[&str], &["gc"], &["--root"], &["Messages"]] {
            assert_eq!(
                parse_cli(&args(unknown)).unwrap_err(),
                CLI_USAGE,
                "{unknown:?}"
            );
        }
    }

    #[test]
    fn messages_cli_takes_only_the_root_flag() {
        assert_eq!(
            parse_cli(&args(&["messages"])).unwrap(),
            CoordinationCli::Messages { root: None }
        );
        assert_eq!(
            parse_cli(&args(&["messages", "--root", "/p"])).unwrap(),
            CoordinationCli::Messages {
                root: Some(PathBuf::from("/p"))
            }
        );
        for bad in [
            &["messages", "extra"] as &[&str],
            &["messages", "--root"],
            &["messages", "--root", ""],
            &["messages", "--as", "w"],
            &["messages", "--root", "/p", "--root", "/q"],
        ] {
            assert_eq!(
                parse_cli(&args(bad)).unwrap_err(),
                MESSAGES_CLI_USAGE,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn read_cli_wants_writer_and_id_exactly() {
        assert_eq!(
            parse_cli(&args(&["read", "s-alpha", "m-01abc23def"])).unwrap(),
            CoordinationCli::Read {
                writer: "s-alpha".into(),
                id: "m-01abc23def".into(),
                root: None,
            }
        );
        // Flags parse regardless of position around the positionals.
        assert_eq!(
            parse_cli(&args(&["read", "--root", "/p", "s-alpha", "m-x0123456789"])).unwrap(),
            parse_cli(&args(&["read", "s-alpha", "m-x0123456789", "--root", "/p"])).unwrap(),
        );
        for bad in [
            &["read"] as &[&str],
            &["read", "s-alpha"],
            &["read", "s-alpha", "m-1", "extra"],
            &["read", "s-alpha", "m-1", "--to", "x"],
            &["read", "s-alpha", "m-1", "--root"],
        ] {
            assert_eq!(
                parse_cli(&args(bad)).unwrap_err(),
                READ_CLI_USAGE,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn send_cli_flags_precede_the_body_and_join_its_words() {
        assert_eq!(
            parse_cli(&args(&[
                "send", "--to", "s-b", "--ttl-s", "3600", "--as", "guest-x1", "--root", "/p",
                "heads", "up:", "touching", "tools.rs",
            ]))
            .unwrap(),
            CoordinationCli::Send {
                to: Some("s-b".into()),
                ttl_s: Some(3600),
                as_writer: Some("guest-x1".into()),
                root: Some(PathBuf::from("/p")),
                body: Some("heads up: touching tools.rs".into()),
            }
        );
        // No positional body → stdin at the edge.
        assert_eq!(
            parse_cli(&args(&["send", "--as", "guest-x1"])).unwrap(),
            CoordinationCli::Send {
                to: None,
                ttl_s: None,
                as_writer: Some("guest-x1".into()),
                root: None,
                body: None,
            }
        );
        // `--` opens the body: later flag-shaped words are body text,
        // and the first plain word closes flag parsing the same way.
        assert_eq!(
            parse_cli(&args(&["send", "--as", "g-1", "--", "--to", "everyone"])).unwrap(),
            CoordinationCli::Send {
                to: None,
                ttl_s: None,
                as_writer: Some("g-1".into()),
                root: None,
                body: Some("--to everyone".into()),
            }
        );
        assert_eq!(
            parse_cli(&args(&["send", "note", "--to", "you"])).unwrap(),
            CoordinationCli::Send {
                to: None,
                ttl_s: None,
                as_writer: None,
                root: None,
                body: Some("note --to you".into()),
            }
        );
        for bad in [
            &["send", "--ttl-s", "soon", "x"] as &[&str],
            &["send", "--ttl-s", "-5", "x"],
            &["send", "--to"],
            &["send", "--to", ""],
            &["send", "--to", "a", "--to", "b"],
            &["send", "--unknown", "x"],
        ] {
            assert_eq!(
                parse_cli(&args(bad)).unwrap_err(),
                SEND_CLI_USAGE,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn delete_cli_wants_one_id() {
        assert_eq!(
            parse_cli(&args(&[
                "delete", "m-01x", "--as", "guest-x1", "--root", "/p"
            ]))
            .unwrap(),
            CoordinationCli::Delete {
                id: "m-01x".into(),
                as_writer: Some("guest-x1".into()),
                root: Some(PathBuf::from("/p")),
            }
        );
        assert_eq!(
            parse_cli(&args(&["delete", "m-01x"])).unwrap(),
            CoordinationCli::Delete {
                id: "m-01x".into(),
                as_writer: None,
                root: None,
            }
        );
        for bad in [
            &["delete"] as &[&str],
            &["delete", "a", "b"],
            &["delete", "m-1", "--to", "x"],
            &["delete", "m-1", "--as"],
        ] {
            assert_eq!(
                parse_cli(&args(bad)).unwrap_err(),
                DELETE_CLI_USAGE,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn writer_identity_prefers_explicit_then_session_then_refuses() {
        // Explicit --as wins verbatim (store validates grammar later).
        assert_eq!(
            resolve_writer_identity(Some("guest-x01"), Some("sess-1")).as_deref(),
            Some("guest-x01")
        );
        // Supervised: the one session→writer mapping (s- prefix,
        // sanitized) — same rule as the declaration filename.
        assert_eq!(
            resolve_writer_identity(None, Some("Sess_01ABC")).as_deref(),
            Some("s-sess-01abc")
        );
        // Nothing: refuse (the edge prints WRITER_IDENTITY_USAGE, exit 2)
        // — never mint a fresh guest.
        assert_eq!(resolve_writer_identity(None, None), None);
        assert_eq!(resolve_writer_identity(Some("  "), Some("")), None);
        assert!(WRITER_IDENTITY_USAGE.contains("--as guest-"));
    }
}
