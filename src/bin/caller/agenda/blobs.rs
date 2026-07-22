//! The agenda-scoped preview blob store — custody for the rendered
//! preview cards of parked rich asks (ask↔agenda unification, slice 1).
//!
//! **Boundary pin (G1 steward rider, 2026-07-22): this store serves Ask-v2
//! previews EXCLUSIVELY.** Typed references (`refs[]`, G1) never touch it —
//! a ref is a pointer (locator + attach-time digest), never content: no
//! file bytes, copies, or uploads enter the agenda for refs, here or in
//! the op log. If a future feature wants ref-adjacent bytes, that is a new
//! owner decision, not an extension of this store.
//!
//! **Copy, don't reference** (ratified guardrail): at park time preview
//! bytes are committed HERE, under the agenda's own directory
//! (`<agenda dir>/blobs/<item id>/`), never referenced from a session
//! upload store whose lifecycle (session pruning, global-store retention)
//! is unrelated to the item's. Retention is tied to the item lifecycle:
//! blobs are deleted when the item is **retired** — not on completion,
//! because answered questions remain visible in the archive with their
//! previews.
//!
//! Layout mirrors the upload store's blob + `.json` sidecar pattern so a
//! directory listing lines the pair up and descriptors rehydrate without
//! a central index:
//!
//! ```text
//! <agenda dir>/blobs/<item ulid>/<blob id>.<ext>       # the bytes
//! <agenda dir>/blobs/<item ulid>/<blob id>.<ext>.json  # AgendaBlobDescriptor
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Sidecar descriptor for one committed preview blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgendaBlobDescriptor {
    /// Blob id (uuid, simple hex) — the `{blob_id}` path segment of the
    /// raw route and the `upload_id` field of the preview reference.
    pub(crate) id: String,
    /// Display filename (sanitized preview label + extension).
    pub(crate) name: String,
    pub(crate) mime: String,
    pub(crate) size: u64,
}

pub(crate) fn blobs_root(agenda_dir: &Path) -> PathBuf {
    agenda_dir.join("blobs")
}

/// The raw-route URL browsers fetch for one committed blob (the
/// `/api/agenda/blobs/{item_id}/{blob_id}/raw` row in
/// `gateway_routes.rs`).
pub(crate) fn agenda_blob_raw_url(item_id: &str, blob_id: &str) -> String {
    format!("/api/agenda/blobs/{item_id}/{blob_id}/raw")
}

fn item_dir(agenda_dir: &Path, item_id: &str) -> PathBuf {
    blobs_root(agenda_dir).join(item_id)
}

