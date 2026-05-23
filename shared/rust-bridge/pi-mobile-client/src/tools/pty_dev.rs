//! `PtyDevToolFactory` — macOS host-shell tool factory used by
//! `pi-server-runner` and other host-side smoke tests.
//!
//! The factory layers no platform-specific shell sandbox on top of
//! pi's default tool registry. The model still calls the standard
//! `bash` tool, which executes against the macOS host shell. This
//! mirrors what iOS's [`IshToolFactory`](super::ish::IshToolFactory)
//! does in spirit (replace pi's `BashTool` with a sandboxed exec
//! surface), but on macOS we deliberately keep `BashTool` so the
//! host shell stands in for the iSH Alpine fakefs. It is **not**
//! suitable for production mobile builds.

use std::path::Path;

use pi::sdk::{Config, ToolFactory, ToolRegistry, default_tool_registry};

/// `ToolFactory` impl that yields pi's stock tool registry.
///
/// The registry returned is exactly what pi's
/// [`create_agent_session`](pi::sdk::create_agent_session) would build
/// when no factory is supplied. We expose it as a distinct type so the
/// in-process runtime path can pick it explicitly (and so callers can
/// tell from the type alone that they are running against the host
/// shell, not iSH/proot).
#[derive(Debug, Default)]
pub struct PtyDevToolFactory;

impl PtyDevToolFactory {
    /// Create a new `PtyDevToolFactory`.
    pub const fn new() -> Self {
        Self
    }
}

impl ToolFactory for PtyDevToolFactory {
    fn create_tool_registry(
        &self,
        enabled: &[&str],
        cwd: &Path,
        config: &Config,
    ) -> ToolRegistry {
        default_tool_registry(enabled, cwd, config)
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_enabled() -> Vec<&'static str> {
        vec!["read", "bash", "edit", "write", "grep", "find", "ls"]
    }

    #[test]
    fn pty_dev_registry_contains_bash() {
        let factory = PtyDevToolFactory::new();
        let enabled = default_enabled();
        let cwd = std::env::temp_dir();
        let config = Config::default();
        let registry = factory.create_tool_registry(&enabled, &cwd, &config);

        // The macOS dev factory keeps pi's stock BashTool, since the
        // host shell is the stand-in for iSH.
        assert!(
            registry.get("bash").is_some(),
            "PtyDev registry must register the bash tool"
        );
        for name in ["read", "write", "edit", "grep", "find", "ls"] {
            assert!(
                registry.get(name).is_some(),
                "PtyDev registry must register the `{name}` tool"
            );
        }
    }
}
