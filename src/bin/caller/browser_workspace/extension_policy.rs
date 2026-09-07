//! Startup authority for byte-exact browser extensions. Requests identify an
//! archive; only owner-pinned process argv can approve it. Never consult env,
//! project configuration, settings, or a caller-supplied policy here.
use super::{BrowserWorkspaceError, BROWSER_EXTENSION_ARCHIVE_MAX_BYTES};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path};
use std::sync::OnceLock;

const POLICY_MAX_BYTES: u64 = 64 * 1024;
const MAX_APPROVALS: usize = 32;
static POLICY: OnceLock<ExtensionPolicy> = OnceLock::new();
static DENY_ALL: ExtensionPolicy = ExtensionPolicy {
    schema_version: 1,
    extensions: Vec::new(),
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExtensionPolicy {
    schema_version: u32,
    extensions: Vec<ApprovedExtension>,
}
impl Default for ExtensionPolicy {
    fn default() -> Self {
        Self {
            schema_version: 1,
            extensions: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovedExtension {
    archive_sha256: String,
    archive_byte_length: u64,
    manifest_version: u32,
    version: String,
    service_worker: String,
}

fn invalid(message: impl Into<String>) -> BrowserWorkspaceError {
    BrowserWorkspaceError::Unsupported(message.into())
}
pub(super) fn sha256_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(super) fn version_valid(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    (1..=4).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 5
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
                && part.parse::<u16>().is_ok()
        })
        && parts.iter().any(|part| *part != "0")
}
/// Portable, literal relative paths: no aliases, traversal, ADS,
/// Windows device names or trailing-dot normalization. Also used on ZIP names.
pub(super) fn relative_path_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.split('/').all(|part| {
            let stem = part
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with('.')
                && !part.ends_with(' ')
                && !part
                    .chars()
                    .any(|c| c.is_control() || "<>:\"\\|?*".contains(c))
                && !matches!(
                    stem.as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "CONIN$"
                        | "CONOUT$"
                        | "COM¹"
                        | "COM²"
                        | "COM³"
                        | "LPT¹"
                        | "LPT²"
                        | "LPT³"
                )
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        })
}
pub(super) fn worker_path_valid(value: &str) -> bool {
    relative_path_valid(value) && !value.contains('%') && !value.contains('#')
}
pub(super) fn absolute_path_valid(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 4096 && !s.chars().any(char::is_control))
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}