/// Strict id shapes for path segments served over HTTP: item ids are
/// ULIDs, blob ids simple-hex uuids. Anything else — separators, dots,
/// traversal — is refused before any path is built.
fn segment_is_safe(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 64
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Commit one preview blob for `item_id`; returns the descriptor. The
/// caller rolls back with [`delete_item_blobs`] on any later failure of
/// the same park (mirrors `ask_user_inner`'s cross-question rollback).
pub(crate) fn commit_blob(
    agenda_dir: &Path,
    item_id: &str,
    label: &str,
    extension: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<AgendaBlobDescriptor, String> {
    if !segment_is_safe(item_id) {
        return Err(format!("invalid item id '{item_id}'"));
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let safe_label = crate::upload_store::sanitize_name(label);
    let dir = item_dir(agenda_dir, item_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("creating agenda blob dir for '{label}': {e}"))?;
    let filename = format!("{id}.{extension}");
    let path = dir.join(&filename);
    std::fs::write(&path, bytes).map_err(|e| format!("storing preview '{label}': {e}"))?;
    let descriptor = AgendaBlobDescriptor {
        id,
        name: format!("{safe_label}.{extension}"),
        mime: mime.to_string(),
        size: bytes.len() as u64,
    };
    let sidecar = dir.join(format!("{filename}.json"));
    let json = serde_json::to_vec_pretty(&descriptor)
        .map_err(|e| format!("encoding preview descriptor '{label}': {e}"))?;
    if let Err(err) = std::fs::write(&sidecar, json) {
        let _ = std::fs::remove_file(&path);
        return Err(format!("storing preview descriptor '{label}': {err}"));
    }
    Ok(descriptor)
}

/// Resolve one blob for serving: `(descriptor, bytes path)`. `None` when
/// either id is malformed or the blob is gone (retired item).
pub(crate) fn find_blob(
    agenda_dir: &Path,
    item_id: &str,
    blob_id: &str,
) -> Option<(AgendaBlobDescriptor, PathBuf)> {
    if !segment_is_safe(item_id) || !segment_is_safe(blob_id) {
        return None;
    }
    let dir = item_dir(agenda_dir, item_id);
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if !stem.starts_with(blob_id) {
            continue;
        }
        let descriptor: AgendaBlobDescriptor =
            serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
        if descriptor.id != blob_id {
            continue;
        }
        let blob_path = dir.join(stem);
        if blob_path.is_file() {
            return Some((descriptor, blob_path));
        }
    }
    None
}

/// Delete every blob of one item (park rollback; retire-time retention).
/// Missing dir is fine — nothing was committed.
pub(crate) fn delete_item_blobs(agenda_dir: &Path, item_id: &str) -> std::io::Result<()> {
    if !segment_is_safe(item_id) {
        return Ok(());
    }
    match std::fs::remove_dir_all(item_dir(agenda_dir, item_id)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_find_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let item = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let committed = commit_blob(
            tmp.path(),
            item,
            "Variant A",
            "html",
            "text/html",
            b"<html/>",
        )
        .unwrap();
        assert_eq!(committed.mime, "text/html");
        assert_eq!(committed.size, 7);
        assert_eq!(committed.name, "Variant_A.html");

        let (descriptor, path) = find_blob(tmp.path(), item, &committed.id).unwrap();
        assert_eq!(descriptor, committed);
        assert_eq!(std::fs::read(&path).unwrap(), b"<html/>");

        // Unknown blob/item miss without error.
        assert!(find_blob(tmp.path(), item, "deadbeef").is_none());
        assert!(find_blob(tmp.path(), "01OTHER", &committed.id).is_none());

        // Retire-time deletion removes the whole item dir; idempotent.
        delete_item_blobs(tmp.path(), item).unwrap();
        assert!(find_blob(tmp.path(), item, &committed.id).is_none());
        assert!(!blobs_root(tmp.path()).join(item).exists());
        delete_item_blobs(tmp.path(), item).unwrap();
    }

    /// Path traversal and separator smuggling in either segment is refused
    /// before any filesystem path is formed.
    #[test]
    fn malformed_ids_never_touch_disk() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["../x", "a/b", "a\\b", "", ".", "a b", &"x".repeat(65)] {
            assert!(commit_blob(tmp.path(), bad, "l", "html", "text/html", b"x").is_err());
            assert!(find_blob(tmp.path(), bad, "aaaa").is_none());
            assert!(find_blob(tmp.path(), "01ITEM", bad).is_none());
        }
    }

    /// The rollback story: several blobs committed for one item vanish
    /// together, exactly like `ask_user_inner`'s cross-question rollback.
    #[test]
    fn rollback_removes_every_committed_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let item = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let a = commit_blob(tmp.path(), item, "A", "html", "text/html", b"a").unwrap();
        let b = commit_blob(tmp.path(), item, "B", "png", "image/png", b"b").unwrap();
        assert!(find_blob(tmp.path(), item, &a.id).is_some());
        assert!(find_blob(tmp.path(), item, &b.id).is_some());
        delete_item_blobs(tmp.path(), item).unwrap();
        assert!(find_blob(tmp.path(), item, &a.id).is_none());
        assert!(find_blob(tmp.path(), item, &b.id).is_none());
    }
}
