//! GitHub App integration (Track PR): a real GitHub App — installation
//! tokens minted from a custody-sealed private key, read-only
//! fine-grained permissions, conditional requests — never a `gh`
//! wrapper, never a PAT. The App client, custody entry, and
//! configuration/status surface landed first; the scanner mirrors
//! watched PRs as thin agenda anchors (see `scanner`); the render-time
//! join serves live PR state beside those anchors (see `join`).
//!
//! The coordination radar's `gh` file-set read is a separate,
//! deliberately cheap lane and stays untouched; unifying the two onto
//! this client is a future commission, not an ambient refactor.

pub(crate) mod client;
pub(crate) mod credentials;
pub(crate) mod join;
pub(crate) mod manifest_ceremony;
pub(crate) mod scanner;
pub(crate) mod status;