/// Read one bounded regular-file handle. The pin covers exactly these bytes,
/// including whitespace. No reopening after hashing and no blocking FIFO race.
pub(super) fn snapshot_file(path: &Path, maximum: u64) -> Result<Vec<u8>, BrowserWorkspaceError> {
    if !absolute_path_valid(path) {
        return Err(invalid(
            "extension input path must be a bounded absolute path without traversal",
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| invalid(format!("cannot inspect extension input: {e}")))?;
    if !metadata.is_file()
        || intendant_platform::platform::path_leaf_is_symlink_or_reparse(path)
            .map_err(|e| invalid(format!("cannot inspect extension input leaf: {e}")))?
    {
        return Err(invalid(
            "extension input must be a regular non-symlink, non-reparse file",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(any(unix, windows)))]
    return Err(invalid(
        "safe extension snapshots are unsupported on this platform",
    ));
    let file = options.open(path).map_err(|e| {
        invalid(format!(
            "cannot open extension input without following links: {e}"
        ))
    })?;
    let opened = file
        .metadata()
        .map_err(|e| invalid(format!("cannot inspect extension input handle: {e}")))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if opened.file_attributes() & 0x0000_0400 != 0 {
            return Err(invalid("opened extension input is a Windows reparse point"));
        }
    }
    if !opened.is_file() || opened.len() == 0 || opened.len() > maximum {
        return Err(invalid(format!(
            "extension input must be a regular file of 1..={maximum} bytes"
        )));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| invalid(format!("cannot snapshot extension input: {e}")))?;
    if bytes.len() as u64 != opened.len() || bytes.len() as u64 > maximum {
        return Err(invalid("extension input changed length while snapshotting"));
    }
    Ok(bytes)
}

impl ExtensionPolicy {
    pub(super) fn from_snapshot(bytes: &[u8], pin: &str) -> Result<Self, BrowserWorkspaceError> {
        if bytes.is_empty() || bytes.len() as u64 > POLICY_MAX_BYTES || !sha256_valid(pin) {
            return Err(invalid(
                "browser extension policy requires 1..=65536 bytes and a lowercase SHA256 pin",
            ));
        }
        if format!("{:x}", Sha256::digest(bytes)) != pin {
            return Err(invalid("browser extension policy SHA256 mismatch"));
        }
        // Deserialize directly into strict structs: serde rejects duplicate and
        // unknown fields at both levels, as well as missing or mistyped fields.
        let policy: Self = serde_json::from_slice(bytes)
            .map_err(|e| invalid(format!("invalid browser extension policy JSON: {e}")))?;
        if policy.schema_version != 1 || policy.extensions.len() > MAX_APPROVALS {
            return Err(invalid(
                "browser extension policy requires schema_version=1 and at most 32 entries",
            ));
        }
        let mut identities = HashSet::new();
        for entry in &policy.extensions {
            if !sha256_valid(&entry.archive_sha256)
                || !(1..=BROWSER_EXTENSION_ARCHIVE_MAX_BYTES).contains(&entry.archive_byte_length)
                || entry.manifest_version != 3
                || !version_valid(&entry.version)
                || !worker_path_valid(&entry.service_worker)
                || !identities.insert(entry.archive_sha256.clone())
            {
                return Err(invalid("invalid or duplicate browser extension approval identity, length, MV3 version, or service-worker path"));
            }
        }
        Ok(policy)
    }
    pub(super) fn from_startup(
        path: Option<&str>,
        pin: Option<&str>,
    ) -> Result<Self, BrowserWorkspaceError> {
        match (path, pin) {
            (None, None) => Ok(Self::default()),
            (Some(path), Some(pin)) => {
                if !sha256_valid(pin) { return Err(invalid("browser extension policy pin must be 64 lowercase hexadecimal characters")); }
                Self::from_snapshot(&snapshot_file(Path::new(path), POLICY_MAX_BYTES)?, pin)
            }
            _ => Err(invalid("--browser-extension-policy and --browser-extension-policy-sha256 must be supplied together")),
        }
    }
    pub(super) fn approved_worker(
        &self,
        digest: &str,
        length: u64,
        manifest_version: u32,
        version: &str,
    ) -> Result<&str, BrowserWorkspaceError> {
        self.extensions.iter().find(|entry| {
            entry.archive_sha256 == digest && entry.archive_byte_length == length
                && entry.manifest_version == manifest_version && entry.version == version
        }).map(|entry| entry.service_worker.as_str()).ok_or_else(|| invalid("extension archive identity is not approved by the immutable daemon startup policy"))
    }
}
pub(super) fn current() -> &'static ExtensionPolicy {
    POLICY.get().unwrap_or(&DENY_ALL)
}
pub(super) fn initialize(
    path: Option<&str>,
    pin: Option<&str>,
) -> Result<(), BrowserWorkspaceError> {
    let policy = ExtensionPolicy::from_startup(path, pin)?;
    POLICY
        .set(policy)
        .map_err(|_| invalid("browser extension policy was already initialized"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn entry() -> serde_json::Value {
        json!({"archive_sha256":"a".repeat(64),"archive_byte_length":123,"manifest_version":3,"version":"1.0.0","service_worker":"worker.js"})
    }
    fn read(bytes: &[u8]) -> Result<ExtensionPolicy, BrowserWorkspaceError> {
        ExtensionPolicy::from_snapshot(bytes, &format!("{:x}", Sha256::digest(bytes)))
    }
    #[test]
    fn startup_defaults_deny_and_requires_both_flags() {
        let policy = ExtensionPolicy::from_startup(None, None).unwrap();
        assert!(policy
            .approved_worker(&"a".repeat(64), 123, 3, "1.0.0")
            .is_err());
        assert!(ExtensionPolicy::from_startup(Some("unused"), None).is_err());
        assert!(ExtensionPolicy::from_startup(None, Some(&"a".repeat(64))).is_err());
    }
    #[test]
    fn strict_snapshot_rejects_hash_drift_duplicate_unknown_and_malformed_fields() {
        let valid = json!({"schema_version":1,"extensions":[entry()]}).to_string();
        assert!(read(valid.as_bytes()).is_ok());
        assert!(ExtensionPolicy::from_snapshot(valid.as_bytes(), &"0".repeat(64)).is_err());
        let pin = format!("{:x}", Sha256::digest(valid.as_bytes()));
        assert!(ExtensionPolicy::from_snapshot(format!("{valid}\n").as_bytes(), &pin).is_err());
        for malformed in [
            valid.replace(
                "\"schema_version\":1",
                "\"schema_version\":1,\"schema_version\":1",
            ),
            valid.replace(
                "\"schema_version\":1",
                "\"schema_version\":1,\"unknown\":true",
            ),
            valid.replace(
                "\"manifest_version\":3",
                "\"manifest_version\":3,\"manifest_version\":3",
            ),
            valid.replace(
                "\"manifest_version\":3",
                "\"manifest_version\":3,\"unknown\":0",
            ),
            valid.replace("\"manifest_version\":3", "\"manifest_version\":2"),
            valid.replace("\"schema_version\":1", "\"schema_version\":2"),
            valid.replace("\"archive_byte_length\":123", "\"archive_byte_length\":-1"),
            format!("{valid} trailing"),
        ] {
            assert!(read(malformed.as_bytes()).is_err(), "{malformed}");
        }
        assert!(read(&vec![b' '; POLICY_MAX_BYTES as usize + 1]).is_err());
    }
    #[test]
    fn policy_rejects_bad_id_length_version_paths_and_entry_count() {
        for (field, bad) in [
            ("archive_sha256", json!("a".repeat(63))),
            ("archive_sha256", json!("A".repeat(64))),
            ("archive_byte_length", json!(0)),
            (
                "archive_byte_length",
                json!(BROWSER_EXTENSION_ARCHIVE_MAX_BYTES + 1),
            ),
            ("version", json!("01.0")),
            ("version", json!("65536")),
            ("version", json!("0.0")),
        ] {
            let mut e = entry();
            e[field] = bad;
            assert!(read(
                json!({"schema_version":1,"extensions":[e]})
                    .to_string()
                    .as_bytes()
            )
            .is_err());
        }
        for path in [
            "",
            "../worker.js",
            "a/../worker.js",
            "/worker.js",
            "a//b.js",
            "./worker.js",
            "C:\\worker.js",
            "a:worker.js",
            "worker.js?x",
            "worker.js#x",
            "%2e%2e/worker.js",
            "NUL.js",
            "COM¹.js",
            "a/worker.js.",
        ] {
            let mut e = entry();
            e["service_worker"] = json!(path);
            assert!(
                read(
                    json!({"schema_version":1,"extensions":[e]})
                        .to_string()
                        .as_bytes()
                )
                .is_err(),
                "{path}"
            );
        }
        for count in [2, 33] {
            // Duplicate identities are refused even below the cap.
            assert!(read(
                json!({"schema_version":1,"extensions":vec![entry();count]})
                    .to_string()
                    .as_bytes()
            )
            .is_err());
        }
        let entries: Vec<_> = (0..33)
            .map(|i| {
                let mut e = entry();
                e["archive_sha256"] = json!(format!("{i:064x}"));
                e
            })
            .collect();
        assert!(read(
            json!({"schema_version":1,"extensions":&entries[..32]})
                .to_string()
                .as_bytes()
        )
        .is_ok());
        assert!(read(
            json!({"schema_version":1,"extensions":entries})
                .to_string()
                .as_bytes()
        )
        .is_err());
    }
    #[test]
    fn startup_policy_is_one_snapshot_not_a_live_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("policy.json");
        let bytes = json!({"schema_version":1,"extensions":[entry()]}).to_string();
        fs::write(&path, &bytes).unwrap();
        let pin = format!("{:x}", Sha256::digest(bytes.as_bytes()));
        let policy = ExtensionPolicy::from_startup(path.to_str(), Some(&pin)).unwrap();
        fs::write(&path, b"{}").unwrap();
        assert_eq!(
            policy
                .approved_worker(&"a".repeat(64), 123, 3, "1.0.0")
                .unwrap(),
            "worker.js"
        );
        assert!(ExtensionPolicy::from_startup(path.to_str(), Some(&pin)).is_err());
        assert!(snapshot_file(temp.path(), POLICY_MAX_BYTES).is_err());
        assert!(snapshot_file(&temp.path().join(".."), POLICY_MAX_BYTES).is_err());
        #[cfg(unix)]
        {
            let link = temp.path().join("link.json");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(snapshot_file(&link, POLICY_MAX_BYTES).is_err());
        }
    }
}
