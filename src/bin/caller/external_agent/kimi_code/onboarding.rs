//! Kimi Code onboarding state: the `config.toml` provider half of a sign-in.
//!
//! Kimi's own `/login` writes TWO things: the OAuth credential
//! (`credentials/kimi-code.json`) and the managed provider + model entries in
//! `config.toml` (`provisionManagedKimiCodeConfig` fetches the live model
//! lineup and applies `applyManagedKimiCodeConfig`; the fresh-install stub
//! says it outright: "Login will populate managed Kimi provider and model
//! entries"). A home with a credential but no provider table is *signed in
//! but unonboarded*: every session dies at the first prompts call with wire
//! code 40110 — "no provider configured; complete onboarding via /login or
//! the providers endpoint" (`AuthSummaryService.ensureReady`, which throws
//! exactly when the config's provider table is empty). The 2026-08-11 owner
//! incident (card 01KZR9YHTX) was Intendant's sign-in ceremony syncing only
//! the credential half and discarding the provider entries the ceremony-home
//! login had written.
//!
//! This module owns both halves of the fix:
//! - [`providers_configured`] — the daemon-side mirror of `ensureReady`'s
//!   gate, used by the ceremony's completion verdict and the dashboard's
//!   partial-state probe.
//! - [`complete_managed_onboarding`] — a TOML-aware merge of the managed
//!   entries the ceremony-home login wrote into the primary home's
//!   `config.toml`, mirroring `applyManagedKimiCodeConfig`'s ownership
//!   semantics (verified against the kimi 0.34.0 binary): the managed
//!   provider record, its model aliases, and the two moonshot service
//!   entries are upstream-owned; user providers, user model aliases, user
//!   extras inside managed aliases (`mergeRefreshedModelAlias`'s
//!   `userExtras` + `overrides`), a preservable `default_model`, and an
//!   existing `[thinking]` table are never clobbered.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table};

/// Kimi's managed OAuth provider id (`KIMI_CODE_PROVIDER_NAME` in the CLI).
pub(crate) const MANAGED_KIMI_PROVIDER: &str = "managed:kimi-code";

/// The two `[services.*]` entries `applyManagedKimiCodeConfig` writes and
/// `applyManagedKimiCodeLogoutConfig` deletes — managed-owned, so the
/// completion merge adopts the ceremony's copies while preserving any other
/// service the user declared.
const MANAGED_SERVICE_KEYS: &[&str] = &["moonshot_search", "moonshot_fetch"];

/// The remote-owned model-alias fields (`MANAGED_KIMI_MODEL_FIELDS` in kimi
/// 0.34.0's `model-alias-merge.ts`, translated to the TOML writer's
/// snake_case). `mergeRefreshedModelAlias` keeps every OTHER key on an
/// existing alias as a user extra (plus a user `overrides` table verbatim)
/// and takes these from upstream only — which also drops a stale managed
/// field the upstream no longer sends.
const MANAGED_MODEL_FIELDS: &[&str] = &[
    "provider",
    "model",
    "max_context_size",
    "capabilities",
    "display_name",
    "protocol",
    "beta_api",
    "adaptive_thinking",
    "support_efforts",
    "default_effort",
];

pub(crate) fn kimi_config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

/// The daemon-side mirror of `AuthSummaryService.ensureReady`'s provider
/// gate: `Some(true)` when `config.toml` declares at least one provider (any
/// provider — a BYOK custom provider passes kimi's gate exactly like the
/// managed one), `Some(false)` when the config is absent, empty, or has an
/// empty provider table (the fresh-install stub), `None` when the config
/// exists but cannot be read or parsed — unknown, not a verdict.
pub(crate) fn providers_configured(home: &Path) -> Option<bool> {
    let text = match fs::read_to_string(kimi_config_path(home)) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    let document = match text.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(_) => return None,
    };
    Some(
        document
            .get("providers")
            .and_then(Item::as_table_like)
            .is_some_and(|providers| !providers.is_empty()),
    )
}

