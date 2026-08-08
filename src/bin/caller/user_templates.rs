//! The dashboard-added PERSONAL automation definitions (skills/plugins
//! unification S5): the S4 user-skill lane retargeted at the automation
//! library — `<state_root>/automations/<name>/SKILL.md`, the exact home
//! the documented shadow-resolution order already reads
//! (`agenda::definitions::resolve_definition`: personal shadows house) — with
//! a [`crate::skill_state::UserTemplateRecord`] (S4's record shape:
//! gate-resolved attribution + sha256 of the accepted bytes) persisted
//! under the `templates` family of `skills/state.json`.
//!
//! **Validation is the REAL definition intake.** The submitted bytes go
//! through [`crate::agenda::parse_definition`] — the ONE validator every
//! stamp resolves through (deny-unknown config blocks, spec frontmatter,
//! node arity, the whole set) — so a definition that would refuse at
//! stamp time refuses at ADD time with the parser's own error. The add
//! executes nothing and arms nothing: a definition does nothing until
//! stamped, and stamping still seals the file under an approval digest
//! through the untouched stamp lane (intake §5.1).
//!
//! **House definitions stay sealed.** The embedded house set lives under
//! `automations/.house/` and is never written here; adding a personal
//! definition under a house NAME is the documented override mechanism —
//! it shadows the house twin visibly (the catalog lists both) and
//! removing it un-shadows. That is the ONE collision the add lane
//! accepts; everything else refuses by name.
//!
//! **Hand-placed stays owner-owned.** Personal definitions the owner
//! placed by hand (no registry record) are the automations library's
//! ordinary tenants — the dashboard lanes never overwrite or delete
//! them (the S4 unmarked-directory law transplanted): an add refusing
//! toward the existing directory and a remove refusing toward "manage
//! it by hand" are both fail-closed walls. The recorded sha256 is
//! provenance, not a gate: a hand-edited recorded file still lists and
//! still stamps (the stamp ceremony re-reads, re-hashes, and seals), but
//! the catalog stops presenting the add's attribution as covering the
//! current bytes — the row reads `stale` and the door is remove + re-add.

use std::path::{Path, PathBuf};

use crate::skill_state::UserTemplateRecord;
use crate::user_skills::UserSkillRefusal;

/// Request-body cap for the template add route
/// (`POST /api/agenda/definitions`), pinned by the routes table test —
/// derived from the S4 skill cap (one prose budget for both libraries);
/// the library add re-checks it so the tunnel lane (params-as-body) is
/// equally bounded.
pub(crate) const ADD_BODY_CAP_BYTES: usize = crate::user_skills::ADD_BODY_CAP_BYTES;

/// `<state_root>/automations/<name>/SKILL.md` — the personal library
/// path the shadow-resolution order reads first.
///
/// Drift between a record's sha256 and this file's current bytes is
/// judged in ONE place — the served catalog
/// (`agenda::definitions::definition_catalog`), against the same read
/// that serves the text — never re-derived here.
pub(crate) fn user_template_md_path_in(state_root: &Path, name: &str) -> PathBuf {
    crate::agenda::automations_dir_in(state_root)
        .join(name)
        .join("SKILL.md")
}

