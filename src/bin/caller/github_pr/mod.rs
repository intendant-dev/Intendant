//! GitHub App integration (Track PR): a real GitHub App — installation
//! tokens minted from a custody-sealed private key, read-only
//! fine-grained permissions, conditional requests — never a `gh`
//! wrapper, never a PAT. This slice ships the App client, the custody
//! entry, and the configuration/status surface; the agenda PR scanner
//! and the render-time state join arrive in the following slices.
//!
//! The coordination radar's `gh` file-set read is a separate,
//! deliberately cheap lane and stays untouched; unifying the two onto
//! this client is a future commission, not an ambient refactor.

pub(crate) mod client;
pub(crate) mod credentials;
pub(crate) mod status;
