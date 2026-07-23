//! Keyless message-lane CLI (Track C, C3): the `intendant coordination`
//! verb executors over the C1 stores — direct filesystem access, no
//! daemon reach, no IAM surface (the ruled §3.6 posture `dir` founded;
//! these verbs are the message lane's floor for every session,
//! supervised or not).
//!
//! Division of labor: argv grammar lives in `paths.rs`
//! (`parse_cli` — pure, usage on noise, exit 2 at the edge); the
//! executors here are pure functions over a resolved space dir and an
//! already-resolved writer identity, so the acceptance tests inject
//! tempdir spaces and explicit identities; [`run`] is the one process
//! edge that reads env, cwd, and stdin, prints, and picks exit codes.
//!
//! Output contract (scripts-stable): one record per line on stdout, in
//! the fixed `id= from= to= kind= age_s= ttl_s= expired=` key=value
//! grammar (every value is a machine token — ids, counts, booleans;
//! `to=*` marks a broadcast, and `*` is outside the writer grammar so
//! the token is unambiguous). **Listings are summaries, never bodies**
//! (§2.2's summary-not-content rule, pinned by test): a message body
//! reaches stdout only through the explicit `read` verb — the
//! documented lazy read, whose output is the summary line and then the
//! body verbatim, quoted data for the caller to weigh. Scan rejections
//! surface on stderr in the §2.2 `invalid:` wording — loud, never
//! silent, never fatal to the listing (rule-5 liveness posture).

use std::path::{Path, PathBuf};

use super::messages::{MessageInput, MessageMeta, MessageSpace};
use super::{paths, CoordinationError};

/// One listing record (the fixed field order the module doc pins).
pub(crate) fn format_meta_line(meta: &MessageMeta, now_ms: u64) -> String {
    format!(
        "id={} from={} to={} kind={} age_s={} ttl_s={} expired={}",
        meta.id,
        meta.writer,
        meta.to.as_deref().unwrap_or("*"),
        meta.kind,
        now_ms.saturating_sub(meta.created_ms) / 1000,
        meta.ttl_s,
        meta.expired,
    )
}

/// The `messages` verb: every message's metadata across all writers,
/// sorted (writer, id) for stable scripting — summaries only.
pub(crate) struct MessagesListing {
    /// One record per line (empty when the space holds no messages).
    pub stdout: String,
    /// Entries the scan rejected by name (rule 5) — the edge reports
    /// the count on stderr and still exits 0.
    pub invalid: usize,
}

pub(crate) fn list_messages(
    space_dir: &Path,
    space: &str,
    now_ms: u64,
) -> Result<MessagesListing, CoordinationError> {
    let scan = MessageSpace::open_existing(space_dir, space).scan_meta(now_ms)?;
    let mut metas = scan.entries;
    metas.sort_by(|a, b| (&a.writer, &a.id).cmp(&(&b.writer, &b.id)));
    let mut stdout = String::new();
    for meta in &metas {
        stdout.push_str(&format_meta_line(meta, now_ms));
        stdout.push('\n');
    }
    Ok(MessagesListing {
        stdout,
        invalid: scan.rejected.len(),
    })
}

/// The `read` verb: the explicit §2.2 lazy read — summary line first,
/// then the body verbatim. `None` = no such message (absent, expired
/// and GC'd, or a writer/id outside the grammar — indistinguishable by
/// design; nothing here guesses).
pub(crate) fn read_message(
    space_dir: &Path,
    space: &str,
    writer: &str,
    id: &str,
    now_ms: u64,
) -> Result<Option<String>, CoordinationError> {
    let store = MessageSpace::open_existing(space_dir, space);
    let Some(message) = store.read(writer, id, now_ms)? else {
        return Ok(None);
    };
    Ok(Some(format!(
        "{}\n{}\n",
        format_meta_line(&message.meta, now_ms),
        message.body
    )))
}

