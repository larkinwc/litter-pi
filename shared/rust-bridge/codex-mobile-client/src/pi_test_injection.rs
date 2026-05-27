//! Test-injection surface for the in-process pi runtime.
//!
//! Gated behind `#[cfg(any(test, feature = "test-injection"))]` (and
//! exposed across the UniFFI boundary only when the `test-injection`
//! cargo feature is enabled). Production iOS Debug/device and package
//! builds compile with the feature off, so the symbols below are
//! invisible to release callers — they exist purely so XCTest /
//! integration tests can substitute a stub `IshExec` and assert the
//! `PiSessionConfig.base_url` actually plumbed into the active
//! session, without booting the iSH kernel or driving a real
//! provider request.
//!
//! Two pieces are exposed:
//!
//! * `PiIshExec` — a UniFFI callback interface that mirrors
//!   `pi_mobile_client::tools::ish::IshExec`. A small adapter
//!   ([`PiIshExecAdapter`]) wraps it in the real Rust trait so the
//!   pi runtime can mount it via `ToolFactoryKind::Ish` without
//!   knowing it's a stub.
//! * `AppClient::connect_local_pi_byok_with_ish_exec` and
//!   `AppClient::pi_active_base_url` — the test-only constructor and
//!   accessor. See `ffi/client.rs` for the actual methods (they
//!   live alongside the production `connect_local_pi_byok` so the
//!   `Arc<MobileClient>` is in scope).

use std::path::Path;
use std::sync::Arc;

use pi_mobile_client::tools::ish::{IshExec, IshExecOutput};

/// UniFFI-friendly mirror of [`IshExecOutput`].
///
/// Swift/Kotlin callbacks return one of these; the adapter copies
/// the bytes back into the real `IshExecOutput`. Kept as a struct of
/// owned primitives so it crosses the UniFFI boundary cleanly.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PiIshExecOutput {
    /// Raw merged stdout+stderr bytes the stub wants to surface to
    /// the agent.
    pub stdout: Vec<u8>,
    /// Exit code the stub wants pi to observe.
    pub exit_code: i32,
}

/// Callback interface implemented by test harnesses (XCTest /
/// integration tests) to record tool invocations and return canned
/// output, replacing the production iSH kernel for VAL-IOS-PI-011.
///
/// Methods are `&self` so the harness can keep an interior-mutable
/// record of every call; UniFFI binds it as a foreign callback.
#[uniffi::export(callback_interface)]
pub trait PiIshExec: Send + Sync {
    /// Run `command` against the stub. `cwd` is the working
    /// directory as a UTF-8 string (empty when pi did not specify
    /// one); `timeout_ms` mirrors pi's `bash` timeout semantics.
    fn exec(&self, command: String, cwd: String, timeout_ms: Option<u64>) -> PiIshExecOutput;
}

/// Adapter that bridges a UniFFI-supplied [`PiIshExec`] callback
/// into the real `pi_mobile_client::tools::ish::IshExec` trait.
///
/// Holds the callback as `Arc<dyn PiIshExec>` so it can be cloned
/// into multiple tool-registry instantiations across the agent's
/// lifetime. Lossy `to_string_lossy` on `cwd` is fine — paths the
/// agent passes are always valid UTF-8 (pi's working directories
/// originate as `String` in `PiSessionConfig`).
pub(crate) struct PiIshExecAdapter {
    inner: Arc<dyn PiIshExec>,
}

impl PiIshExecAdapter {
    pub(crate) fn new(inner: Arc<dyn PiIshExec>) -> Self {
        Self { inner }
    }
}

impl IshExec for PiIshExecAdapter {
    fn exec(&self, command: &str, cwd: Option<&Path>, timeout_ms: Option<u64>) -> IshExecOutput {
        let cwd_str = cwd
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let result = self
            .inner
            .exec(command.to_string(), cwd_str, timeout_ms);
        IshExecOutput {
            stdout: result.stdout,
            exit_code: result.exit_code,
        }
    }
}
