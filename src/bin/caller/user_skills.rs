//! The daemon-owned USER skill library (skills/plugins unification S4):
//! dashboard-added skills' source of truth under
//! `<state_root>/skills/<name>/SKILL.md`, registered in
//! [`crate::skill_state`] with the add's gate-resolved attribution and
//! the sha256 of the accepted bytes.
//!
//! **Add is an authority-bearing act**: a skill is instructions every
//! backend on this machine will obey — its description is injected
//! ambiently into every native session's catalog and its body loads on
//! invoke. The ONLY input lanes are pasted or uploaded SKILL.md bytes
//! (both land the same request field); there is deliberately NO
//! URL/marketplace fetch and NO local-path adoption lane — provenance
//! stays "the owner held these bytes", and a live directory the daemon
//! re-reads forever is a mutable-authority hazard (intake §3a, ruling
//! H4; parked vocabulary refuses by name until its own ruling).
//!
//! **Why a library and not a direct write into the discovery roots**: an
//! unmarked root directory is user-owned forever — the installer could
//! never deactivate, refresh, or remove it. Library + marked
//! materialization (`source: user (dashboard-added)`, the third marker
//! class in [`crate::skill_install`]) is the only shape that yields the
//! full lifecycle without weakening the user-owned law.
//!
//! Validation is fail-closed with named refusals: frontmatter must parse,
//! `name` must equal the submitted slug, the slug grammar holds, the
//! description is non-empty, and a name colliding with any builtin,
//! plugin payload, or existing user skill refuses by name. The recorded
//! sha256 is the provenance seal: materialization and re-enable both
//! re-verify the library bytes against it (ruling R3), so a hand-edited
//! library copy is never re-taught — it reads `stale` and the door is
//! remove + re-add.

use std::path::{Path, PathBuf};

use crate::skill_state::{DisabledRecord, UserSkillRecord};

/// Request-body cap for the add route (`POST /api/skills`), pinned by the
/// routes table test. 64 KiB is generous for prose; the library add
/// re-checks it so the tunnel lane (params-as-body) is equally bounded.
pub(crate) const ADD_BODY_CAP_BYTES: usize = 64 * 1024;

/// One materializable user skill: the library bytes the installer copies
/// into both discovery roots (verified against the registry sha before
/// they ever enter a desired set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserSkillPayload {
    pub(crate) name: String,
    pub(crate) skill_md: String,
}

/// A refused or failed library mutation. Text is composed here — beside
/// the validation — so the per-kind shapes are pinned in one place (the
/// S3 refusal conventions: 409 wrong-lane naming the door, 404 unknown).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UserSkillRefusal {
    /// The submitted bytes fail validation (parse error, slug grammar,
    /// name mismatch, empty description, size).
    Invalid { message: String },
    /// The name collides with a builtin, plugin payload, or existing
    /// user skill — refused by name.
    NameCollision { message: String },
    /// The gesture aimed at a kind another door manages (remove of a
    /// builtin or plugin payload) — the refusal names that door.
    WrongLane { message: String },
    /// Not a name any registry knows.
    UnknownSkill { message: String },
    /// Filesystem/state failure.
    Io { message: String },
}

impl UserSkillRefusal {
    pub(crate) fn http_status(&self) -> u16 {
        match self {
            Self::Invalid { .. } => 400,
            Self::NameCollision { .. } | Self::WrongLane { .. } => 409,
            Self::UnknownSkill { .. } => 404,
            Self::Io { .. } => 500,
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Invalid { message }
            | Self::NameCollision { message }
            | Self::WrongLane { message }
            | Self::UnknownSkill { message }
            | Self::Io { message } => message,
        }
    }

    fn io(message: impl Into<String>) -> Self {
        Self::Io {
            message: message.into(),
        }
    }
}

/// Why a library copy fails verification against its registry record.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UserLibraryIssue {
    /// The file exists but its bytes no longer hash to the recorded
    /// sha256 (a hand edit — the provenance seal is broken).
    Stale,
    /// No library SKILL.md on disk.
    Missing,
}

impl std::fmt::Display for UserLibraryIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stale => write!(f, "stale (bytes no longer match the recorded sha256)"),
            Self::Missing => write!(f, "missing (no SKILL.md in the library)"),
        }
    }
}

/// `<state_root>/skills/<name>` — the library directory for one skill.
/// Sibling of the registry file (`skills/state.json`); a collision is
/// impossible because the slug grammar admits no dots.
pub(crate) fn user_skill_dir_in(state_root: &Path, name: &str) -> PathBuf {
    state_root.join("skills").join(name)
}