/// Whether an isolated ceremony home's login finished its provisioning half:
/// the managed provider record exists and at least one model alias points at
/// it. This is the ceremony's completion gate — a login process that has
/// written its credential but not yet its provider entries is still running,
/// and promoting at that instant would recreate the incident state in the
/// primary home.
pub(crate) fn ceremony_home_provisioned(ceremony_home: &Path) -> bool {
    let Ok(text) = fs::read_to_string(kimi_config_path(ceremony_home)) else {
        return false;
    };
    let Ok(document) = text.parse::<DocumentMut>() else {
        return false;
    };
    document
        .get("providers")
        .and_then(Item::as_table_like)
        .is_some_and(|providers| providers.get(MANAGED_KIMI_PROVIDER).is_some())
        && !managed_model_aliases(&document).is_empty()
}

/// Every `[models.<key>]` alias whose `provider` is the managed provider.
fn managed_model_aliases(document: &DocumentMut) -> Vec<(String, Table)> {
    let Some(models) = document.get("models").and_then(Item::as_table_like) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|(key, item)| {
            let table = as_detached_table(item)?;
            (table.get("provider").and_then(Item::as_str) == Some(MANAGED_KIMI_PROVIDER))
                .then(|| (key.to_string(), table))
        })
        .collect()
}

