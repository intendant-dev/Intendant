//! Startup helpers carved from main.rs (god-file split). The
//! per-mode wiring builder and mode-branch modules land here once
//! the internal-agent unification window opens.

pub(crate) mod daemon;
pub(crate) mod interactive;
pub(crate) mod mcp_mode;
pub(crate) mod peer_boot;
pub(crate) mod web;

pub(crate) use daemon::*;
pub(crate) use peer_boot::*;
pub(crate) use web::*;
