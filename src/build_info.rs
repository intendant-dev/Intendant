//! Build provenance embedded at compile time.
//!
//! The values come from `build.rs` `cargo:rustc-env` directives
//! (`INTENDANT_GIT_SHA`, `INTENDANT_BUILD_TIMESTAMP`,
//! `INTENDANT_TARGET_TRIPLE`), which apply to every binary in the package.
//! Shared between the `intendant` and `intendant-runtime` binaries the same
//! way `win_sandbox.rs` is: a plain `mod` in the runtime crate root and a
//! `#[path]` include from the caller crate.
//!
//! `option_env!` with fallbacks (not `env!`): a build whose build script
//! could not resolve git — or a hypothetical build where the directives are
//! absent — still compiles and reports `unknown` instead of failing.

/// One-line version + provenance string: package version, git commit short
/// SHA (with `-dirty` marker), build timestamp, and target triple. Contains
/// no credentials, hostnames, usernames, or filesystem paths — safe to print
/// on any surface.
pub(crate) fn version_line(binary: &str) -> String {
    format!(
        "{} {} (commit {}, built {}, {})",
        binary,
        option_env!("CARGO_PKG_VERSION").unwrap_or("unknown"),
        option_env!("INTENDANT_GIT_SHA").unwrap_or("unknown"),
        option_env!("INTENDANT_BUILD_TIMESTAMP").unwrap_or("unknown"),
        option_env!("INTENDANT_TARGET_TRIPLE").unwrap_or("unknown"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_carries_binary_name_and_package_version() {
        let line = version_line("intendant");
        assert!(line.starts_with("intendant "));
        assert!(line.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn version_line_carries_all_provenance_fields() {
        // build.rs always emits the three directives (with "unknown"
        // fallbacks computed at build time), so the line always has the
        // full `(commit <sha>, built <ts>, <triple>)` shape.
        let line = version_line("intendant-runtime");
        assert!(line.contains("(commit "));
        assert!(line.contains(", built "));
        // The target triple is the last parenthesized field.
        assert!(line.trim_end().ends_with(')'));
        assert!(
            line.contains(option_env!("INTENDANT_TARGET_TRIPLE").unwrap_or("unknown")),
            "target triple must ride the version line: {line}"
        );
    }
}