/// The `send` verb: one bounded message from an already-resolved
/// writer identity. The store owns every refusal (grammar, the
/// reserved `daemon` writer, caps, doc bound, empty body) — the CLI
/// surfaces them verbatim.
pub(crate) fn send_message(
    space_dir: &Path,
    space: &str,
    writer: &str,
    to: Option<&str>,
    ttl_s: Option<u32>,
    body: &str,
) -> Result<MessageMeta, CoordinationError> {
    let store = MessageSpace::open(space_dir, space)?;
    store.write(writer, &MessageInput { to, ttl_s, body })
}

/// The `delete` verb: a writer removes its own message. `false` = no
/// such message; the reserved daemon writer is refused by the store.
pub(crate) fn delete_message(
    space_dir: &Path,
    space: &str,
    writer: &str,
    id: &str,
) -> Result<bool, CoordinationError> {
    MessageSpace::open_existing(space_dir, space).delete_own(writer, id)
}

/// Resolve the space for one invocation: the explicit `--root` (or the
/// cwd) through the one shared resolution rule — the
/// `INTENDANT_COORDINATION_DIR` override wins, exactly as it does for
/// every supervised consumer.
fn resolve_space(explicit_root: Option<&Path>) -> Result<(PathBuf, String), String> {
    let root = match explicit_root {
        Some(root) => root.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| format!("cannot resolve the current directory: {e}"))?,
    };
    Ok(paths::resolve_space_dir(
        paths::env_override().as_deref(),
        &crate::platform::intendant_home(),
        &root,
    ))
}