/// `<state_root>/skills/<name>/SKILL.md`.
pub(crate) fn user_skill_md_path_in(state_root: &Path, name: &str) -> PathBuf {
    user_skill_dir_in(state_root, name).join("SKILL.md")
}

/// The raw library bytes for one skill, if present (the catalog reads
/// these for the row's description even when the copy is stale — display
/// is not teaching).
pub(crate) fn user_skill_library_bytes_in(state_root: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(user_skill_md_path_in(state_root, name)).ok()
}

/// Verify one registry record's library copy against its recorded
/// sha256. Re-enable and every materialization pass go through this: the
/// daemon only teaches bytes the attributed record attests.
pub(crate) fn verify_user_library_in(
    state_root: &Path,
    record: &UserSkillRecord,
) -> Result<(), UserLibraryIssue> {
    let Some(bytes) = user_skill_library_bytes_in(state_root, &record.name) else {
        return Err(UserLibraryIssue::Missing);
    };
    if crate::agenda::digest_bytes(bytes.as_bytes()) == record.sha256 {
        Ok(())
    } else {
        Err(UserLibraryIssue::Stale)
    }
}

/// The row-facing library status vocabulary.
pub(crate) fn user_library_status_in(state_root: &Path, record: &UserSkillRecord) -> &'static str {
    match verify_user_library_in(state_root, record) {
        Ok(()) => "ok",
        Err(UserLibraryIssue::Stale) => "stale",
        Err(UserLibraryIssue::Missing) => "missing",
    }
}

/// Every registry record whose library copy verifies, as installable
/// payloads. Drifted or missing copies are EXCLUDED fail-closed — they
/// leave the desired set, so the next sweep clears their root copies
/// (the row's `library` status names why). The disabled-set is NOT
/// subtracted here: the installer subtracts it on every pass, so the set
/// outranks the sweep at that layer for user skills exactly as for
/// builtins.
pub(crate) fn active_user_skill_payloads_in(state_root: &Path) -> Vec<UserSkillPayload> {
    crate::skill_state::user_skill_records_in(state_root)
        .into_iter()
        .filter_map(|record| {
            verify_user_library_in(state_root, &record).ok()?;
            let skill_md = user_skill_library_bytes_in(state_root, &record.name)?;
            Some(UserSkillPayload {
                name: record.name,
                skill_md,
            })
        })
        .collect()
}

/// [`active_user_skill_payloads_in`] against the daemon's own state root.
pub(crate) fn active_user_skill_payloads() -> Vec<UserSkillPayload> {
    active_user_skill_payloads_in(&intendant_core::state_paths::intendant_home())
}

/// The §3a validation set, fail-closed with named refusals. Pure: no
/// filesystem access, no registry mutation.
fn validate_user_skill(name: &str, skill_md: &str) -> Result<(), UserSkillRefusal> {
    if skill_md.len() > ADD_BODY_CAP_BYTES {
        return Err(UserSkillRefusal::Invalid {
            message: format!(
                "SKILL.md is {} bytes — the user-skill cap is {} KiB",
                skill_md.len(),
                ADD_BODY_CAP_BYTES / 1024
            ),
        });
    }
    if !crate::agenda::valid_slug(name) {
        return Err(UserSkillRefusal::Invalid {
            message: format!(
                "skill name {name:?} violates the slug grammar (1..=64 lowercase \
                 alphanumerics with single interior hyphens)"
            ),
        });
    }
    let (config, _body) = intendant_core::skills::parse_skill_md(skill_md, Path::new(name))
        .map_err(|error| UserSkillRefusal::Invalid {
            message: format!("SKILL.md does not parse: {error}"),
        })?;
    if config.name != name {
        return Err(UserSkillRefusal::Invalid {
            message: format!(
                "frontmatter name {:?} must equal the submitted skill name {name:?}",
                config.name
            ),
        });
    }
    if config.description.trim().is_empty() {
        return Err(UserSkillRefusal::Invalid {
            message: "description must be non-empty — it becomes the ambient catalog line \
                      injected into every session"
                .to_string(),
        });
    }
    Ok(())
}