/// Add one personal definition: cap + slug walls, then the REAL definition
/// intake ([`crate::agenda::parse_definition`] — its error verbatim),
/// then the collision walls (an existing record refuses remove-first; a
/// hand-placed directory refuses fail-closed; a HOUSE name is the one
/// accepted collision — the documented shadow mechanism). Accepted bytes
/// seal into the library (private, tmp + rename) and the record lands
/// last with the caller's gate-resolved attribution + sha256 — a library
/// write without a record is a hand-placed-looking orphan at worst, and
/// registration-last keeps a failed add retryable.
pub(crate) fn add_user_template_in(
    state_root: &Path,
    name: &str,
    skill_md: &str,
    added_by: crate::skill_state::DisabledRecord,
) -> Result<UserTemplateRecord, UserSkillRefusal> {
    if skill_md.len() > ADD_BODY_CAP_BYTES {
        return Err(UserSkillRefusal::Invalid {
            message: format!(
                "definition SKILL.md is {} bytes — the definition cap is {} KiB",
                skill_md.len(),
                ADD_BODY_CAP_BYTES / 1024
            ),
        });
    }
    if !crate::agenda::valid_slug(name) {
        return Err(UserSkillRefusal::Invalid {
            message: format!(
                "definition name {name:?} violates the slug grammar (1..=64 lowercase \
                 alphanumerics with single interior hyphens)"
            ),
        });
    }
    // The ONE validator, at add time: whatever would refuse at stamp
    // time refuses here, with the parser's own reason.
    crate::agenda::parse_definition(skill_md, name)
        .map_err(|error| UserSkillRefusal::Invalid { message: error })?;
    if crate::skill_state::user_template_records_in(state_root)
        .iter()
        .any(|record| record.name == name)
    {
        return Err(UserSkillRefusal::NameCollision {
            message: format!(
                "a personal definition named '{name}' already exists — remove it first to \
                 replace its bytes"
            ),
        });
    }
    let dir = crate::agenda::automations_dir_in(state_root).join(name);
    if dir.exists() {
        // Hand-placed personal definitions are owner-owned: the
        // dashboard lane never writes into a directory it did not
        // create (the S4 unmarked-directory law).
        return Err(UserSkillRefusal::NameCollision {
            message: format!(
                "a personal definition directory named '{name}' already exists in the \
                 automations library (hand-placed, not dashboard-added) — manage it by \
                 hand or pick another name"
            ),
        });
    }

    intendant_core::state_paths::create_private_dir_all(&dir).map_err(|error| {
        UserSkillRefusal::Io {
            message: format!("create {}: {error}", dir.display()),
        }
    })?;
    let path = user_template_md_path_in(state_root, name);
    let tmp = dir.join("SKILL.md.tmp");
    intendant_core::state_paths::write_private_file(&tmp, skill_md.as_bytes()).map_err(
        |error| UserSkillRefusal::Io {
            message: format!("write {}: {error}", tmp.display()),
        },
    )?;
    std::fs::rename(&tmp, &path).map_err(|error| UserSkillRefusal::Io {
        message: format!("rename {}: {error}", path.display()),
    })?;

    let record = UserTemplateRecord {
        name: name.to_string(),
        added_by,
        sha256: crate::agenda::digest_bytes(skill_md.as_bytes()),
        foreign: serde_json::Map::new(),
    };
    crate::skill_state::upsert_user_template_record_in(state_root, record.clone())
        .map_err(|message| UserSkillRefusal::Io { message })?;
    Ok(record)
}