/// A standalone copy of a table-like item (standard or inline), losing the
/// distinction — every table this merge adopts re-renders as a standard
/// `[section]` table in the primary document.
fn as_detached_table(item: &Item) -> Option<Table> {
    match item {
        Item::Table(table) => Some(table.clone()),
        Item::Value(value) => value.as_inline_table().map(|inline| {
            let mut table = inline.clone().into_table();
            table.set_implicit(false);
            table
        }),
        _ => None,
    }
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Merge the managed provider + model entries a completed ceremony-home login
/// wrote into the primary home's `config.toml`, then persist it atomically
/// with owner-private permissions.
///
/// The data is exactly what kimi's own provisioner fetched from the managed
/// endpoint minutes earlier and wrote with the account's live model lineup —
/// this merge replays that write against the primary with the same ownership
/// boundaries kimi's own code enforces:
/// - `providers."managed:kimi-code"`: adopted (managed-owned; kimi's logout
///   deletes it). Every other provider is untouched.
/// - managed model aliases: adopted per `mergeRefreshedModelAlias` — managed
///   fields from the ceremony, user extras and a user `overrides` table
///   preserved; primary managed aliases absent from the ceremony set are
///   removed (kimi's stale-alias rule). Aliases on other providers are
///   untouched.
/// - `default_model`: preserved when kimi's `canPreserveDefaultModel` would
///   preserve it (it names a ceremony managed alias, or an existing alias on
///   another provider); otherwise the ceremony's selection is adopted.
/// - `[thinking]`: adopted only when the primary has none — kimi recomputes
///   it at login, but overriding an explicit user table from a completion
///   merge is scarier than leaving it, and the per-session profile call
///   sets thinking anyway.
/// - `[services]`: the two moonshot entries adopted (managed-owned); every
///   other service preserved.
///
/// A primary `config.toml` that exists but does not parse is an error, never
/// a clobber: the file is user-owned and kimi's own login would fail its
/// read-modify-write the same way.
pub(crate) fn complete_managed_onboarding(
    primary_home: &Path,
    ceremony_home: &Path,
) -> io::Result<()> {
    let ceremony_text = fs::read_to_string(kimi_config_path(ceremony_home)).map_err(|error| {
        invalid_data(format!(
            "Kimi login finished without a readable ceremony config.toml: {error}"
        ))
    })?;
    let ceremony: DocumentMut = ceremony_text.parse().map_err(|error| {
        invalid_data(format!(
            "Kimi login wrote an unparseable ceremony config.toml: {error}"
        ))
    })?;
    let managed_provider = ceremony
        .get("providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(MANAGED_KIMI_PROVIDER))
        .and_then(as_detached_table)
        .ok_or_else(|| {
            invalid_data(format!(
                "Kimi login did not write the {MANAGED_KIMI_PROVIDER} provider entry"
            ))
        })?;
    let ceremony_aliases = managed_model_aliases(&ceremony);
    if ceremony_aliases.is_empty() {
        return Err(invalid_data(format!(
            "Kimi login wrote no model aliases for {MANAGED_KIMI_PROVIDER}"
        )));
    }

    let primary_path = kimi_config_path(primary_home);
    let primary_text = match fs::read_to_string(&primary_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut primary: DocumentMut = primary_text.parse().map_err(|error| {
        invalid_data(format!(
            "existing Kimi config.toml {} does not parse ({error}); refusing to rewrite it",
            primary_path.display()
        ))
    })?;

    // 1. The managed provider record.
    root_table(&mut primary, "providers").insert(
        MANAGED_KIMI_PROVIDER,
        Item::Table(managed_provider.clone()),
    );

    // 2. Drop primary managed aliases the ceremony lineup no longer carries.
    let ceremony_keys: std::collections::HashSet<&str> = ceremony_aliases
        .iter()
        .map(|(key, _)| key.as_str())
        .collect();
    let stale: Vec<String> = primary
        .get("models")
        .and_then(Item::as_table_like)
        .map(|models| {
            models
                .iter()
                .filter(|(key, item)| {
                    !ceremony_keys.contains(key)
                        && item
                            .as_table_like()
                            .and_then(|table| table.get("provider"))
                            .and_then(Item::as_str)
                            == Some(MANAGED_KIMI_PROVIDER)
                })
                .map(|(key, _)| key.to_string())
                .collect()
        })
        .unwrap_or_default();
    if let Some(models) = primary.get_mut("models").and_then(Item::as_table_like_mut) {
        for key in &stale {
            models.remove(key);
        }
    }

    // 3. Adopt the ceremony aliases, preserving user extras + overrides.
    let models = root_table(&mut primary, "models");
    for (key, ceremony_alias) in &ceremony_aliases {
        let merged = match models.get(key).and_then(as_detached_table) {
            Some(existing) => merge_refreshed_model_alias(&existing, ceremony_alias),
            None => ceremony_alias.clone(),
        };
        models.insert(key, Item::Table(merged));
    }

    // 4. default_model: kimi's canPreserveDefaultModel rule.
    let current_default = primary
        .get("default_model")
        .and_then(Item::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty());
    let preservable = current_default.as_deref().is_some_and(|default| {
        ceremony_keys.contains(default)
            || primary
                .get("models")
                .and_then(Item::as_table_like)
                .and_then(|models| models.get(default))
                .and_then(Item::as_table_like)
                .is_some_and(|alias| {
                    alias.get("provider").and_then(Item::as_str) != Some(MANAGED_KIMI_PROVIDER)
                })
    });
    if !preservable {
        if let Some(ceremony_default) = ceremony.get("default_model").and_then(Item::as_str) {
            primary["default_model"] = toml_edit::value(ceremony_default);
        }
    }

    // 5. [thinking]: only when the primary has none.
    if primary.get("thinking").is_none() {
        if let Some(thinking) = ceremony.get("thinking").and_then(as_detached_table) {
            primary
                .as_table_mut()
                .insert("thinking", Item::Table(thinking));
        }
    }

    // 6. [services]: the two managed entries; user services preserved.
    let ceremony_services: Vec<(&str, Table)> = MANAGED_SERVICE_KEYS
        .iter()
        .filter_map(|key| {
            ceremony
                .get("services")
                .and_then(Item::as_table_like)
                .and_then(|services| services.get(key))
                .and_then(as_detached_table)
                .map(|table| (*key, table))
        })
        .collect();
    if !ceremony_services.is_empty() {
        let services = root_table(&mut primary, "services");
        for (key, table) in ceremony_services {
            services.insert(key, Item::Table(table));
        }
    }

    persist_private_config(&primary_path, primary.to_string().as_bytes())
}

/// `mergeRefreshedModelAlias` (kimi 0.34.0): user extras (existing keys
/// outside [`MANAGED_MODEL_FIELDS`], minus `overrides`), then every upstream
/// key, then the existing `overrides` table verbatim.
fn merge_refreshed_model_alias(existing: &Table, upstream: &Table) -> Table {
    let mut merged = Table::new();
    for (key, item) in existing.iter() {
        if key == "overrides" || MANAGED_MODEL_FIELDS.contains(&key) {
            continue;
        }
        merged.insert(key, item.clone());
    }
    for (key, item) in upstream.iter() {
        merged.insert(key, item.clone());
    }
    if let Some(overrides) = existing.get("overrides") {
        merged.insert("overrides", overrides.clone());
    }
    merged
}

/// The named root table, created as an explicit table when absent. A scalar
/// squatting on the name is replaced — the merge's sections are tables in
/// every config kimi itself writes.
fn root_table<'a>(document: &'a mut DocumentMut, key: &str) -> &'a mut Table {
    let root = document.as_table_mut();
    if !root.get(key).is_some_and(Item::is_table) {
        root.insert(key, Item::Table(Table::new()));
    }
    root.get_mut(key)
        .and_then(Item::as_table_mut)
        .expect("just inserted a table")
}

/// Stage-and-rename the merged config with owner-private permissions, the
/// same discipline the credential installer uses.
fn persist_private_config(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Kimi config.toml has no parent directory"))?;
    let (mut file, staged) = crate::file_watcher::stage_in(parent)?;
    let staged_write = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| set_private_config_permissions(&staged));
    drop(file);
    if let Err(error) = staged_write {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    if let Err(error) = crate::file_watcher::persist_staged(&staged, path) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    set_private_config_permissions(path)
}

#[cfg(unix)]
fn set_private_config_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn set_private_config_permissions(path: &Path) -> io::Result<()> {
    crate::platform::set_owner_private_permissions(path)
}

#[cfg(not(any(unix, windows)))]
fn set_private_config_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fresh-install stub kimi 0.34.0 writes (181 bytes, comment-only) —
    /// the exact primary-home state of the 2026-08-11 incident box.
    const STUB_CONFIG: &str = "\
# ~/.kimi-code/config.toml
# Runtime settings for Kimi Code.
# This file starts empty so built-in defaults can apply.
# Login will populate managed Kimi provider and model entries.
";

    /// A login-provisioned ceremony config (shape captured live from kimi
    /// 0.34.0's own writer on this seat's dev box).
    const CEREMONY_CONFIG: &str = r#"default_model = "kimi-code/k3"

[thinking]
enabled = true
effort = "high"

[services.moonshot_search]
base_url = "https://api.kimi.com/coding/v1/search"
api_key = ""

[services.moonshot_search.oauth]
storage = "file"
key = "oauth/kimi-code"

[services.moonshot_fetch]
base_url = "https://api.kimi.com/coding/v1/fetch"
api_key = ""

[services.moonshot_fetch.oauth]
storage = "file"
key = "oauth/kimi-code"

[providers."managed:kimi-code"]
type = "kimi"
api_key = ""
base_url = "https://api.kimi.com/coding/v1"

[providers."managed:kimi-code".oauth]
storage = "file"
key = "oauth/kimi-code"

[models."kimi-code/k3"]
provider = "managed:kimi-code"
model = "k3"
max_context_size = 1048576
capabilities = [ "thinking", "always_thinking", "image_in", "video_in", "tool_use" ]
display_name = "K3"
support_efforts = [ "low", "high", "max" ]
default_effort = "high"

[models."kimi-code/k3-256k"]
provider = "managed:kimi-code"
model = "k3-256k"
max_context_size = 262144
capabilities = [ "thinking", "always_thinking", "image_in", "tool_use" ]
display_name = "K3-256k"
"#;

    fn home_with_config(config: Option<&str>) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        if let Some(config) = config {
            fs::write(kimi_config_path(home.path()), config).unwrap();
        }
        home
    }

    fn parsed(home: &Path) -> DocumentMut {
        fs::read_to_string(kimi_config_path(home))
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn providers_configured_truth_table() {
        // Absent config: not configured (kimi's gate reads an empty table).
        assert_eq!(providers_configured(home_with_config(None).path()), Some(false));
        // The incident's comment-only stub.
        assert_eq!(
            providers_configured(home_with_config(Some(STUB_CONFIG)).path()),
            Some(false)
        );
        // An empty declared table is still empty.
        assert_eq!(
            providers_configured(home_with_config(Some("[providers]\n")).path()),
            Some(false)
        );
        // Any provider passes ensureReady — BYOK counts.
        assert_eq!(
            providers_configured(
                home_with_config(Some("[providers.custom]\ntype = \"openai\"\n")).path()
            ),
            Some(true)
        );
        assert_eq!(
            providers_configured(home_with_config(Some(CEREMONY_CONFIG)).path()),
            Some(true)
        );
        // Unparseable is unknown, never a verdict.
        assert_eq!(
            providers_configured(home_with_config(Some("providers = [broken")).path()),
            None
        );
    }

    #[test]
    fn ceremony_provisioning_gate_requires_managed_provider_and_alias() {
        assert!(!ceremony_home_provisioned(home_with_config(None).path()));
        assert!(!ceremony_home_provisioned(
            home_with_config(Some(STUB_CONFIG)).path()
        ));
        // A BYOK provider does not make a LOGIN ceremony provisioned — the
        // gate is specifically about the managed entries login writes.
        assert!(!ceremony_home_provisioned(
            home_with_config(Some("[providers.custom]\ntype = \"openai\"\n")).path()
        ));
        // Provider record without any alias is a half-written provision.
        assert!(!ceremony_home_provisioned(
            home_with_config(Some(
                "[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\n"
            ))
            .path()
        ));
        assert!(ceremony_home_provisioned(
            home_with_config(Some(CEREMONY_CONFIG)).path()
        ));
    }

    #[test]
    fn incident_stub_primary_gains_full_managed_onboarding() {
        let primary = home_with_config(Some(STUB_CONFIG));
        let ceremony = home_with_config(Some(CEREMONY_CONFIG));
        complete_managed_onboarding(primary.path(), ceremony.path()).unwrap();

        assert_eq!(providers_configured(primary.path()), Some(true));
        let text = fs::read_to_string(kimi_config_path(primary.path())).unwrap();
        // The user-facing file keeps its comments — the merge edits, it does
        // not regenerate.
        assert!(text.contains("# Login will populate managed Kimi provider"));
        let document = parsed(primary.path());
        assert_eq!(
            document["providers"][MANAGED_KIMI_PROVIDER]["oauth"]["key"].as_str(),
            Some("oauth/kimi-code")
        );
        assert_eq!(
            document["models"]["kimi-code/k3"]["max_context_size"].as_integer(),
            Some(1_048_576)
        );
        assert_eq!(document["default_model"].as_str(), Some("kimi-code/k3"));
        assert_eq!(document["thinking"]["enabled"].as_bool(), Some(true));
        assert_eq!(
            document["services"]["moonshot_fetch"]["base_url"].as_str(),
            Some("https://api.kimi.com/coding/v1/fetch")
        );
    }

    #[test]
    fn absent_primary_config_is_created_whole() {
        let primary = home_with_config(None);
        let ceremony = home_with_config(Some(CEREMONY_CONFIG));
        complete_managed_onboarding(primary.path(), ceremony.path()).unwrap();
        assert_eq!(providers_configured(primary.path()), Some(true));
        assert_eq!(
            parsed(primary.path())["default_model"].as_str(),
            Some("kimi-code/k3")
        );
    }

    #[test]
    fn user_entries_survive_the_merge_untouched() {
        let primary = home_with_config(Some(
            r#"# my hand-tuned setup
default_model = "byok/gpt"

[providers.byok]
type = "openai"
api_key = "sk-user"
base_url = "https://example.test/v1"

[models."byok/gpt"]
provider = "byok"
model = "gpt-test"
max_context_size = 128000

[services.my_search]
base_url = "https://search.example.test"

[thinking]
enabled = false
"#,
        ));
        let ceremony = home_with_config(Some(CEREMONY_CONFIG));
        complete_managed_onboarding(primary.path(), ceremony.path()).unwrap();

        let document = parsed(primary.path());
        // User provider, alias, service, and comment all intact.
        assert_eq!(
            document["providers"]["byok"]["api_key"].as_str(),
            Some("sk-user")
        );
        assert_eq!(
            document["models"]["byok/gpt"]["model"].as_str(),
            Some("gpt-test")
        );
        assert_eq!(
            document["services"]["my_search"]["base_url"].as_str(),
            Some("https://search.example.test")
        );
        let text = fs::read_to_string(kimi_config_path(primary.path())).unwrap();
        assert!(text.contains("# my hand-tuned setup"));
        // The preservable non-managed default_model is preserved
        // (canPreserveDefaultModel), and the user's thinking table wins.
        assert_eq!(document["default_model"].as_str(), Some("byok/gpt"));
        assert_eq!(document["thinking"]["enabled"].as_bool(), Some(false));
        // The managed lineup still landed beside the user's entries.
        assert_eq!(
            document["providers"][MANAGED_KIMI_PROVIDER]["type"].as_str(),
            Some("kimi")
        );
        assert_eq!(
            document["models"]["kimi-code/k3"]["display_name"].as_str(),
            Some("K3")
        );
    }

    #[test]
    fn stale_managed_aliases_are_replaced_and_user_extras_kept() {
        let primary = home_with_config(Some(
            r#"default_model = "kimi-code/k2"

[providers."managed:kimi-code"]
type = "kimi"
api_key = ""
base_url = "https://api.kimi.com/coding/v1"

[models."kimi-code/k2"]
provider = "managed:kimi-code"
model = "k2"
max_context_size = 131072

[models."kimi-code/k3"]
provider = "managed:kimi-code"
model = "k3"
max_context_size = 4096
support_efforts = [ "low" ]
my_note = "pinned for the demo"

[models."kimi-code/k3".overrides]
temperature = 0.2
"#,
        ));
        let ceremony = home_with_config(Some(CEREMONY_CONFIG));
        complete_managed_onboarding(primary.path(), ceremony.path()).unwrap();

        let document = parsed(primary.path());
        // The retired managed alias is gone (kimi's stale-alias rule)…
        assert!(document["models"].get("kimi-code/k2").is_none());
        // …so the dangling default follows the ceremony's selection.
        assert_eq!(document["default_model"].as_str(), Some("kimi-code/k3"));
        // Managed fields refreshed from upstream.
        assert_eq!(
            document["models"]["kimi-code/k3"]["max_context_size"].as_integer(),
            Some(1_048_576)
        );
        // User extras + overrides preserved (mergeRefreshedModelAlias).
        assert_eq!(
            document["models"]["kimi-code/k3"]["my_note"].as_str(),
            Some("pinned for the demo")
        );
        assert_eq!(
            document["models"]["kimi-code/k3"]["overrides"]["temperature"].as_float(),
            Some(0.2)
        );
    }

    #[test]
    fn preserved_managed_default_survives_a_refresh() {
        let primary = home_with_config(Some(
            "default_model = \"kimi-code/k3-256k\"\n\n[thinking]\nenabled = true\n",
        ));
        let ceremony = home_with_config(Some(CEREMONY_CONFIG));
        complete_managed_onboarding(primary.path(), ceremony.path()).unwrap();
        // The existing default names a ceremony managed alias — preserved
        // even though the ceremony selected kimi-code/k3.
        assert_eq!(
            parsed(primary.path())["default_model"].as_str(),
            Some("kimi-code/k3-256k")
        );
    }

    #[test]
    fn unprovisioned_ceremony_home_is_an_error_and_primary_untouched() {
        let primary = home_with_config(Some(STUB_CONFIG));
        for ceremony_config in [
            None,
            Some(STUB_CONFIG),
            // Managed provider without a single alias.
            Some("[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\n"),
        ] {
            let ceremony = home_with_config(ceremony_config);
            assert!(complete_managed_onboarding(primary.path(), ceremony.path()).is_err());
        }
        assert_eq!(
            fs::read_to_string(kimi_config_path(primary.path())).unwrap(),
            STUB_CONFIG
        );
    }

    #[test]
    fn unparseable_primary_config_is_refused_not_rewritten() {
        let primary = home_with_config(Some("providers = [broken"));
        let ceremony = home_with_config(Some(CEREMONY_CONFIG));
        let error = complete_managed_onboarding(primary.path(), ceremony.path()).unwrap_err();
        assert!(error.to_string().contains("refusing to rewrite"), "{error}");
        assert_eq!(
            fs::read_to_string(kimi_config_path(primary.path())).unwrap(),
            "providers = [broken"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merged_config_is_owner_private() {
        use std::os::unix::fs::PermissionsExt;
        let primary = home_with_config(None);
        let ceremony = home_with_config(Some(CEREMONY_CONFIG));
        complete_managed_onboarding(primary.path(), ceremony.path()).unwrap();
        let mode = fs::metadata(kimi_config_path(primary.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn completion_is_idempotent() {
        let primary = home_with_config(Some(STUB_CONFIG));
        let ceremony = home_with_config(Some(CEREMONY_CONFIG));
        complete_managed_onboarding(primary.path(), ceremony.path()).unwrap();
        let first = fs::read_to_string(kimi_config_path(primary.path())).unwrap();
        complete_managed_onboarding(primary.path(), ceremony.path()).unwrap();
        assert_eq!(
            fs::read_to_string(kimi_config_path(primary.path())).unwrap(),
            first
        );
    }
}