/// The collision wall: a user skill may never take a name any registry
/// already owns (builtin, plugin payload, or existing user record) — the
/// refusal names the holder.
fn refuse_name_collision_in(state_root: &Path, name: &str) -> Result<(), UserSkillRefusal> {
    match crate::skill_state::skill_lifecycle_in(state_root, name) {
        crate::skill_state::SkillLifecycle::Builtin(_) => Err(UserSkillRefusal::NameCollision {
            message: format!(
                "skill name '{name}' collides with the builtin skill of the same name — \
                 builtins ship in the binary; pick another name"
            ),
        }),
        crate::skill_state::SkillLifecycle::PluginManaged(plugin) => {
            Err(UserSkillRefusal::NameCollision {
                message: format!(
                    "skill name '{name}' collides with a payload of plugin '{}' ({}) — \
                     pick another name",
                    plugin.id, plugin.display_name
                ),
            })
        }
        crate::skill_state::SkillLifecycle::User(_) => Err(UserSkillRefusal::NameCollision {
            message: format!(
                "a user skill named '{name}' already exists — remove it first to replace \
                 its bytes"
            ),
        }),
        crate::skill_state::SkillLifecycle::Unknown => Ok(()),
    }
}

/// Add one user skill: validate, refuse collisions by name, seal the
/// bytes into the library (private, tmp + rename), and register the
/// record with the caller's gate-resolved attribution and the sha256 of
/// the accepted bytes. The caller reconciles the installed roots
/// afterwards so materialization lands in the same request. Registration
/// is last: a library write without a record is invisible (not listed,
/// not materialized) and simply overwritten on retry.
pub(crate) fn add_user_skill_in(
    state_root: &Path,
    name: &str,
    skill_md: &str,
    added_by: DisabledRecord,
) -> Result<UserSkillRecord, UserSkillRefusal> {
    validate_user_skill(name, skill_md)?;
    refuse_name_collision_in(state_root, name)?;

    let dir = user_skill_dir_in(state_root, name);
    intendant_core::state_paths::create_private_dir_all(&dir)
        .map_err(|error| UserSkillRefusal::io(format!("create {}: {error}", dir.display())))?;
    let path = user_skill_md_path_in(state_root, name);
    let tmp = dir.join("SKILL.md.tmp");
    intendant_core::state_paths::write_private_file(&tmp, skill_md.as_bytes())
        .map_err(|error| UserSkillRefusal::io(format!("write {}: {error}", tmp.display())))?;
    std::fs::rename(&tmp, &path)
        .map_err(|error| UserSkillRefusal::io(format!("rename {}: {error}", path.display())))?;

    let record = UserSkillRecord {
        name: name.to_string(),
        added_by,
        sha256: crate::agenda::digest_bytes(skill_md.as_bytes()),
        foreign: serde_json::Map::new(),
    };
    crate::skill_state::upsert_user_skill_record_in(state_root, record.clone())
        .map_err(UserSkillRefusal::io)?;
    Ok(record)
}

