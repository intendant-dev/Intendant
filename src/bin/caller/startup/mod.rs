//! Startup carved from main.rs (god-file split): web bind/TLS and
//! peer-boot helpers, the three mode branches main() dispatches to
//! (daemon, mcp_mode, headless — the foreground shape), and wiring.rs
//! — the shared builders those branches assemble themselves from (one
//! copy of each block that used to exist once per mode).

pub(crate) mod daemon;
pub(crate) mod headless;
pub(crate) mod mcp_mode;
pub(crate) mod peer_boot;
pub(crate) mod web;
pub(crate) mod wiring;

pub(crate) use daemon::*;
pub(crate) use peer_boot::*;
pub(crate) use web::*;
