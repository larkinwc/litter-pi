//! `pi-mobile-client`: shared Rust bridge between Litter's mobile stack and
//! the in-process pi coding agent runtime.
//!
//! The public API surface is intentionally narrow. Only the types required
//! to spawn and talk to an in-process pi runtime are exported; everything
//! else (the asupersync↔tokio bridge internals, the auth/tools/approvals
//! modules, the config builder) is implementation detail.

// `deny(unsafe_code)` rather than `forbid` so a single marker symbol in
// `tools/proot.rs` (used to keep `ProotToolFactory` discoverable in the
// Android stripped .so) can opt in via `#[allow(unsafe_code)]`. No other
// site in `pi-mobile-client` is allowed to use `unsafe`.
#![deny(unsafe_code)]

// Implementation modules — kept `pub(crate)` (or private) so the crate
// presents the narrow API described in `architecture.md`.
mod runtime_bridge;
mod server;
mod turn_state;

// Stub modules that other features in this milestone will fill in. They
// are declared (so the file tree matches the architecture doc) but not
// re-exported from the crate root.
mod approvals;
pub mod auth;
mod config;
mod events;
mod local_runtime_instructions;
pub mod tools;

// Narrow public surface.
pub use server::{
    Command, InProcessStartArgs, PiEvent, PiInProcessHandle, PiSessionConfig, ToolFactoryKind,
    start_in_process,
};
pub use turn_state::{PiTurnState, classify_retryable};