/// The process edge for `intendant coordination …` (argv after the
/// `coordination` word): parses, reads env/cwd/stdin exactly here,
/// prints, and returns the exit code — 0 on success, 1 on store/IO
/// refusals, 2 on usage (including an unresolvable writer identity).
pub(crate) fn run(argv: &[String]) -> i32 {
    let cmd = match paths::parse_cli(argv) {
        Ok(cmd) => cmd,
        Err(usage) => {
            eprintln!("{usage}");
            return 2;
        }
    };
    let root = match &cmd {
        paths::CoordinationCli::Dir { root }
        | paths::CoordinationCli::Messages { root }
        | paths::CoordinationCli::Read { root, .. }
        | paths::CoordinationCli::Send { root, .. }
        | paths::CoordinationCli::Delete { root, .. } => root.clone(),
    };
    let (space_dir, space) = match resolve_space(root.as_deref()) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let now_ms = super::now_ms();
    match cmd {
        paths::CoordinationCli::Dir { .. } => {
            println!("{}", space_dir.display());
            0
        }
        paths::CoordinationCli::Messages { .. } => {
            match list_messages(&space_dir, &space, now_ms) {
                Ok(listing) => {
                    print!("{}", listing.stdout);
                    if listing.invalid > 0 {
                        eprintln!("invalid: {} entries ignored", listing.invalid);
                    }
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        paths::CoordinationCli::Read { writer, id, .. } => {
            match read_message(&space_dir, &space, &writer, &id, now_ms) {
                Ok(Some(text)) => {
                    print!("{text}");
                    0
                }
                Ok(None) => {
                    eprintln!("error: no message {id} from {writer} in this space");
                    1
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        paths::CoordinationCli::Send {
            to,
            ttl_s,
            as_writer,
            body,
            ..
        } => {
            let session_id = std::env::var("INTENDANT_SESSION_ID").ok();
            let Some(writer) =
                paths::resolve_writer_identity(as_writer.as_deref(), session_id.as_deref())
            else {
                eprintln!("{}", paths::WRITER_IDENTITY_USAGE);
                return 2;
            };
            let body = match body {
                Some(body) => body,
                None => {
                    // Bounded stdin slurp: one byte over the §9 doc cap
                    // is enough for the store to refuse with its named
                    // oversize error.
                    use std::io::Read;
                    let mut buf = String::new();
                    let cap = (super::MAX_DOC_BYTES + 1) as u64;
                    if let Err(e) = std::io::stdin().lock().take(cap).read_to_string(&mut buf) {
                        eprintln!("error: reading the message body from stdin: {e}");
                        return 1;
                    }
                    buf
                }
            };
            match send_message(&space_dir, &space, &writer, to.as_deref(), ttl_s, &body) {
                Ok(meta) => {
                    println!("{}", meta.id);
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        paths::CoordinationCli::Delete { id, as_writer, .. } => {
            let session_id = std::env::var("INTENDANT_SESSION_ID").ok();
            let Some(writer) =
                paths::resolve_writer_identity(as_writer.as_deref(), session_id.as_deref())
            else {
                eprintln!("{}", paths::WRITER_IDENTITY_USAGE);
                return 2;
            };
            match delete_message(&space_dir, &space, &writer, &id) {
                Ok(true) => 0,
                Ok(false) => {
                    eprintln!("error: no message {id} for writer {writer} in this space");
                    1
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::messages::MESSAGE_TTL_MIN_S;
    use super::super::paths::{parse_cli, resolve_writer_identity, CoordinationCli};
    use super::super::{gc, radar, render};
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    /// Drive one parsed `send` through the pure layer with an explicit
    /// identity environment (the edge's env read stays untested-thin).
    fn send_parsed(
        space_dir: &Path,
        argv: &[&str],
        session_id: Option<&str>,
    ) -> Result<MessageMeta, CoordinationError> {
        let Ok(CoordinationCli::Send {
            to,
            ttl_s,
            as_writer,
            body,
            ..
        }) = parse_cli(&args(argv))
        else {
            panic!("send argv must parse: {argv:?}");
        };
        let writer = resolve_writer_identity(as_writer.as_deref(), session_id)
            .expect("identity resolves for this fixture");
        send_message(
            space_dir,
            "test-space",
            &writer,
            to.as_deref(),
            ttl_s,
            &body.expect("fixture body is positional"),
        )
    }

    /// The ruled C3 acceptance: a daemonless guest writer round-trips
    /// through the exact CLI grammar — send → meta listed → read
    /// returns the exact body → delete — with no daemon and no env.
    #[test]
    fn daemonless_writer_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let body = "heads up: coordination/mod.rs is mid-carve; land after #560";
        let meta = send_parsed(
            &space_dir,
            &[
                "send",
                "--as",
                "guest-x01round",
                "--to",
                "s-native-7f2a",
                "--ttl-s",
                "3600",
                body,
            ],
            None,
        )
        .unwrap();
        assert!(meta.id.starts_with("m-"), "{}", meta.id);

        let now = super::super::now_ms();
        let listing = list_messages(&space_dir, "test-space", now).unwrap();
        assert_eq!(listing.invalid, 0);
        let lines: Vec<&str> = listing.stdout.lines().collect();
        assert_eq!(lines.len(), 1, "{}", listing.stdout);
        assert_eq!(
            lines[0],
            format!(
                "id={} from=guest-x01round to=s-native-7f2a kind=message age_s={} ttl_s=3600 expired=false",
                meta.id,
                now.saturating_sub(meta.created_ms) / 1000
            )
        );

        let read = read_message(&space_dir, "test-space", "guest-x01round", &meta.id, now)
            .unwrap()
            .expect("message reads back");
        let (summary, read_body) = read.split_once('\n').expect("summary line then body");
        assert_eq!(summary, lines[0]);
        assert_eq!(read_body, format!("{body}\n"), "body verbatim");

        // Delete through the parsed grammar (same identity resolution).
        let Ok(CoordinationCli::Delete { id, as_writer, .. }) =
            parse_cli(&args(&["delete", &meta.id, "--as", "guest-x01round"]))
        else {
            panic!("delete argv must parse");
        };
        let writer = resolve_writer_identity(as_writer.as_deref(), None).unwrap();
        assert!(delete_message(&space_dir, "test-space", &writer, &id).unwrap());
        assert!(
            !delete_message(&space_dir, "test-space", &writer, &id).unwrap(),
            "second delete reports absence"
        );
        assert!(list_messages(&space_dir, "test-space", now)
            .unwrap()
            .stdout
            .is_empty());
        assert!(
            read_message(&space_dir, "test-space", "guest-x01round", &id, now)
                .unwrap()
                .is_none()
        );
    }

    /// Supervised identity: no `--as` resolves the session id through
    /// the one session→writer mapping — passed explicitly into the pure
    /// layer (the edge's env read is a one-liner).
    #[test]
    fn supervised_identity_maps_the_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let meta = send_parsed(
            &space_dir,
            &["send", "supervised", "note"],
            Some("Sess_01ABC"),
        )
        .unwrap();
        assert_eq!(meta.writer, "s-sess-01abc");
        assert_eq!(meta.to, None, "no --to = broadcast");
        assert_eq!(
            meta.ttl_s,
            super::super::messages::MESSAGE_TTL_DEFAULT_S,
            "default TTL when --ttl-s is absent"
        );
        let listing = list_messages(&space_dir, "test-space", super::super::now_ms()).unwrap();
        assert!(
            listing.stdout.contains("from=s-sess-01abc to=* "),
            "{}",
            listing.stdout
        );
        // The reserved writer refusal surfaces the store's own error —
        // on send and on delete alike.
        let err = send_parsed(&space_dir, &["send", "--as", "daemon", "x"], None).unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
        let err = delete_message(&space_dir, "test-space", "daemon", &meta.id).unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    /// The ruled summary-not-content pin: listing output never carries
    /// body bytes — sentinel bodies (and a radar-note body built from a
    /// sentinel path) stay out of the listing and appear only through
    /// the explicit `read`.
    #[test]
    fn listing_is_summary_not_content() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        const SENTINEL_A: &str = "XYZZY_BODY_SENTINEL_ALPHA";
        const SENTINEL_B: &str = "XYZZY_BODY_SENTINEL_BRAVO";
        const SENTINEL_PATH: &str = "secret/XYZZY_SENTINEL_PATH.rs";
        send_parsed(
            &space_dir,
            &[
                "send",
                "--as",
                "guest-x01pin",
                &format!("{SENTINEL_A} do not print"),
            ],
            None,
        )
        .unwrap();
        send_parsed(
            &space_dir,
            &[
                "send",
                "--as",
                "guest-x01pin",
                "--to",
                "s-native-7f2a",
                &format!("directed {SENTINEL_B}"),
            ],
            None,
        )
        .unwrap();
        // A daemon radar note: its body is built from paths — the
        // listing must not leak those either.
        let store = MessageSpace::open(&space_dir, "test-space").unwrap();
        let note = store
            .write_radar_note(&super::super::messages::RadarNoteInput {
                to: "s-native-7f2a",
                parties: &["s-native-7f2a", "s-other"],
                declared: true,
                git: false,
                pr: None,
                paths: &[SENTINEL_PATH.to_string()],
                ttl_s: None,
            })
            .unwrap()
            .expect("note lands");

        let now = super::super::now_ms();
        let listing = list_messages(&space_dir, "test-space", now).unwrap();
        assert_eq!(listing.stdout.lines().count(), 3);
        for sentinel in [SENTINEL_A, SENTINEL_B, "XYZZY"] {
            assert!(
                !listing.stdout.contains(sentinel),
                "listing leaked body bytes ({sentinel}):\n{}",
                listing.stdout
            );
        }
        assert!(
            !listing.stdout.contains("secret/"),
            "listing leaked a radar-note path:\n{}",
            listing.stdout
        );
        // The lazy read is where content legitimately surfaces.
        let read = read_message(&space_dir, "test-space", "daemon", &note.id, now)
            .unwrap()
            .unwrap();
        assert!(read.contains(SENTINEL_PATH), "{read}");
    }

    /// TTL acceptance: the expired flag is visible in the listing
    /// before the sweep; `gc::sweep_space` at a later clock removes the
    /// message and the listing goes quiet.
    #[test]
    fn ttl_expiry_flags_then_gc_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let meta = send_parsed(
            &space_dir,
            &[
                "send",
                "--as",
                "guest-x01ttl",
                "--ttl-s",
                "60",
                "short lived",
            ],
            None,
        )
        .unwrap();
        assert_eq!(meta.ttl_s, MESSAGE_TTL_MIN_S);

        let now = super::super::now_ms();
        assert!(list_messages(&space_dir, "test-space", now)
            .unwrap()
            .stdout
            .contains("expired=false"));

        let later = now + u64::from(MESSAGE_TTL_MIN_S) * 1000 + 1500;
        let listing = list_messages(&space_dir, "test-space", later).unwrap();
        assert!(
            listing.stdout.contains("expired=true"),
            "expired flag visible before the sweep: {}",
            listing.stdout
        );

        let mut report = gc::GcReport::default();
        gc::sweep_space(&space_dir, "test-space", later, &mut report);
        assert_eq!(report.messages_removed, 1);
        assert!(!space_dir
            .join("messages/guest-x01ttl")
            .join(format!("{}.md", meta.id))
            .exists());
        assert!(list_messages(&space_dir, "test-space", later)
            .unwrap()
            .stdout
            .is_empty());
    }

    /// The round-trip's "seen by the radar" half (C2's fixture style):
    /// a CLI-sent message surfaces on the recipient's rendered
    /// `messages:` line — existence and provenance only, never text.
    #[test]
    fn radar_messages_line_reflects_a_cli_sent_message() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        let meta = send_parsed(
            &space_dir,
            &[
                "send",
                "--as",
                "guest-x01radar",
                "--to",
                "s-native-7f2a",
                "XYZZY_BODY_SENTINEL never rendered",
            ],
            None,
        )
        .unwrap();

        let now = super::super::now_ms();
        let bus = radar::read_space_bus(&space_dir, now).unwrap();
        let snapshot = radar::compute_space_snapshot(
            &radar::RadarSpaceInputs {
                space_key: "test-space-0123456789abcdef",
                declarations: &bus.declarations,
                observed: &[],
                messages: &bus.messages,
                pr_files: &[],
                scan_invalid: bus.scan_invalid,
            },
            now,
        );
        let block = render::render_block(&snapshot, "s-native-7f2a", None, 0, now)
            .expect("a directed message is signal");
        assert!(
            block.text.contains(&format!(
                "messages: 1 unread — from guest-x01radar: {}",
                meta.id
            )),
            "{}",
            block.text
        );
        assert!(
            !block.text.contains("XYZZY"),
            "the radar line never carries body text: {}",
            block.text
        );
        // A third party sees nothing: the message is not addressed to
        // it and no other signal exists.
        assert!(
            render::render_block(&snapshot, "s-bystander", None, 0, now).is_none(),
            "directed mail is not a bystander's signal"
        );
    }

    /// Listing rejections stay loud and non-fatal (rule-5 posture): a
    /// malformed same-UID file becomes an `invalid` count while the
    /// well-formed records still print.
    #[test]
    fn listing_counts_malformed_entries_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let space_dir = tmp.path().join("space");
        send_parsed(&space_dir, &["send", "--as", "guest-x01loud", "fine"], None).unwrap();
        std::fs::write(
            space_dir.join("messages/guest-x01loud/m-junk.md"),
            "not a protocol document",
        )
        .unwrap();
        let listing = list_messages(&space_dir, "test-space", super::super::now_ms()).unwrap();
        assert_eq!(listing.stdout.lines().count(), 1, "{}", listing.stdout);
        assert_eq!(listing.invalid, 1);
    }
}
