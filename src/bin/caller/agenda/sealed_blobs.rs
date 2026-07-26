//! The sealed-refs snapshot store (owner-ruled 2026-07-26, PR B of the
//! sealed-refs design): content-addressed custody for the exact bytes a
//! binding ref pinned at propose time, so the approved revision of a
//! referenced file SURVIVES later edits to the live file — preservation
//! and traceability, not access control (IAM owns malice).
//!
//! Layout: `<agenda dir>/blobs/<sha256>` — one FILE per distinct
//! content, named by its full lowercase sha256 hex, so dedup is the
//! filename and integrity is re-checkable from the name alone. It
//! shares the `blobs/` root with the Ask-v2 preview store's per-item
//! DIRECTORIES (`blobs/<item ulid>/…`) without touching them: 64-hex
//! filenames and ULID directories cannot collide, and the lifecycles
//! stay disjoint — previews die with their item; sealed snapshots are
//! shared by hash across manifests and have NO deletion path yet. GC is
//! deliberately deferred: blobs are small text, dedup'd by content, and
//! a wrong sweep here destroys exactly the history this store exists to
//! preserve — a future pass can collect hashes no live manifest pins.
//!
//! **The deliberate PR-B semantics shift, stated here per the ruling:**
//! with a snapshot present, live-file drift NO LONGER refuses the fire
//! (PR A's shape) — the sealed bytes are the binding content, the fired
//! task points the session at the snapshot path, and drift is noted
//! informationally. Refusal remains only where preservation itself
//! failed closed: a corrupt snapshot (bytes no longer hash to the pin),
//! or a missing snapshot that cannot be healed from live bytes still
//! matching the approved pin.

use super::types::BindingRef;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Full sha256 of an in-memory buffer as lowercase hex — the bytes twin
/// of [`super::store::digest_file`], for callers that must hash and
/// persist the SAME read (no re-read window between verify and seal).
pub(crate) fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The sealed snapshot path for one pinned hash.
pub(crate) fn sealed_blob_path(agenda_dir: &Path, sha256: &str) -> PathBuf {
    super::blobs::blobs_root(agenda_dir).join(sha256)
}

/// Commit one sealed snapshot. `bytes` MUST already hash to `sha256` —
/// both callers (propose intake, the fire-time heal) hold the verified
/// bytes and the pin together; this is checked here anyway because a
/// blob filed under the wrong name would refuse every later fire as
/// corrupt. Content-addressed dedup: an existing blob under this hash
/// is the same bytes by construction, so re-sealing is a no-op. The
/// write is atomic (unique tmp + rename, fsync'd first) so a reader
/// never sees a short blob; a crash mid-write leaves only tmp residue
/// that the next seal of any content ignores.
pub(crate) fn seal_content(
    agenda_dir: &Path,
    sha256: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let computed = digest_bytes(bytes);
    if computed != sha256 {
        return Err(std::io::Error::other(format!(
            "refusing to seal under {sha256}: content hashes {computed}"
        )));
    }
    let path = sealed_blob_path(agenda_dir, sha256);
    if path.is_file() {
        return Ok(path);
    }
    let dir = super::blobs::blobs_root(agenda_dir);
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map(|()| path)
}

/// One verified seal at fire time: the snapshot path the fired session
/// reads as the binding content, plus the informational live-file state.
#[derive(Debug)]
pub(crate) struct SealedVerification {
    pub(crate) sealed_path: PathBuf,
    /// `Some(note)` when the live file no longer matches the approved
    /// pin — informational by design (the semantics shift above): it
    /// rides the fired task's rider line, never refuses.
    pub(crate) live_drift: Option<&'static str>,
}

impl SealedVerification {
    /// The fired task's data line for this ref (sealed refs doctrine:
    /// the CONTENT at the sealed path is what the owner reviewed and
    /// may carry instructions; this line itself is a pointer, data).
    pub(crate) fn rider_line(&self, binding_ref: &BindingRef) -> String {
        let drift = match self.live_drift {
            Some(note) => format!(" ({note})"),
            None => String::new(),
        };
        format!(
            "Binding ref {} sha256 {} — sealed copy {}, verified at fire{drift}",
            binding_ref.locator,
            binding_ref.sha256,
            self.sealed_path.display()
        )
    }
}

