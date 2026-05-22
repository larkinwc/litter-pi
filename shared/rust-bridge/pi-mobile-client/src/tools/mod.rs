//! Per-platform tool factories that produce pi's `ToolRegistry`.
//!
//! Each platform replaces pi's stock `BashTool` (host shell) with a
//! sandboxed exec surface:
//!
//! * iOS — [`ish::IshToolFactory`] routes through the iSH Alpine fakefs.
//! * Android — [`proot::ProotToolFactory`] routes through the proot
//!   runtime (implemented in a sibling feature).
//! * macOS dev/CI — [`pty_dev::PtyDevToolFactory`] runs against a real
//!   host PTY (implemented in a sibling feature).
//!
//! No `BashTool` is registered for the pi runtime here; the factories
//! above are the only path to a shell tool from `pi-mobile-client`.

pub mod ish;
pub mod proot;
pub mod pty_dev;
