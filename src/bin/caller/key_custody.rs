//! OS-keystore custody for the daemon's private-key estates (Track K).
//!
//! Two ruled classes migrate through this module. Class 1 — `ca.key`,
//! `server.key`, `client.key`, `client.p12` in the access cert dir — and
//! class 3, the Ed25519 identity keys: `daemon-identity/ed25519.pk8` and
//! every org `root.pk8`/`issuer.pk8` under `<cert_dir>/org/<handle>/`.
//! `intendant custody migrate` relocates each file into a sealed blob
//! under its estate's `custody/` subdirectory (wrapping key held by the
//! platform keystore via `intendant-custody`) and replaces the file with
//! a *tombstone* naming the custody entry. Every consumption seam reads
//! through [`read_key_material`]/[`read_key_material_opt`], which route
//! by content: a real key serves as-is (labeled file mode), a tombstone
//! routes to custody and **never** falls back to a file — after
//! migration, a stale plain copy reappearing next to a tombstone must
//! not silently win. Key regeneration writes go through
//! [`write_key_material`], so a recert on a migrated estate refreshes
//! the custody entry instead of regressing it to a file.
//!
//! Binding labels from the Track K ruling:
//! - Custody before a Developer ID + hardened-runtime binary is
//!   *bar-raising, not lane-sealing* — it defeats the casual same-uid
//!   file read, not a patient same-uid attacker.
//! - Migration is *relocation, not rotation* (Q6, owner-accepted
//!   2026-07-21): copies made before migration are unaffected.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use intendant_custody::{BackendKind, CustodyBackend, Secret};

use crate::credential_audit;

/// Sealed blobs live in this subdirectory of each estate root — inside
/// the subtree the runtime sandbox already read-denies, so custody state
/// needs no new policy.
const CUSTODY_SUBDIR: &str = "custody";

/// First line of a migrated key file. Old binaries fail loudly on it
/// ("no private key found"), current ones route to custody.
const TOMBSTONE_MAGIC: &[u8] = b"INTENDANT CUSTODY TOMBSTONE v1\n";

/// The class-1 artifacts: the access-certs private-key subtree as one
/// unit (ruling Q-S). The .p12 bundles the client key, so leaving it out
/// would make migrating `client.key` theater.
const CLASS1_FILES: [&str; 4] = ["ca.key", "server.key", "client.key", "client.p12"];

/// The class-3 per-org key files under `<cert_dir>/org/<handle>/`.
const ORG_KEY_FILES: [&str; 2] = ["root.pk8", "issuer.pk8"];

/// The class-3 daemon identity key file.
const IDENTITY_KEY_FILE: &str = "ed25519.pk8";

/// One custody-managed file: where it lives and the custody entry name
/// its tombstone carries (which is also the seal AAD).
struct EstateFile {
    name: String,
    path: PathBuf,
    entry: String,
}

/// A group of custody-managed files sharing one directory; sealed blobs
/// live at `<root>/custody/`.
struct Estate {
    label: String,
    root: PathBuf,
    files: Vec<EstateFile>,
}

fn access_estate(cert_dir: &Path) -> Estate {
    Estate {
        label: "access-certs".to_string(),
        root: cert_dir.to_path_buf(),
        files: CLASS1_FILES
            .iter()
            .map(|name| EstateFile {
                name: (*name).to_string(),
                path: cert_dir.join(name),
                entry: format!("access-certs/{name}"),
            })
            .collect(),
    }
}

/// The default daemon-identity estate. An operator-configured
/// hosted-control `identity_path` outside this directory still routes
/// through the read seam if hand-migrated, but the verbs only enumerate
/// the default location in v1.
fn identity_estate(identity_dir: &Path) -> Estate {
    Estate {
        label: "daemon-identity".to_string(),
        root: identity_dir.to_path_buf(),
        files: vec![EstateFile {
            name: IDENTITY_KEY_FILE.to_string(),
            path: identity_dir.join(IDENTITY_KEY_FILE),
            entry: format!("daemon-identity/{IDENTITY_KEY_FILE}"),
        }],
    }
}

