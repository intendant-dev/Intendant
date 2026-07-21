//! OS-keystore custody for the access-certs private-key estate (Track K).
//!
//! The class-1 artifacts — `ca.key`, `server.key`, `client.key`,
//! `client.p12` — normally live as 0600 files in the access cert dir.
//! `intendant custody migrate` relocates each into a sealed blob under
//! `<cert_dir>/custody/` (wrapping key held by the platform keystore via
//! `intendant-custody`) and replaces the file with a *tombstone* naming
//! the custody entry. Every consumption seam reads through
//! [`read_key_material`], which routes by content: a real key serves
//! as-is (labeled file mode), a tombstone routes to custody and **never**
//! falls back to a file — after migration, a stale plain copy reappearing
//! next to a tombstone must not silently win. Key regeneration writes go
//! through [`write_key_material`], so a recert on a migrated estate
//! refreshes the custody entry instead of regressing it to a file.
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

/// Sealed blobs live in this subdirectory of the access cert dir — inside
/// the estate, so the runtime sandbox's `access-certs` read-deny and
/// write-exclusion cover custody state with no new policy.
const CUSTODY_SUBDIR: &str = "custody";

/// First line of a migrated key file. Old binaries fail loudly on it
/// ("no private key found"), current ones route to custody.
const TOMBSTONE_MAGIC: &[u8] = b"INTENDANT CUSTODY TOMBSTONE v1\n";

/// The class-1 artifacts `intendant custody migrate` relocates: the
/// access-certs private-key subtree as one unit (ruling Q-S). The .p12
/// bundles the client key, so leaving it out would make migrating
/// `client.key` theater.
const CLASS1_FILES: [&str; 4] = ["ca.key", "server.key", "client.key", "client.p12"];