/// Remove one user skill: delete the library copy, then the registry
/// record (record last keeps a failed removal retryable — the record
/// still classifies as user, and the already-deleted library reads as
/// missing). The caller reconciles the installed roots afterwards, which
/// sweeps the marked copies from both discovery roots — never an
/// unmarked user-owned twin (the installer's ownership law).
///
/// The record is the target, not the row classification: a record
/// shadowed by a later-shipping builtin of the same name stays removable
/// (its shipped twin is untouched — the desired set already prefers
/// shipped payloads). Builtin and plugin names without a record refuse
/// toward their own lifecycle door; unknown names refuse by name.
pub(crate) fn remove_user_skill_in(
    state_root: &Path,
    name: &str,
) -> Result<UserSkillRecord, UserSkillRefusal> {
    let record = crate::skill_state::user_skill_records_in(state_root)
        .into_iter()
        .find(|record| record.name == name);
    let Some(record) = record else {
        return match crate::skill_state::skill_lifecycle_in(state_root, name) {
            crate::skill_state::SkillLifecycle::Builtin(_) => Err(UserSkillRefusal::WrongLane {
                message: format!(
                    "builtin skill '{name}' cannot be removed — its bytes ship in the \
                     binary; deactivate it instead"
                ),
            }),
            crate::skill_state::SkillLifecycle::PluginManaged(plugin) => {
                Err(UserSkillRefusal::WrongLane {
                    message: format!(
                        "a plugin-materialized skill is managed by its plugin — toggle '{}' ({})",
                        plugin.id, plugin.display_name
                    ),
                })
            }
            _ => Err(UserSkillRefusal::UnknownSkill {
                message: format!(
                    "unknown skill '{name}' — not a builtin, bundled plugin payload, or \
                     user-added skill"
                ),
            }),
        };
    };

    let dir = user_skill_dir_in(state_root, name);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(UserSkillRefusal::io(format!(
                "remove {}: {error}",
                dir.display()
            )))
        }
    }
    crate::skill_state::remove_user_skill_record_in(state_root, name)
        .map_err(UserSkillRefusal::io)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_MD: &str = "---\nname: my-notes\ndescription: teach a thing\n---\nThe body.\n";

    fn stamp(principal: &str) -> DisabledRecord {
        DisabledRecord {
            principal: Some(principal.to_string()),
            kind: Some("dashboard".to_string()),
            at_ms: 7,
            foreign: serde_json::Map::new(),
        }
    }

    /// The library round-trip at the record layer: add seals bytes +
    /// attribution + sha256, the payload loader serves exactly the sealed
    /// bytes, remove deletes both the record and the library copy.
    #[test]
    fn add_records_attribution_and_sha_and_remove_deletes_the_library() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let record = add_user_skill_in(root, "my-notes", VALID_MD, stamp("principal:me")).unwrap();
        assert_eq!(record.name, "my-notes");
        assert_eq!(record.added_by.principal.as_deref(), Some("principal:me"));
        assert_eq!(
            record.sha256,
            crate::agenda::digest_bytes(VALID_MD.as_bytes())
        );
        assert_eq!(
            std::fs::read_to_string(user_skill_md_path_in(root, "my-notes")).unwrap(),
            VALID_MD
        );
        assert_eq!(
            crate::skill_state::user_skill_records_in(root),
            vec![record.clone()]
        );
        assert_eq!(user_library_status_in(root, &record), "ok");
        assert_eq!(
            active_user_skill_payloads_in(root),
            vec![UserSkillPayload {
                name: "my-notes".to_string(),
                skill_md: VALID_MD.to_string(),
            }]
        );

        // The library file is private where the platform enforces it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(user_skill_md_path_in(root, "my-notes"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "library copy must be private");
        }

        let removed = remove_user_skill_in(root, "my-notes").unwrap();
        assert_eq!(removed.name, "my-notes");
        assert!(crate::skill_state::user_skill_records_in(root).is_empty());
        assert!(!user_skill_dir_in(root, "my-notes").exists());
        assert!(active_user_skill_payloads_in(root).is_empty());
    }

    /// The §3a validation set refuses named, and nothing lands in the
    /// library or the registry on any refusal.
    #[test]
    fn validation_refusals_are_named_and_leave_no_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cases: Vec<(&str, &str, &str)> = vec![
            (
                "Bad_Slug",
                VALID_MD,
                "skill name \"Bad_Slug\" violates the slug grammar (1..=64 lowercase \
                 alphanumerics with single interior hyphens)",
            ),
            (
                "my-notes",
                "no frontmatter here",
                "SKILL.md does not parse: my-notes: missing YAML frontmatter (must start with ---)",
            ),
            (
                "my-notes",
                "---\nname: other-name\ndescription: d\n---\nbody\n",
                "frontmatter name \"other-name\" must equal the submitted skill name \"my-notes\"",
            ),
            (
                "my-notes",
                "---\nname: my-notes\ndescription: \"  \"\n---\nbody\n",
                "description must be non-empty — it becomes the ambient catalog line \
                 injected into every session",
            ),
        ];
        for (name, md, expected) in cases {
            let refusal = add_user_skill_in(root, name, md, stamp("p")).unwrap_err();
            assert_eq!(refusal.message(), expected);
            assert_eq!(refusal.http_status(), 400, "{expected}");
        }
        let oversized = format!(
            "---\nname: my-notes\ndescription: d\n---\n{}",
            "x".repeat(ADD_BODY_CAP_BYTES)
        );
        let refusal = add_user_skill_in(root, "my-notes", &oversized, stamp("p")).unwrap_err();
        assert_eq!(
            refusal.message(),
            format!("SKILL.md is {} bytes — the user-skill cap is 64 KiB", oversized.len())
        );

        assert!(crate::skill_state::user_skill_records_in(root).is_empty());
        assert!(!user_skill_dir_in(root, "my-notes").exists());
    }

    /// Collision refusals name the holder for all three registries
    /// (intake §3a: the `plugin_registry` collision invariant extended to
    /// the add lane).
    #[test]
    fn name_collisions_refuse_by_name_for_all_three_registries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let builtin = crate::builtin_skills::BUILTIN_SKILLS[0].name;
        let md = format!("---\nname: {builtin}\ndescription: d\n---\nbody\n");
        let refusal = add_user_skill_in(root, builtin, &md, stamp("p")).unwrap_err();
        assert_eq!(
            refusal.message(),
            format!(
                "skill name '{builtin}' collides with the builtin skill of the same name — \
                 builtins ship in the binary; pick another name"
            )
        );
        assert_eq!(refusal.http_status(), 409);

        let plugin = &crate::plugin_registry::BUNDLED_PLUGINS[0];
        let payload = plugin.skills[0].name;
        let md = format!("---\nname: {payload}\ndescription: d\n---\nbody\n");
        let refusal = add_user_skill_in(root, payload, &md, stamp("p")).unwrap_err();
        assert_eq!(
            refusal.message(),
            format!(
                "skill name '{payload}' collides with a payload of plugin '{}' ({}) — \
                 pick another name",
                plugin.id, plugin.display_name
            )
        );
        assert_eq!(refusal.http_status(), 409);

        add_user_skill_in(root, "my-notes", VALID_MD, stamp("p")).unwrap();
        let refusal = add_user_skill_in(root, "my-notes", VALID_MD, stamp("p")).unwrap_err();
        assert_eq!(
            refusal.message(),
            "a user skill named 'my-notes' already exists — remove it first to replace \
             its bytes"
        );
        assert_eq!(refusal.http_status(), 409);
    }

    /// Remove's per-kind walls (the S3 refusal conventions): builtins and
    /// plugin payloads refuse 409 toward their own door, unknown names
    /// refuse 404, and none of them touch the registry.
    #[test]
    fn remove_refusals_are_named_per_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let builtin = crate::builtin_skills::BUILTIN_SKILLS[0].name;
        let refusal = remove_user_skill_in(root, builtin).unwrap_err();
        assert_eq!(
            refusal.message(),
            format!(
                "builtin skill '{builtin}' cannot be removed — its bytes ship in the \
                 binary; deactivate it instead"
            )
        );
        assert_eq!(refusal.http_status(), 409);

        let plugin = &crate::plugin_registry::BUNDLED_PLUGINS[0];
        let payload = plugin.skills[0].name;
        let refusal = remove_user_skill_in(root, payload).unwrap_err();
        assert_eq!(
            refusal.message(),
            format!(
                "a plugin-materialized skill is managed by its plugin — toggle '{}' ({})",
                plugin.id, plugin.display_name
            )
        );
        assert_eq!(refusal.http_status(), 409);

        let refusal = remove_user_skill_in(root, "no-such-skill").unwrap_err();
        assert_eq!(
            refusal.message(),
            "unknown skill 'no-such-skill' — not a builtin, bundled plugin payload, or \
             user-added skill"
        );
        assert_eq!(refusal.http_status(), 404);
    }

    /// Drift semantics: a hand-edited or deleted library copy leaves the
    /// payload set fail-closed (so the sweep clears the roots), surfaces
    /// as `stale`/`missing`, and a shadowed record (a builtin later
    /// shipping the same name) remains removable — remove targets the
    /// record, never the shipped twin.
    #[test]
    fn drifted_copies_are_excluded_and_shadowed_records_stay_removable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let record = add_user_skill_in(root, "my-notes", VALID_MD, stamp("p")).unwrap();

        std::fs::write(
            user_skill_md_path_in(root, "my-notes"),
            "---\nname: my-notes\ndescription: edited\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(user_library_status_in(root, &record), "stale");
        assert!(active_user_skill_payloads_in(root).is_empty());
        assert_eq!(
            verify_user_library_in(root, &record),
            Err(UserLibraryIssue::Stale)
        );

        std::fs::remove_dir_all(user_skill_dir_in(root, "my-notes")).unwrap();
        assert_eq!(user_library_status_in(root, &record), "missing");
        assert_eq!(
            verify_user_library_in(root, &record),
            Err(UserLibraryIssue::Missing)
        );

        // A shadowed record: hand-write a record under a builtin's name
        // (the add-refusal can never mint one, but a later daemon may
        // ship a builtin taking a recorded name). The record — not the
        // builtin — is what remove deletes.
        let builtin = crate::builtin_skills::BUILTIN_SKILLS[0].name;
        crate::skill_state::upsert_user_skill_record_in(
            root,
            UserSkillRecord {
                name: builtin.to_string(),
                added_by: DisabledRecord::default(),
                sha256: "0".repeat(64),
                foreign: serde_json::Map::new(),
            },
        )
        .unwrap();
        assert!(matches!(
            crate::skill_state::skill_lifecycle_in(root, builtin),
            crate::skill_state::SkillLifecycle::Builtin(_)
        ));
        let removed = remove_user_skill_in(root, builtin).unwrap();
        assert_eq!(removed.name, builtin);
        assert!(crate::skill_state::user_skill_records_in(root).is_empty());
    }
}