/// One estate per org key directory on disk (root key required by the
/// handle lister; the issuer key is optional and skips when absent).
fn org_estates(cert_dir: &Path) -> Vec<Estate> {
    crate::access::org::local_org_handles(cert_dir)
        .into_iter()
        .map(|handle| {
            let root = cert_dir.join("org").join(&handle);
            Estate {
                label: format!("org {handle}"),
                files: ORG_KEY_FILES
                    .iter()
                    .map(|name| EstateFile {
                        name: (*name).to_string(),
                        path: root.join(name),
                        entry: format!("org/{handle}/{name}"),
                    })
                    .collect(),
                root,
            }
        })
        .collect()
}

/// Every custody-managed estate for the given roots, in status order.
fn all_estates(cert_dir: &Path, identity_dir: &Path) -> Vec<Estate> {
    let mut estates = vec![access_estate(cert_dir), identity_estate(identity_dir)];
    estates.extend(org_estates(cert_dir));
    estates
}

fn is_tombstone(bytes: &[u8]) -> bool {
    bytes.starts_with(TOMBSTONE_MAGIC)
}

/// The custody entry named by a tombstone. Only called on bytes that
/// passed [`is_tombstone`]; a magic-but-no-entry file is corrupt and
/// fails closed rather than being served as key material.
fn tombstone_entry(bytes: &[u8]) -> Result<String, String> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("entry: "))
                .map(|entry| entry.trim().to_string())
        })
        .filter(|entry| !entry.is_empty())
        .ok_or_else(|| "corrupt custody tombstone: no entry line".to_string())
}

fn tombstone_body(entry: &str, kind: BackendKind) -> Vec<u8> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut body = TOMBSTONE_MAGIC.to_vec();
    body.extend_from_slice(
        format!(
            "entry: {entry}\nbackend: {kind}\nmoved: {now}\n\n\
             The private key that lived here is held in OS-keystore custody: a\n\
             sealed blob in the {CUSTODY_SUBDIR}/ directory beside this file, whose\n\
             wrapping key lives in the platform keystore. `intendant custody status`\n\
             shows the estate; `intendant custody restore` returns keys to plain files.\n"
        )
        .as_bytes(),
    );
    body
}

/// The platform custody backend for blobs under `<cert_dir>/custody`, or
/// `None` where no OS-keystore backend exists yet (Windows DPAPI and
/// Linux secret-service arrive with a later Track K slice; until then
/// those platforms stay in labeled file mode).
fn platform_backend(cert_dir: &Path) -> Option<Result<Box<dyn CustodyBackend>, String>> {
    #[cfg(target_os = "macos")]
    {
        Some(
            intendant_custody::mac_keychain::mac_wrapped_backend(cert_dir.join(CUSTODY_SUBDIR))
                .map(|backend| Box::new(backend) as Box<dyn CustodyBackend>)
                .map_err(|error| format!("custody backend setup: {error}")),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cert_dir;
        None
    }
}

/// Read private-key material from `path`, routing tombstones to custody.
///
/// This is the single consumption seam for every class-1 read (TLS
/// server key, peer mTLS client identity, CA ceremonies, p12 serving) —
/// and it also serves operator-override key paths, which simply never
/// contain a tombstone. Missing files and custody failures are named
/// errors; a migrated key never silently degrades to a file read. A
/// plain file is the labeled pre-migration default (`intendant custody
/// status` names it), deliberately not a per-read audit event — reads
/// happen at every daemon boot and dial, and there is no silent
/// degradation lane left to audit: the custody lane fails closed.
pub fn read_key_material(path: &Path) -> Result<Secret, String> {
    read_key_material_opt(path)?.ok_or_else(|| format!("read {}: no such file", path.display()))
}

/// [`read_key_material`] for load-or-create callers: an absent file is
/// `Ok(None)` (the caller's create branch), every other outcome —
/// including custody failures for a tombstoned file — stays a named
/// error. Absence discrimination lives here so callers never parse
/// error strings for it.
pub fn read_key_material_opt(path: &Path) -> Result<Option<Secret>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    if !is_tombstone(&bytes) {
        return Ok(Some(Secret::new(bytes)));
    }
    let entry = tombstone_entry(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    let backend = required_backend(path, &entry)?;
    retrieve_migrated(backend.as_ref(), path, &entry).map(Some)
}

/// Write private-key material to `path`, keeping migrated entries in
/// custody: if the file is a tombstone, the new material replaces the
/// custody entry and the tombstone is refreshed; otherwise this is a
/// plain owner-only-from-creation file write.
pub fn write_key_material(path: &Path, material: &[u8]) -> Result<(), String> {
    let existing = std::fs::read(path).ok();
    match existing.as_deref().filter(|bytes| is_tombstone(bytes)) {
        Some(bytes) => {
            let entry =
                tombstone_entry(bytes).map_err(|error| format!("{}: {error}", path.display()))?;
            let backend = required_backend(path, &entry)?;
            store_migrated(backend.as_ref(), path, &entry, material)
        }
        None => intendant_core::state_paths::write_private_file(path, material)
            .map_err(|error| format!("write {}: {error}", path.display())),
    }
}

/// Resolve the platform backend for a migrated file, failing closed (and
/// auditing) when this platform cannot serve the entry.
fn required_backend(path: &Path, entry: &str) -> Result<Box<dyn CustodyBackend>, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    match platform_backend(parent) {
        Some(Ok(backend)) => Ok(backend),
        Some(Err(error)) => {
            audit_denied(entry, path, &error);
            Err(format!(
                "custody entry {entry} for {}: {error}; `intendant custody restore` returns keys \
                 to files (run it from a context the keystore trusts)",
                path.display()
            ))
        }
        None => {
            let error = "no custody backend on this platform".to_string();
            audit_denied(entry, path, &error);
            Err(format!(
                "custody entry {entry} for {}: {error} — the key was migrated on a platform with \
                 an OS keystore; run `intendant custody restore` there, or restore the file from \
                 backup",
                path.display()
            ))
        }
    }
}

