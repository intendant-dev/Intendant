//! Entry-name validation and filesystem mapping.
//!
//! Entry names are stable identifiers shaped like the paths they custody
//! ("access-certs/client.key"). They participate in seal AAD, so a name is
//! part of an entry's cryptographic identity — and they map onto single
//! filenames, so they must not smuggle path structure.

use crate::CustodyError;

pub(crate) const MAX_ENTRY_NAME_LEN: usize = 128;

/// Validate an entry name: ASCII alphanumerics plus `.`, `_`, `-`, `/`;
/// non-empty, bounded, no leading/trailing/doubled `/`, and no `.`/`..`
/// segments (names map to filenames — path structure never survives).
pub fn validate_entry_name(name: &str) -> Result<(), CustodyError> {
    let invalid = |reason: &'static str| CustodyError::InvalidName {
        name: name.to_string(),
        reason,
    };
    if name.is_empty() {
        return Err(invalid("empty"));
    }
    if name.len() > MAX_ENTRY_NAME_LEN {
        return Err(invalid("longer than 128 bytes"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(invalid("characters outside [A-Za-z0-9._/-]"));
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err(invalid("leading, trailing, or doubled '/'"));
    }
    if name
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(invalid("'.' or '..' path segment"));
    }
    Ok(())
}

/// The single flat filename a (validated) entry name stores under.
pub(crate) fn file_stem_for(name: &str) -> String {
    name.replace('/', "__")
}

/// File name of a [`crate::WrappedBlobBackend`] entry's sealed blob.
/// Exported so callers can *observe* custody state (status listings)
/// with pure path math — constructing a backend would create its blob
/// directory as a side effect.
pub fn sealed_blob_file_name(name: &str) -> Result<String, CustodyError> {
    validate_entry_name(name)?;
    Ok(format!("{}.sealed", file_stem_for(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_validate_and_map_flat() {
        validate_entry_name("access-certs/client.key").unwrap();
        validate_entry_name("daemon-identity/ed25519.pk8").unwrap();
        assert_eq!(
            file_stem_for("access-certs/client.key"),
            "access-certs__client.key"
        );

        for bad in [
            "",
            "/leading",
            "trailing/",
            "a//b",
            "a/../b",
            "./a",
            "sp ace",
            "uni\u{2603}code",
            &"x".repeat(MAX_ENTRY_NAME_LEN + 1),
        ] {
            assert!(
                validate_entry_name(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }
}
