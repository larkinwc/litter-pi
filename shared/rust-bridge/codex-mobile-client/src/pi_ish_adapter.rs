//! Bridge between `pi-mobile-client`'s `IshExec` trait and the iSH
//! runtime embedded in this crate (`crate::ish_runtime::run`).
//!
//! Lives here rather than in `pi-mobile-client` because the iSH kernel
//! is only linked on the iOS device/simulator lane and is owned by
//! `codex-mobile-client`. The iOS BYOK pi start path constructs one of
//! these adapters and threads it through
//! `pi_mobile_client::ToolFactoryKind::Ish`, which the runtime bridge
//! mounts as the pi `bash` tool.

use std::path::Path;

use pi_mobile_client::tools::ish::{IshExec, IshExecOutput};

/// `IshExec` impl that forwards to `crate::ish_runtime::run`.
///
/// On non-iOS targets `ish_runtime` is not compiled in; the adapter is
/// only constructed on iOS device/simulator builds so this module is
/// gated to the same `cfg`.
pub(crate) struct IshRuntimeExec;

impl IshExec for IshRuntimeExec {
    fn exec(
        &self,
        command: &str,
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> IshExecOutput {
        let cwd_str = cwd.map(|p| p.to_string_lossy().into_owned());
        let cwd_ref = cwd_str.as_deref();
        let (exit_code, stdout) = crate::ish_runtime::run(command, cwd_ref, timeout_ms);
        IshExecOutput { stdout, exit_code }
    }
}
