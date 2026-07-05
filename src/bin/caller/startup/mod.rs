//! Startup carved from main.rs (god-file split): web bind/TLS and
//! peer-boot helpers, plus the four mode branches main() dispatches
//! to (daemon, mcp_mode, interactive, headless). The per-mode wiring
//! dedup (four near-identical EventBus/listener/transcriber blocks
//! -> one builder) is the planned follow-up semantic pass.

pub(crate) mod daemon;
pub(crate) mod headless;
pub(crate) mod interactive;
pub(crate) mod mcp_mode;
pub(crate) mod peer_boot;
pub(crate) mod web;

pub(crate) use daemon::*;
pub(crate) use peer_boot::*;
pub(crate) use web::*;