/// Retrieve a migrated entry — the fail-closed custody lane. Every
/// failure is audited and named; there is deliberately no file fallback.
fn retrieve_migrated(
    backend: &dyn CustodyBackend,
    path: &Path,
    entry: &str,
) -> Result<Secret, String> {
    backend.retrieve(entry).map_err(|error| {
        let error = error.to_string();
        audit_denied(entry, path, &error);
        format!(
            "custody entry {entry} for {}: {error}; `intendant custody restore` returns keys to \
             files (run it from a context the keystore trusts)",
            path.display()
        )
    })
}

/// Replace a migrated entry's material and refresh its tombstone.
fn store_migrated(
    backend: &dyn CustodyBackend,
    path: &Path,
    entry: &str,
    material: &[u8],
) -> Result<(), String> {
    backend.store(entry, material).map_err(|error| {
        let error = error.to_string();
        audit_denied(entry, path, &error);
        format!("custody store {entry} for {}: {error}", path.display())
    })?;
    replace_file_atomic(path, &tombstone_body(entry, backend.kind()))?;
    credential_audit::record(
        credential_audit::EVENT_KEY_STORED,
        entry,
        &path.display().to_string(),
        "daemon",
        "custody entry replaced by key regeneration".to_string(),
    );
    Ok(())
}

fn audit_denied(entry: &str, path: &Path, detail: &str) {
    credential_audit::record(
        credential_audit::EVENT_KEY_DENIED,
        entry,
        &path.display().to_string(),
        "daemon",
        detail.to_string(),
    );
}

/// Staged sibling used by [`replace_file_atomic`]; a distinct suffix per
/// full file name so `client.key` and `client.p12` never collide.
fn staged_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".custody-staged");
    path.with_file_name(name)
}