fn entry_name_for(file_name: &str) -> String {
    format!("access-certs/{file_name}")
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
             sealed blob under access-certs/{CUSTODY_SUBDIR}/ whose wrapping key lives in\n\
             the platform keystore. `intendant custody status` shows the estate;\n\
             `intendant custody restore` returns keys to plain files.\n"
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
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if !is_tombstone(&bytes) {
        return Ok(Secret::new(bytes));
    }
    let entry = tombstone_entry(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    let backend = required_backend(path, &entry)?;
    retrieve_migrated(backend.as_ref(), path, &entry)
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

/// Relocate every class-1 file in `cert_dir` into `backend`. Per-file
/// sequence: store → retrieve-and-verify byte-equal → tombstone the
/// file. Any failure stops the run with the file untouched — a failed or
/// half migration leaves a working file install (already-migrated files
/// from earlier in the run stay migrated; the verb is idempotent).
fn migrate_estate(
    cert_dir: &Path,
    backend: &dyn CustodyBackend,
    mut report: impl FnMut(&str, &Outcome),
) -> Result<(), String> {
    for name in CLASS1_FILES {
        let path = cert_dir.join(name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                report(name, &Outcome::Skipped("not present"));
                continue;
            }
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        if is_tombstone(&bytes) {
            report(name, &Outcome::Skipped("already in custody"));
            continue;
        }
        let entry = entry_name_for(name);
        backend
            .store(&entry, &bytes)
            .map_err(|error| format!("store {entry}: {error}"))?;
        let sealed = backend
            .retrieve(&entry)
            .map_err(|error| format!("verify {entry}: {error}"))?;
        if sealed.as_bytes() != bytes.as_slice() {
            return Err(format!(
                "verify {entry}: round-trip mismatch — file left untouched"
            ));
        }
        replace_file_atomic(&path, &tombstone_body(&entry, backend.kind()))?;
        credential_audit::record(
            credential_audit::EVENT_KEY_MIGRATED,
            &entry,
            &path.display().to_string(),
            "operator",
            format!(
                "relocated into {} custody (relocation, not rotation — pre-migration copies \
                 unaffected)",
                backend.kind()
            ),
        );
        report(name, &Outcome::Done);
    }
    Ok(())
}

/// Return every migrated class-1 file to a plain file. Per-file:
/// retrieve → durable file write over the tombstone → delete the blob
/// (best-effort; a leftover blob is inert once the file is real again).
fn restore_estate(
    cert_dir: &Path,
    backend: &dyn CustodyBackend,
    mut report: impl FnMut(&str, &Outcome),
) -> Result<(), String> {
    for name in CLASS1_FILES {
        let path = cert_dir.join(name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                report(name, &Outcome::Skipped("not present"));
                continue;
            }
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        if !is_tombstone(&bytes) {
            report(name, &Outcome::Skipped("already a file"));
            continue;
        }
        let entry = tombstone_entry(&bytes).map_err(|error| format!("{name}: {error}"))?;
        let material = backend
            .retrieve(&entry)
            .map_err(|error| format!("retrieve {entry}: {error}"))?;
        replace_file_atomic(&path, material.as_bytes())?;
        if let Err(error) = backend.delete(&entry) {
            eprintln!("!! blob delete for {entry} failed ({error}) — the restored file wins; the leftover sealed blob is inert");
        }
        credential_audit::record(
            credential_audit::EVENT_KEY_RESTORED,
            &entry,
            &path.display().to_string(),
            "operator",
            "returned from custody to a plain file".to_string(),
        );
        report(name, &Outcome::Done);
    }
    Ok(())
}

/// `intendant custody <status|migrate|restore>` — keyless, local, opt-in
/// (ruling Q2: migration never happens on boot).
pub fn run_cli(args: Vec<String>) -> Result<(), String> {
    let action = args.first().map(String::as_str).unwrap_or("");
    if !matches!(action, "status" | "migrate" | "restore") {
        return Err("usage: intendant custody <status|migrate|restore>\n\
             \n\
             status    Show where each access private key lives (file, custody, missing)\n\
             migrate   Relocate access private keys into OS-keystore custody (opt-in)\n\
             restore   Return custody-held access private keys to plain files"
            .to_string());
    }
    let cert_dir = crate::access::backend::select_backend().cert_dir();
    match action {
        "status" => print_status(&cert_dir),
        "migrate" => {
            let backend = cli_backend(&cert_dir)?;
            println!(
                ":: migrating access private keys into {} custody",
                backend.kind()
            );
            migrate_estate(&cert_dir, backend.as_ref(), print_outcome)?;
            println!();
            println!(
                ":: done — sealed blobs live in {}; reads route through custody",
                cert_dir.join(CUSTODY_SUBDIR).display()
            );
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
            let backend = cli_backend(&cert_dir)?;
            println!(":: returning access private keys to plain files");
            restore_estate(&cert_dir, backend.as_ref(), print_outcome)?;
            println!(":: done — keys are plain files again (labeled file mode)");
            Ok(())
        }
        _ => unreachable!("matched above"),
    }
}

fn cli_backend(cert_dir: &Path) -> Result<Box<dyn CustodyBackend>, String> {
    match platform_backend(cert_dir) {
        Some(Ok(backend)) => Ok(backend),
        Some(Err(error)) => Err(error),
        None => Err(
            "no custody backend on this platform yet (Windows DPAPI and Linux secret-service \
             backends arrive with a later Track K slice); access keys stay in labeled file mode"
                .to_string(),
        ),
    }
}

fn print_outcome(name: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Done => println!("   {name:<12} done"),
        Outcome::Skipped(reason) => println!("   {name:<12} skipped ({reason})"),
    }
}

fn print_status(cert_dir: &Path) -> Result<(), String> {
    println!("Custody status (access-certs estate)");
    println!("   cert dir: {}", cert_dir.display());
    let backend = match platform_backend(cert_dir) {
        Some(Ok(backend)) => {
            println!("   backend:  {} (available)", backend.kind());
            Some(backend)
        }
        Some(Err(error)) => {
            println!("   backend:  unavailable ({error})");
            None
        }
        None => {
            println!(
                "   backend:  none on this platform yet — keys stay in labeled file mode\n\
                              (Windows DPAPI / Linux secret-service arrive with a later Track K slice)"
            );
            None
        }
    };
    println!();
    for name in CLASS1_FILES {
        let path = cert_dir.join(name);
        let line = match std::fs::read(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing".to_string(),
            Err(error) => format!("unreadable ({error})"),
            Ok(bytes) if !is_tombstone(&bytes) => "file mode".to_string(),
            Ok(bytes) => match tombstone_entry(&bytes) {
                Err(error) => format!("INCONSISTENT: {error}"),
                Ok(entry) => match backend.as_deref().map(|backend| backend.contains(&entry)) {
                    Some(Ok(true)) => "custody (sealed blob present)".to_string(),
                    Some(Ok(false)) => format!(
                        "INCONSISTENT: tombstone for {entry} but no sealed blob — restore is \
                         impossible; regenerate via `intendant access setup --force`"
                    ),
                    Some(Err(error)) => format!("custody (blob check failed: {error})"),
                    None => format!("custody entry {entry} (no backend on this platform)"),
                },
            },
        };
        println!("   {name:<12} {line}");
    }
    Ok(())
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
        migrate_estate(tmp.path(), &backend, |name, outcome| {
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
        migrate_estate(tmp.path(), &backend, |name, outcome| {
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
        migrate_estate(tmp.path(), &backend, |_, _| {}).unwrap();
        assert!(is_tombstone(&std::fs::read(&key).unwrap()));

        restore_estate(tmp.path(), &backend, |_, _| {}).unwrap();
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
        migrate_estate(tmp.path(), &backend, |_, _| {}).unwrap();

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
        let error = migrate_estate(tmp.path(), &FailingStore, |_, _| {}).unwrap_err();
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
}