/// The commission's exact informational drift note.
const LIVE_DRIFTED: &str = "live file drifted from sealed revision";
const LIVE_UNREADABLE: &str = "live file unreadable; the sealed revision is the binding content";

/// Fire-time seal verification of one binding ref against the snapshot
/// store. Success serves the SEALED bytes' path; the live file is
/// probed only to note drift honestly. `Err` carries the named
/// occurrence-failure reason the scheduler journals:
///
/// - snapshot corrupt (bytes no longer hash to the pin) — fail-closed;
/// - snapshot missing and the live file no longer matches the pin —
///   preservation cannot be reconstructed;
/// - snapshot unreadable (I/O beyond absence).
///
/// A missing snapshot whose live file STILL hashes to the approved pin
/// heals in place: those are the approved bytes by the digest-bound
/// pin's own definition, so sealing them now is pure preservation.
/// This closes the window for manifests approved on the hash-pin build
/// (PR A) before this store existed, instead of refusing firings with
/// zero corruption behind them.
pub(crate) fn verify_sealed_binding_ref(
    agenda_dir: &Path,
    binding_ref: &BindingRef,
) -> Result<SealedVerification, String> {
    let live_path = super::store::binding_ref_path(&binding_ref.locator)?;
    let sealed_path = sealed_blob_path(agenda_dir, &binding_ref.sha256);
    match std::fs::read(&sealed_path) {
        Ok(bytes) => {
            let sealed_hash = digest_bytes(&bytes);
            if sealed_hash != binding_ref.sha256 {
                return Err(format!(
                    "binding ref snapshot corrupt: {} (sealed blob hashes {sealed_hash}, \
                     approved pin {}) — re-propose and re-approve to reseal",
                    binding_ref.locator, binding_ref.sha256
                ));
            }
            Ok(SealedVerification {
                sealed_path,
                live_drift: live_pin_note(live_path, &binding_ref.sha256),
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let bytes = std::fs::read(live_path).map_err(|live_err| {
                format!(
                    "binding ref snapshot missing and live file unreadable: {} ({live_err}) — \
                     re-propose and re-approve to reseal",
                    binding_ref.locator
                )
            })?;
            if digest_bytes(&bytes) != binding_ref.sha256 {
                return Err(format!(
                    "binding ref snapshot missing and live file drifted: {} — the approved \
                     revision cannot be reconstructed; re-propose and re-approve to reseal",
                    binding_ref.locator
                ));
            }
            let sealed_path =
                seal_content(agenda_dir, &binding_ref.sha256, &bytes).map_err(|seal_err| {
                    format!(
                        "binding ref snapshot missing and resealing failed: {} ({seal_err})",
                        binding_ref.locator
                    )
                })?;
            Ok(SealedVerification {
                sealed_path,
                live_drift: None,
            })
        }
        Err(err) => Err(format!(
            "binding ref snapshot unreadable: {} ({err})",
            binding_ref.locator
        )),
    }
}

/// Informational live-file probe: `None` when the live file still
/// matches the approved pin, otherwise the note the rider line carries.
fn live_pin_note(path: &Path, pin: &str) -> Option<&'static str> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Some(LIVE_UNREADABLE);
    };
    if !meta.is_file() {
        return Some(LIVE_UNREADABLE);
    }
    if meta.len() > super::types::MAX_REF_FILE_HASH_BYTES {
        return Some(LIVE_DRIFTED);
    }
    match super::store::digest_file(path) {
        Ok(live) if live == pin => None,
        Ok(_) => Some(LIVE_DRIFTED),
        Err(_) => Some(LIVE_UNREADABLE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIST vector: sha256("abc").
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn seal_content_is_atomic_content_addressed_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let path = seal_content(dir.path(), ABC, b"abc").unwrap();
        assert_eq!(path, sealed_blob_path(dir.path(), ABC));
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        // No tmp residue after a clean seal.
        let residue: Vec<_> = std::fs::read_dir(super::super::blobs::blobs_root(dir.path()))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(residue.is_empty(), "atomic writes leave no tmp files");
        // Dedup: re-sealing the same content is a no-op returning the
        // same path.
        let again = seal_content(dir.path(), ABC, b"abc").unwrap();
        assert_eq!(again, path);
        // Bytes that do not hash to the claimed pin are refused — a blob
        // filed under the wrong name would refuse every later fire.
        let err = seal_content(dir.path(), ABC, b"not abc").unwrap_err();
        assert!(err.to_string().contains("refusing to seal"), "{err}");
    }

    #[test]
    fn verification_serves_sealed_bytes_and_notes_live_drift() {
        let agenda = tempfile::tempdir().unwrap();
        let content = tempfile::tempdir().unwrap();
        let live = content.path().join("brief.md");
        std::fs::write(&live, b"abc").unwrap();
        let binding_ref = BindingRef {
            locator: format!("file:{}", live.display()),
            sha256: ABC.to_string(),
        };
        seal_content(agenda.path(), ABC, b"abc").unwrap();

        // Live intact: sealed path served, no drift note.
        let verification = verify_sealed_binding_ref(agenda.path(), &binding_ref).unwrap();
        assert_eq!(
            verification.sealed_path,
            sealed_blob_path(agenda.path(), ABC)
        );
        assert_eq!(verification.live_drift, None);

        // Live drifted: STILL serves the sealed path — the semantics
        // shift — with the commission's exact informational note.
        std::fs::write(&live, b"amended after approval").unwrap();
        let verification = verify_sealed_binding_ref(agenda.path(), &binding_ref).unwrap();
        assert_eq!(verification.live_drift, Some(LIVE_DRIFTED));
        assert_eq!(
            std::fs::read(&verification.sealed_path).unwrap(),
            b"abc",
            "the sealed revision is the binding content despite live drift"
        );
        assert!(verification
            .rider_line(&binding_ref)
            .ends_with("(live file drifted from sealed revision)"));

        // Live deleted: sealed still serves, unreadable note.
        std::fs::remove_file(&live).unwrap();
        let verification = verify_sealed_binding_ref(agenda.path(), &binding_ref).unwrap();
        assert_eq!(verification.live_drift, Some(LIVE_UNREADABLE));
    }

    #[test]
    fn corrupt_or_unreconstructable_snapshots_refuse_by_name() {
        let agenda = tempfile::tempdir().unwrap();
        let content = tempfile::tempdir().unwrap();
        let live = content.path().join("brief.md");
        std::fs::write(&live, b"abc").unwrap();
        let binding_ref = BindingRef {
            locator: format!("file:{}", live.display()),
            sha256: ABC.to_string(),
        };

        // Corrupt snapshot: bytes under the pin's name no longer hash to
        // it — fail-closed even though the live file is intact.
        std::fs::create_dir_all(super::super::blobs::blobs_root(agenda.path())).unwrap();
        std::fs::write(sealed_blob_path(agenda.path(), ABC), b"corrupted").unwrap();
        let err = verify_sealed_binding_ref(agenda.path(), &binding_ref).unwrap_err();
        assert!(err.starts_with("binding ref snapshot corrupt:"), "{err}");

        // Missing snapshot + live matching the pin: heals in place (the
        // PR-A window) and serves the fresh seal.
        std::fs::remove_file(sealed_blob_path(agenda.path(), ABC)).unwrap();
        let verification = verify_sealed_binding_ref(agenda.path(), &binding_ref).unwrap();
        assert_eq!(std::fs::read(&verification.sealed_path).unwrap(), b"abc");
        assert_eq!(verification.live_drift, None);

        // Missing snapshot + drifted live: the approved revision cannot
        // be reconstructed — refuse by name.
        std::fs::remove_file(sealed_blob_path(agenda.path(), ABC)).unwrap();
        std::fs::write(&live, b"amended").unwrap();
        let err = verify_sealed_binding_ref(agenda.path(), &binding_ref).unwrap_err();
        assert!(
            err.starts_with("binding ref snapshot missing and live file drifted:"),
            "{err}"
        );

        // Missing snapshot + unreadable live.
        std::fs::remove_file(&live).unwrap();
        let err = verify_sealed_binding_ref(agenda.path(), &binding_ref).unwrap_err();
        assert!(
            err.starts_with("binding ref snapshot missing and live file unreadable:"),
            "{err}"
        );
    }
}