/// Owner-only staged write + rename. Standard library only (the tempfile
/// crate stays off persist seams), synced before rename, parent synced
/// after so the swap is durable before any custody blob is deleted.
fn replace_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let staged = staged_path(path);
    let _ = std::fs::remove_file(&staged);
    let mut file = intendant_core::state_paths::private_file_options()
        .create_new(true)
        .open(&staged)
        .map_err(|error| format!("stage {}: {error}", staged.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("stage {}: {error}", staged.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", staged.display()))?;
    drop(file);
    std::fs::rename(&staged, path)
        .map_err(|error| format!("replace {}: {error}", path.display()))?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// One artifact's outcome in a migrate/restore run.
enum Outcome {
    Done,
    Skipped(&'static str),
}

/// Relocate every file of one estate into `backend`. Per-file sequence:
/// store → retrieve-and-verify byte-equal → tombstone the file. Any
/// failure stops the run with the file untouched — a failed or half
/// migration leaves a working file install (files migrated earlier in
/// the run stay migrated; the verb is idempotent).
fn migrate_estate(
    estate: &Estate,
    backend: &dyn CustodyBackend,
    mut report: impl FnMut(&str, &Outcome),
) -> Result<(), String> {
    for file in &estate.files {
        let bytes = match std::fs::read(&file.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                report(&file.name, &Outcome::Skipped("not present"));
                continue;
            }
            Err(error) => return Err(format!("read {}: {error}", file.path.display())),
        };
        if is_tombstone(&bytes) {
            report(&file.name, &Outcome::Skipped("already in custody"));
            continue;
        }
        let entry = &file.entry;
        backend
            .store(entry, &bytes)
            .map_err(|error| format!("store {entry}: {error}"))?;
        let sealed = backend
            .retrieve(entry)
            .map_err(|error| format!("verify {entry}: {error}"))?;
        if sealed.as_bytes() != bytes.as_slice() {
            return Err(format!(
                "verify {entry}: round-trip mismatch — file left untouched"
            ));
        }
        replace_file_atomic(&file.path, &tombstone_body(entry, backend.kind()))?;
        credential_audit::record(
            credential_audit::EVENT_KEY_MIGRATED,
            entry,
            &file.path.display().to_string(),
            "operator",
            format!(
                "relocated into {} custody (relocation, not rotation — pre-migration copies \
                 unaffected)",
                backend.kind()
            ),
        );
        report(&file.name, &Outcome::Done);
    }
    Ok(())
}

/// Return every migrated file of one estate to a plain file. Per-file:
/// retrieve → durable file write over the tombstone → delete the blob
/// (best-effort; a leftover blob is inert once the file is real again).
fn restore_estate(
    estate: &Estate,
    backend: &dyn CustodyBackend,
    mut report: impl FnMut(&str, &Outcome),
) -> Result<(), String> {
    for file in &estate.files {
        let bytes = match std::fs::read(&file.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                report(&file.name, &Outcome::Skipped("not present"));
                continue;
            }
            Err(error) => return Err(format!("read {}: {error}", file.path.display())),
        };
        if !is_tombstone(&bytes) {
            report(&file.name, &Outcome::Skipped("already a file"));
            continue;
        }
        let entry = tombstone_entry(&bytes).map_err(|error| format!("{}: {error}", file.name))?;
        let material = backend
            .retrieve(&entry)
            .map_err(|error| format!("retrieve {entry}: {error}"))?;
        replace_file_atomic(&file.path, material.as_bytes())?;
        if let Err(error) = backend.delete(&entry) {
            eprintln!("!! blob delete for {entry} failed ({error}) — the restored file wins; the leftover sealed blob is inert");
        }
        credential_audit::record(
            credential_audit::EVENT_KEY_RESTORED,
            &entry,
            &file.path.display().to_string(),
            "operator",
            "returned from custody to a plain file".to_string(),
        );
        report(&file.name, &Outcome::Done);
    }
    Ok(())
}

/// The platform backend kind, without constructing anything — pure
/// availability for status labeling.
fn platform_backend_kind() -> Option<BackendKind> {
    #[cfg(target_os = "macos")]
    {
        Some(BackendKind::MacKeychainWrapped)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

const NO_BACKEND_MESSAGE: &str =
    "no custody backend on this platform yet (Windows DPAPI and Linux secret-service backends \
     arrive with a later Track K slice); private keys stay in labeled file mode";

/// `intendant custody <status|migrate|restore>` — keyless, local, opt-in
/// (ruling Q2: migration never happens on boot). Covers the class-1
/// access estate and the class-3 identity estates in one pass.
pub fn run_cli(args: Vec<String>) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("");
    if !matches!(action, "status" | "migrate" | "restore") {
        return Err("usage: intendant custody <status|migrate|restore>\n\
             \n\
             status    Show where each daemon private key lives (file, custody, missing)\n\
             migrate   Relocate daemon private keys into OS-keystore custody (opt-in)\n\
             restore   Return custody-held private keys to plain files"
            .to_string());
    }
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    let identity_dir = crate::daemon_identity::default_identity_dir();
    let estates = all_estates(&cert_dir, &identity_dir);
    match action {
        "status" => {
            print_status(&estates);
            Ok(())
        }
        "migrate" => {
            let kind = platform_backend_kind().ok_or(NO_BACKEND_MESSAGE.to_string())?;
            println!(":: migrating daemon private keys into {kind} custody");
            for estate in &estates {
                println!("   {} ({})", estate.label, estate.root.display());
                if !estate.root.exists() {
                    println!("      (not present)");
                    continue;
                }
                let backend = cli_backend(&estate.root)?;
                migrate_estate(estate, backend.as_ref(), print_outcome)?;
            }
            println!();
            println!(":: done — sealed blobs live beside each estate; reads route through custody");
            println!(
                ":: relocation, not rotation: copies made before migration are unaffected\n\
                 :: (pre-migration copy risk owner-accepted 2026-07-21)"
            );
            println!(
                ":: custody raises the bar against casual same-uid file reads; it is not a\n\
                 :: sealed lane until the daemon ships as a signed, hardened binary"
            );
            Ok(())
        }
        "restore" => {
            platform_backend_kind().ok_or(NO_BACKEND_MESSAGE.to_string())?;
            println!(":: returning daemon private keys to plain files");
            for estate in &estates {
                println!("   {} ({})", estate.label, estate.root.display());
                if !estate.root.exists() {
                    println!("      (not present)");
                    continue;
                }
                let backend = cli_backend(&estate.root)?;
                restore_estate(estate, backend.as_ref(), print_outcome)?;
            }
            println!(":: done — keys are plain files again (labeled file mode)");
            Ok(())
        }
        _ => unreachable!("matched above"),
    }
}

fn cli_backend(estate_root: &Path) -> Result<Box<dyn CustodyBackend>, String> {
    match platform_backend(estate_root) {
        Some(Ok(backend)) => Ok(backend),
        Some(Err(error)) => Err(error),
        None => Err(NO_BACKEND_MESSAGE.to_string()),
    }
}

fn print_outcome(name: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Done => println!("      {name:<12} done"),
        Outcome::Skipped(reason) => println!("      {name:<12} skipped ({reason})"),
    }
}

/// Observation only: pure path math, no backend construction (which
/// would create custody directories), no keystore access (no prompt or
/// deny surface from a status listing).
fn print_status(estates: &[Estate]) {
    println!("Custody status");
    match platform_backend_kind() {
        Some(kind) => println!("   backend: {kind} (available)"),
        None => println!(
            "   backend: none on this platform yet — keys stay in labeled file mode\n\
             \x20           (Windows DPAPI / Linux secret-service arrive with a later Track K slice)"
        ),
    }
    for estate in estates {
        println!();
        println!("   {} ({})", estate.label, estate.root.display());
        for file in &estate.files {
            let line = status_line(estate, file);
            println!("      {:<12} {line}", file.name);
        }
    }
}

fn status_line(estate: &Estate, file: &EstateFile) -> String {
    match std::fs::read(&file.path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing".to_string(),
        Err(error) => format!("unreadable ({error})"),
        Ok(bytes) if !is_tombstone(&bytes) => "file mode".to_string(),
        Ok(bytes) => match tombstone_entry(&bytes) {
            Err(error) => format!("INCONSISTENT: {error}"),
            Ok(entry) => {
                let blob = intendant_custody::sealed_blob_file_name(&entry)
                    .map(|name| estate.root.join(CUSTODY_SUBDIR).join(name));
                match blob {
                    Err(error) => format!("INCONSISTENT: tombstone entry invalid: {error}"),
                    Ok(blob) if blob.is_file() => "custody (sealed blob present)".to_string(),
                    Ok(_) => format!(
                        "INCONSISTENT: tombstone for {entry} but no sealed blob — restore is \
                         impossible; regenerate this key"
                    ),
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use intendant_custody::PlainFileBackend;

    fn backend_in(dir: &Path) -> PlainFileBackend {
        PlainFileBackend::new(dir.join(CUSTODY_SUBDIR)).unwrap()
    }

    fn seed(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn tombstone_detection_and_entry_roundtrip() {
        let body = tombstone_body("access-certs/server.key", BackendKind::PlainFile);
        assert!(is_tombstone(&body));
        assert_eq!(tombstone_entry(&body).unwrap(), "access-certs/server.key");
        assert!(!is_tombstone(b"-----BEGIN PRIVATE KEY-----\nabc\n"));
        // Magic without an entry line is corrupt, not key material.
        assert!(tombstone_entry(TOMBSTONE_MAGIC).is_err());
    }

    #[test]
    fn plain_files_serve_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        let path = seed(tmp.path(), "server.key", b"PLAIN PEM");
        assert_eq!(read_key_material(&path).unwrap().as_bytes(), b"PLAIN PEM");
        // Operator-override paths outside the estate behave identically.
        let other = seed(tmp.path(), "operator-override.pem", b"OTHER PEM");
        assert_eq!(read_key_material(&other).unwrap().as_bytes(), b"OTHER PEM");
    }

    #[test]
    fn migrate_tombstones_files_and_custody_serves_them() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = backend_in(tmp.path());
        let key = seed(tmp.path(), "client.key", b"CLIENT KEY");
        let p12 = seed(tmp.path(), "client.p12", b"\x30\x82P12 BINARY");

        let mut outcomes = Vec::new();
        migrate_estate(&access_estate(tmp.path()), &backend, |name, outcome| {
            outcomes.push((name.to_string(), matches!(outcome, Outcome::Done)));
        })
        .unwrap();
        assert_eq!(
            outcomes
                .iter()
                .filter(|(_, done)| *done)
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["client.key", "client.p12"],
            "absent ca.key/server.key skip, present files migrate"
        );

        // Files are tombstones now; the custody lane serves the material.
        let key_bytes = std::fs::read(&key).unwrap();
        assert!(is_tombstone(&key_bytes));
        let entry = tombstone_entry(&key_bytes).unwrap();
        assert_eq!(entry, "access-certs/client.key");
        assert_eq!(
            retrieve_migrated(&backend, &key, &entry)
                .unwrap()
                .as_bytes(),
            b"CLIENT KEY"
        );
        assert!(is_tombstone(&std::fs::read(&p12).unwrap()));

        // Idempotent: a second run skips everything.
        let mut second = Vec::new();
        migrate_estate(&access_estate(tmp.path()), &backend, |name, outcome| {
            second.push((name.to_string(), matches!(outcome, Outcome::Done)));
        })
        .unwrap();
        assert!(second.iter().all(|(_, done)| !done));

        // The migrate + custody-read trail reached the audit log.
        let events = credential_audit::recent(100);
        assert!(events.iter().any(|event| {
            event.event == credential_audit::EVENT_KEY_MIGRATED
                && event.kind == "access-certs/client.key"
                && event.label == key.display().to_string()
                && event.detail.contains("relocation, not rotation")
        }));
    }

    #[test]
    fn restore_returns_files_and_deletes_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = backend_in(tmp.path());
        let key = seed(tmp.path(), "server.key", b"SERVER KEY");
        migrate_estate(&access_estate(tmp.path()), &backend, |_, _| {}).unwrap();
        assert!(is_tombstone(&std::fs::read(&key).unwrap()));

        restore_estate(&access_estate(tmp.path()), &backend, |_, _| {}).unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), b"SERVER KEY");
        assert!(
            !backend.contains("access-certs/server.key").unwrap(),
            "restored entries leave no blob behind"
        );
        let events = credential_audit::recent(100);
        assert!(events.iter().any(|event| {
            event.event == credential_audit::EVENT_KEY_RESTORED
                && event.label == key.display().to_string()
        }));
    }

    #[test]
    fn custody_write_refreshes_entry_and_keeps_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = backend_in(tmp.path());
        let key = seed(tmp.path(), "server.key", b"OLD SERVER KEY");
        migrate_estate(&access_estate(tmp.path()), &backend, |_, _| {}).unwrap();

        // The regen path: store through the custody-aware writer.
        let bytes = std::fs::read(&key).unwrap();
        let entry = tombstone_entry(&bytes).unwrap();
        store_migrated(&backend, &key, &entry, b"NEW SERVER KEY").unwrap();
        assert!(is_tombstone(&std::fs::read(&key).unwrap()));
        assert_eq!(
            retrieve_migrated(&backend, &key, &entry)
                .unwrap()
                .as_bytes(),
            b"NEW SERVER KEY"
        );
    }

    #[test]
    fn write_key_material_plain_path_is_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fresh.key");
        write_key_material(&path, b"FRESH").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"FRESH");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    /// A backend failure mid-migrate leaves the current file untouched
    /// (files migrated earlier in the run stay migrated).
    #[test]
    fn migrate_failure_leaves_file_intact() {
        struct FailingStore;
        impl CustodyBackend for FailingStore {
            fn kind(&self) -> BackendKind {
                BackendKind::PlainFile
            }
            fn store(
                &self,
                name: &str,
                _material: &[u8],
            ) -> Result<(), intendant_custody::CustodyError> {
                Err(intendant_custody::CustodyError::BackendUnavailable {
                    backend: BackendKind::PlainFile,
                    reason: format!("simulated outage for {name}"),
                })
            }
            fn retrieve(&self, name: &str) -> Result<Secret, intendant_custody::CustodyError> {
                Err(intendant_custody::CustodyError::NotFound {
                    name: name.to_string(),
                })
            }
            fn delete(&self, _name: &str) -> Result<(), intendant_custody::CustodyError> {
                Ok(())
            }
            fn contains(&self, _name: &str) -> Result<bool, intendant_custody::CustodyError> {
                Ok(false)
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let key = seed(tmp.path(), "ca.key", b"CA KEY");
        let error =
            migrate_estate(&access_estate(tmp.path()), &FailingStore, |_, _| {}).unwrap_err();
        assert!(error.contains("store access-certs/ca.key"), "{error}");
        assert_eq!(
            std::fs::read(&key).unwrap(),
            b"CA KEY",
            "a failed store must leave the file untouched"
        );
    }

    /// The custody lane never falls back to files: a denied retrieval is
    /// a named error and lands in the audit trail.
    #[test]
    fn denied_retrieval_fails_closed_and_audits() {
        struct Denying;
        impl CustodyBackend for Denying {
            fn kind(&self) -> BackendKind {
                BackendKind::MacKeychainWrapped
            }
            fn store(
                &self,
                _name: &str,
                _material: &[u8],
            ) -> Result<(), intendant_custody::CustodyError> {
                Ok(())
            }
            fn retrieve(&self, _name: &str) -> Result<Secret, intendant_custody::CustodyError> {
                Err(intendant_custody::CustodyError::DeniedNonInteractive {
                    backend: BackendKind::MacKeychainWrapped,
                    reason: "simulated headless deny".to_string(),
                })
            }
            fn delete(&self, _name: &str) -> Result<(), intendant_custody::CustodyError> {
                Ok(())
            }
            fn contains(&self, _name: &str) -> Result<bool, intendant_custody::CustodyError> {
                Ok(true)
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("client.key");
        replace_file_atomic(
            &path,
            &tombstone_body("access-certs/client.key", BackendKind::MacKeychainWrapped),
        )
        .unwrap();
        let error = retrieve_migrated(&Denying, &path, "access-certs/client.key").unwrap_err();
        assert!(error.contains("denied non-interactively"), "{error}");
        assert!(error.contains("intendant custody restore"), "{error}");
        let events = credential_audit::recent(100);
        assert!(events.iter().any(|event| {
            event.event == credential_audit::EVENT_KEY_DENIED
                && event.label == path.display().to_string()
                && event.detail.contains("simulated headless deny")
        }));
    }

    /// On platforms with no custody backend, a tombstoned key is a named
    /// error pointing at the platform gap — never a silent file serve.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn tombstone_without_platform_backend_is_a_named_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("client.key");
        replace_file_atomic(
            &path,
            &tombstone_body("access-certs/client.key", BackendKind::MacKeychainWrapped),
        )
        .unwrap();
        let error = read_key_material(&path).unwrap_err();
        assert!(
            error.contains("no custody backend on this platform"),
            "{error}"
        );
    }

    #[test]
    fn cli_rejects_unknown_actions() {
        let error = run_cli(vec!["frobnicate".to_string()]).unwrap_err();
        assert!(error.contains("usage: intendant custody"), "{error}");
        let error = run_cli(Vec::new()).unwrap_err();
        assert!(error.contains("usage: intendant custody"), "{error}");
    }

    /// Estate enumeration covers class 1 plus every class-3 location:
    /// the daemon identity key and each org's key directory, with entry
    /// names carrying the estate namespace.
    #[test]
    fn estate_enumeration_covers_identity_and_orgs() {
        let tmp = tempfile::tempdir().unwrap();
        let cert_dir = tmp.path().join("access-certs");
        let identity_dir = tmp.path().join("daemon-identity");
        std::fs::create_dir_all(cert_dir.join("org/acme")).unwrap();
        std::fs::create_dir_all(cert_dir.join("org/zeta")).unwrap();
        std::fs::write(cert_dir.join("org/acme/root.pk8"), b"ROOT").unwrap();
        std::fs::write(cert_dir.join("org/zeta/root.pk8"), b"ROOT").unwrap();
        // A directory without a root key is not an org estate.
        std::fs::create_dir_all(cert_dir.join("org/empty")).unwrap();

        let estates = all_estates(&cert_dir, &identity_dir);
        let labels: Vec<&str> = estates.iter().map(|estate| estate.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["access-certs", "daemon-identity", "org acme", "org zeta"]
        );

        let identity = &estates[1];
        assert_eq!(identity.files.len(), 1);
        assert_eq!(identity.files[0].entry, "daemon-identity/ed25519.pk8");
        assert_eq!(identity.files[0].path, identity_dir.join("ed25519.pk8"));

        let acme = &estates[2];
        let entries: Vec<&str> = acme.files.iter().map(|file| file.entry.as_str()).collect();
        assert_eq!(entries, vec!["org/acme/root.pk8", "org/acme/issuer.pk8"]);
        // Every generated entry name is valid custody vocabulary.
        for estate in &estates {
            for file in &estate.files {
                intendant_custody::validate_entry_name(&file.entry).unwrap();
            }
        }
    }

    /// A migrated org estate round-trips: root key tombstoned, custody
    /// serves it, restore returns the file. The absent issuer key skips.
    #[test]
    fn org_estate_migrates_and_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let cert_dir = tmp.path().to_path_buf();
        let org_dir = cert_dir.join("org/acme");
        std::fs::create_dir_all(&org_dir).unwrap();
        std::fs::write(org_dir.join("root.pk8"), b"ORG ROOT PKCS8").unwrap();

        let estates = org_estates(&cert_dir);
        assert_eq!(estates.len(), 1);
        let estate = &estates[0];
        let backend = backend_in(&estate.root);

        let mut outcomes = Vec::new();
        migrate_estate(estate, &backend, |name, outcome| {
            outcomes.push((name.to_string(), matches!(outcome, Outcome::Done)));
        })
        .unwrap();
        assert_eq!(
            outcomes,
            vec![
                ("root.pk8".to_string(), true),
                ("issuer.pk8".to_string(), false)
            ]
        );

        let bytes = std::fs::read(org_dir.join("root.pk8")).unwrap();
        assert!(is_tombstone(&bytes));
        assert_eq!(tombstone_entry(&bytes).unwrap(), "org/acme/root.pk8");
        assert_eq!(
            retrieve_migrated(&backend, &org_dir.join("root.pk8"), "org/acme/root.pk8")
                .unwrap()
                .as_bytes(),
            b"ORG ROOT PKCS8"
        );
        // The handle lister still sees the org (the tombstone is a file).
        assert_eq!(
            crate::access::org::local_org_handles(&cert_dir),
            vec!["acme".to_string()]
        );

        restore_estate(estate, &backend, |_, _| {}).unwrap();
        assert_eq!(
            std::fs::read(org_dir.join("root.pk8")).unwrap(),
            b"ORG ROOT PKCS8"
        );
    }

    /// The load-or-create seam: absent is `Ok(None)` (create branch),
    /// plain files serve, and a real identity key round-trips through a
    /// migrated estate via the custody lane.
    #[test]
    fn read_opt_discriminates_absence_and_identity_key_survives_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_dir = tmp.path().join("daemon-identity");
        std::fs::create_dir_all(&identity_dir).unwrap();
        let key_path = identity_dir.join("ed25519.pk8");

        assert!(read_key_material_opt(&key_path).unwrap().is_none());

        // A real key pair, created through the daemon-identity module.
        let identity = crate::daemon_identity::DaemonIdentity::load_or_create(&key_path).unwrap();
        assert_eq!(
            read_key_material_opt(&key_path)
                .unwrap()
                .expect("plain file serves")
                .as_bytes(),
            std::fs::read(&key_path).unwrap().as_slice()
        );

        // Migrate the identity estate; the custody lane serves bytes that
        // parse back into the same signing identity.
        let estate = identity_estate(&identity_dir);
        let backend = backend_in(&identity_dir);
        migrate_estate(&estate, &backend, |_, _| {}).unwrap();
        assert!(is_tombstone(&std::fs::read(&key_path).unwrap()));
        let material =
            retrieve_migrated(&backend, &key_path, "daemon-identity/ed25519.pk8").unwrap();
        let reloaded = crate::daemon_identity::DaemonIdentity::from_pkcs8(material.as_bytes())
            .expect("custody-served bytes parse as the identity key");
        assert_eq!(reloaded.public_key_b64u(), identity.public_key_b64u());
    }
}