/// Remove one dashboard-added template: delete the library directory,
/// then the registry record (record last keeps a failed removal
/// retryable, exactly the S4 order). The record is the target — never
/// house bytes (which live under `.house/` and are untouchable here) and
/// never a hand-placed directory (no record ⇒ the refusal names the
/// managing door). Removing a house-named record un-shadows the house
/// twin: resolution falls back to the materialized house copy.
pub(crate) fn remove_user_template_in(
    state_root: &Path,
    name: &str,
) -> Result<UserTemplateRecord, UserSkillRefusal> {
    let record = crate::skill_state::user_template_records_in(state_root)
        .into_iter()
        .find(|record| record.name == name);
    let Some(record) = record else {
        if user_template_md_path_in(state_root, name).is_file() {
            return Err(UserSkillRefusal::WrongLane {
                message: format!(
                    "personal definition '{name}' was not added from the dashboard — its \
                     directory is hand-placed in the automations library; manage it by hand"
                ),
            });
        }
        if crate::agenda::is_house_definition_name(name) {
            return Err(UserSkillRefusal::WrongLane {
                message: format!(
                    "house definition '{name}' ships in the binary — there is nothing to \
                     remove; add a personal definition of the same name to shadow it instead"
                ),
            });
        }
        return Err(UserSkillRefusal::UnknownSkill {
            message: format!(
                "unknown definition '{name}' — not a house definition or a dashboard-added \
                 personal one"
            ),
        });
    };

    let dir = crate::agenda::automations_dir_in(state_root).join(name);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(UserSkillRefusal::Io {
                message: format!("remove {}: {error}", dir.display()),
            })
        }
    }
    crate::skill_state::remove_user_template_record_in(state_root, name)
        .map_err(|message| UserSkillRefusal::Io { message })?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid ACTION definition for a given name — one node,
    /// stamp-time cadence left to the sheet.
    fn action_md(name: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: a test automation\n---\n\nShared \
             orientation.\n\n## node: {name}\n\n```toml\ntitle = \"Test node\"\n```\n\nDo \
             the thing.\n"
        )
    }

    fn stamp(principal: &str) -> crate::skill_state::DisabledRecord {
        crate::skill_state::DisabledRecord {
            principal: Some(principal.to_string()),
            kind: Some("dashboard".to_string()),
            at_ms: 11,
            foreign: serde_json::Map::new(),
        }
    }

    /// The library round-trip: add validates through the REAL parser,
    /// seals bytes + attribution + sha256 into the `templates` record
    /// family, the file lands private at the personal-shadow home, and
    /// remove deletes both the record and the directory.
    #[test]
    fn add_records_attribution_and_sha_and_remove_deletes_the_library() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let md = action_md("my-automation");

        let record = add_user_template_in(root, "my-automation", &md, stamp("principal:me"))
            .expect("valid definition adds");
        assert_eq!(record.name, "my-automation");
        assert_eq!(record.added_by.principal.as_deref(), Some("principal:me"));
        assert_eq!(record.sha256, crate::agenda::digest_bytes(md.as_bytes()));
        assert_eq!(
            std::fs::read_to_string(user_template_md_path_in(root, "my-automation")).unwrap(),
            md
        );
        assert_eq!(
            crate::skill_state::user_template_records_in(root),
            vec![record.clone()]
        );

        // The stamp lane resolves the added definition at the personal
        // home — the file a stamp would seal IS the added file.
        let (resolved, provenance) =
            crate::agenda::resolve_definition(root, "my-automation").unwrap();
        assert_eq!(resolved, user_template_md_path_in(root, "my-automation"));
        assert_eq!(provenance, crate::agenda::DefinitionProvenance::Personal);

        // The library file is private where the platform enforces it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(user_template_md_path_in(root, "my-automation"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "library copy must be private");
        }

        let removed = remove_user_template_in(root, "my-automation").unwrap();
        assert_eq!(removed.name, "my-automation");
        assert!(crate::skill_state::user_template_records_in(root).is_empty());
        assert!(!crate::agenda::automations_dir_in(root)
            .join("my-automation")
            .exists());
    }

    /// Add-time validation IS the stamp-time parser: a definition the
    /// intake would refuse refuses here with the parser's own error, and
    /// nothing lands in the library or the registry.
    #[test]
    fn add_refuses_with_the_definition_parsers_own_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cases: &[(&str, &str)] = &[
            // A non-spec frontmatter key (the Amendment A1 wall).
            (
                "---\nname: bad-key\ndescription: d\nshape: action\n---\n\n## node: \
                 bad-key\n\n```toml\n```\n\nP.\n",
                "bad-key",
            ),
            // An unknown config-block key (deny_unknown_fields).
            (
                "---\nname: bad-config\ndescription: d\n---\n\n## node: \
                 bad-config\n\n```toml\nbudget = 5\n```\n\nP.\n",
                "bad-config",
            ),
            // No node section at all.
            (
                "---\nname: no-nodes\ndescription: d\n---\n\nProse only.\n",
                "no-nodes",
            ),
        ];
        for (md, name) in cases {
            let parser_error = crate::agenda::parse_definition(md, name).unwrap_err();
            let refusal = add_user_template_in(root, name, md, stamp("p")).unwrap_err();
            assert_eq!(
                refusal.message(),
                parser_error,
                "the add must serve the parser's own refusal"
            );
            assert_eq!(refusal.http_status(), 400);
            assert!(!crate::agenda::automations_dir_in(root).join(name).exists());
        }
        assert!(crate::skill_state::user_template_records_in(root).is_empty());

        // The pre-parse walls stay named: slug grammar and the byte cap.
        let refusal =
            add_user_template_in(root, "Bad_Slug", &action_md("bad-slug"), stamp("p")).unwrap_err();
        assert_eq!(
            refusal.message(),
            "definition name \"Bad_Slug\" violates the slug grammar (1..=64 lowercase \
             alphanumerics with single interior hyphens)"
        );
        let oversized = format!("{}{}", action_md("big"), "x".repeat(ADD_BODY_CAP_BYTES));
        let refusal = add_user_template_in(root, "big", &oversized, stamp("p")).unwrap_err();
        assert_eq!(
            refusal.message(),
            format!(
                "definition SKILL.md is {} bytes — the definition cap is 64 KiB",
                oversized.len()
            )
        );
    }

    /// The collision walls: an existing dashboard-added record refuses
    /// remove-first; a hand-placed directory refuses fail-closed (the
    /// dashboard never writes into a directory it did not create); a
    /// HOUSE name is accepted — that add is the documented shadow
    /// mechanism, and the sealed house bytes are untouched by it.
    #[test]
    fn collision_walls_and_the_house_shadow_exception() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        crate::agenda::materialize_house_definitions(root).unwrap();

        // Recorded collision.
        add_user_template_in(root, "mine", &action_md("mine"), stamp("p")).unwrap();
        let refusal =
            add_user_template_in(root, "mine", &action_md("mine"), stamp("p")).unwrap_err();
        assert_eq!(
            refusal.message(),
            "a personal definition named 'mine' already exists — remove it first to \
             replace its bytes"
        );
        assert_eq!(refusal.http_status(), 409);

        // Hand-placed collision: a directory the dashboard did not
        // create is never written into.
        let hand = crate::agenda::automations_dir_in(root).join("hand-made");
        std::fs::create_dir_all(&hand).unwrap();
        std::fs::write(hand.join("SKILL.md"), action_md("hand-made")).unwrap();
        let refusal = add_user_template_in(root, "hand-made", &action_md("hand-made"), stamp("p"))
            .unwrap_err();
        assert_eq!(
            refusal.message(),
            "a personal definition directory named 'hand-made' already exists in the \
             automations library (hand-placed, not dashboard-added) — manage it by \
             hand or pick another name"
        );
        assert_eq!(refusal.http_status(), 409);
        assert_eq!(
            std::fs::read_to_string(hand.join("SKILL.md")).unwrap(),
            action_md("hand-made"),
            "the hand-placed bytes are untouched"
        );

        // House-name add: ACCEPTED — the personal copy shadows the house
        // twin; the sealed house bytes never change.
        let house_path = root
            .join("automations")
            .join(".house")
            .join("triage")
            .join("SKILL.md");
        let house_before = std::fs::read_to_string(&house_path).unwrap();
        let shadow_md = action_md("triage");
        add_user_template_in(root, "triage", &shadow_md, stamp("principal:me")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&house_path).unwrap(),
            house_before,
            "ADD never edits house bytes"
        );
        let (resolved, provenance) = crate::agenda::resolve_definition(root, "triage").unwrap();
        assert_eq!(resolved, user_template_md_path_in(root, "triage"));
        assert_eq!(provenance, crate::agenda::DefinitionProvenance::Personal);

        // Removing the shadow un-shadows: resolution falls back to the
        // sealed house copy, byte-identical.
        remove_user_template_in(root, "triage").unwrap();
        let (resolved, provenance) = crate::agenda::resolve_definition(root, "triage").unwrap();
        assert_eq!(resolved, house_path);
        assert_eq!(provenance, crate::agenda::DefinitionProvenance::House);
        assert_eq!(std::fs::read_to_string(&house_path).unwrap(), house_before);
    }

    /// Remove's per-kind walls: house names refuse toward the shadow
    /// mechanism (409), hand-placed directories refuse toward "manage it
    /// by hand" (409), unknown names 404 — and none of them touch the
    /// registry or the library.
    #[test]
    fn remove_refusals_are_named_per_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        crate::agenda::materialize_house_definitions(root).unwrap();

        let refusal = remove_user_template_in(root, "triage").unwrap_err();
        assert_eq!(
            refusal.message(),
            "house definition 'triage' ships in the binary — there is nothing to \
             remove; add a personal definition of the same name to shadow it instead"
        );
        assert_eq!(refusal.http_status(), 409);

        let hand = crate::agenda::automations_dir_in(root).join("hand-made");
        std::fs::create_dir_all(&hand).unwrap();
        std::fs::write(hand.join("SKILL.md"), action_md("hand-made")).unwrap();
        let refusal = remove_user_template_in(root, "hand-made").unwrap_err();
        assert_eq!(
            refusal.message(),
            "personal definition 'hand-made' was not added from the dashboard — its \
             directory is hand-placed in the automations library; manage it by hand"
        );
        assert_eq!(refusal.http_status(), 409);
        assert!(hand.join("SKILL.md").is_file(), "hand-placed files survive");

        // A hand-placed shadow of a house name refuses down the SAME
        // hand-placed wall (the record is the dashboard's only claim).
        let shadow = crate::agenda::automations_dir_in(root).join("triage");
        std::fs::create_dir_all(&shadow).unwrap();
        std::fs::write(shadow.join("SKILL.md"), action_md("triage")).unwrap();
        let refusal = remove_user_template_in(root, "triage").unwrap_err();
        assert!(
            refusal.message().contains("hand-placed"),
            "{}",
            refusal.message()
        );

        let refusal = remove_user_template_in(root, "no-such").unwrap_err();
        assert_eq!(
            refusal.message(),
            "unknown definition 'no-such' — not a house definition or a dashboard-added \
             personal one"
        );
        assert_eq!(refusal.http_status(), 404);
    }

    /// Drift semantics (S4's conventions on the template family): a
    /// hand-edited recorded file and a record whose file is gone never
    /// gate removal — remove stays the door out of drift. (The
    /// `ok`/`stale`/`missing` rendering is the served catalog's,
    /// computed against the same read that serves the text — pinned in
    /// `agenda::definitions`.)
    #[test]
    fn drift_never_gates_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let record =
            add_user_template_in(root, "mine", &action_md("mine"), stamp("principal:me")).unwrap();

        // A hand edit: the recorded sha no longer covers the file bytes.
        std::fs::write(
            user_template_md_path_in(root, "mine"),
            action_md("mine").replace("Do the thing.", "Do a different thing."),
        )
        .unwrap();
        let drifted = std::fs::read_to_string(user_template_md_path_in(root, "mine")).unwrap();
        assert_ne!(
            crate::agenda::digest_bytes(drifted.as_bytes()),
            record.sha256,
            "the edit really drifted the bytes past the record"
        );

        // Remove deletes the drifted copy + record (the named remedy).
        remove_user_template_in(root, "mine").unwrap();
        assert!(crate::skill_state::user_template_records_in(root).is_empty());

        // Missing: record without a file (a failed removal's residue, or
        // a hand-deleted directory) — remove still clears it.
        let orphan = UserTemplateRecord {
            name: "orphan".to_string(),
            added_by: stamp("p"),
            sha256: "0".repeat(64),
            foreign: serde_json::Map::new(),
        };
        crate::skill_state::upsert_user_template_record_in(root, orphan).unwrap();
        let removed = remove_user_template_in(root, "orphan").unwrap();
        assert_eq!(removed.name, "orphan");
        assert!(crate::skill_state::user_template_records_in(root).is_empty());
    }
}
