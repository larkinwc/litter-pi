//! `IshToolFactory` — iOS replacement for pi's built-in `BashTool`.
//!
//! Routes the pi agent's `shell` (a.k.a. `bash`) tool through the iSH Alpine
//! fakefs that already lives inside the Litter app. Read/Write/Edit/Grep/
//! Find/Ls tools are left in their stock pure-Rust form (operating on the
//! iOS sandbox paths under `~/Documents/home/pi/`).
//!
//! The factory derives the rest of the registry from
//! [`pi::sdk::default_tool_registry`] and then replaces the `bash` entry
//! with an [`IshTool`] whose [`Tool::execute`] routes commands through a
//! caller-supplied [`IshExec`] implementation. In production the
//! `IshExec` is a thin wrapper around
//! `codex-mobile-client::ish_runtime::run`; in tests it is a recording
//! stub.
//!
//! Keeping the runtime call behind a small trait means
//! `pi-mobile-client` does not have to depend on `codex-mobile-client`,
//! and lets us unit-test the wiring without touching the real iSH
//! kernel.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pi::sdk::{
    Config, ContentBlock, Error, Result, TextContent, Tool, ToolFactory, ToolOutput, ToolUpdate,
    default_tool_registry,
};

/// Output produced by an [`IshExec`] invocation.
///
/// Consumed by the iOS BYOK start path that mounts an `IshExec` adapter
/// over `codex-mobile-client::ish_runtime`. The adapter lives in
/// `codex-mobile-client` (which depends on `pi-mobile-client`, not the
/// other way around) and is wired in through `ToolFactoryKind::Ish`.
#[derive(Debug, Clone)]
pub struct IshExecOutput {
    /// Raw stdout+stderr bytes, in the order iSH produced them.
    pub stdout: Vec<u8>,
    /// Exit code from the iSH command. Non-zero values are surfaced to
    /// the agent as a tool error.
    pub exit_code: i32,
}

/// Abstraction over the iSH execution surface.
///
/// The production impl forwards to `codex-mobile-client::ish_runtime::run`;
/// tests supply a recording stub. Trait methods take `&self` and a few
/// owned arguments so the impl can be stored as `Arc<dyn IshExec>` and
/// shared across the agent loop without lifetime juggling.
///
/// Implemented by an adapter in `codex-mobile-client` (the iOS BYOK
/// start path wires `codex-mobile-client::ish_runtime::run` behind it
/// and threads the adapter through `ToolFactoryKind::Ish`).
pub trait IshExec: Send + Sync {
    /// Run `command` inside the iSH fakefs.
    ///
    /// `cwd` is the working directory to execute in (relative paths are
    /// resolved against the fakefs root, not the host). `timeout_ms`
    /// matches pi's `bash` timeout semantics.
    fn exec(&self, command: &str, cwd: Option<&Path>, timeout_ms: Option<u64>) -> IshExecOutput;
}

/// `pi::sdk::Tool` impl that replaces `BashTool` for the iOS pi runtime.
///
/// Owns the working directory it was created against (for diagnostics
/// only — the actual path lives inside the iSH fakefs) and an
/// `Arc<dyn IshExec>` it forwards every call to.
pub struct IshTool {
    cwd: PathBuf,
    exec: Arc<dyn IshExec>,
}

impl IshTool {
    /// Construct an `IshTool` bound to `cwd` and `exec`.
    pub fn new(cwd: &Path, exec: Arc<dyn IshExec>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            exec,
        }
    }

    /// Synchronous entrypoint used by tests and any in-process caller
    /// that wants to drive the tool without going through the full
    /// `pi::sdk::Tool::execute` async surface. Mirrors `IshTool::call`
    /// as described in the architecture doc:
    /// `call(json) -> Result<String>`.
    pub fn call(&self, input: serde_json::Value) -> Result<String> {
        let command = input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::validation("ish tool: missing `command` field"))?;
        let timeout_ms = input
            .get("timeout")
            .and_then(serde_json::Value::as_u64)
            .map(|s| s.saturating_mul(1000));

        let out = self
            .exec
            .exec(command, Some(self.cwd.as_path()), timeout_ms);
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.exit_code != 0 {
            // The agent surfaces the stdout verbatim; we still return Ok
            // so the assistant can inspect the failure. Append the exit
            // code so the model has something to reason about.
            let mut combined = text;
            if !combined.ends_with('\n') && !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&format!("Command exited with code {}", out.exit_code));
            return Ok(combined);
        }
        Ok(text)
    }
}

#[async_trait]
impl Tool for IshTool {
    fn name(&self) -> &str {
        // Keep the pi-facing name as `bash` so the model's tool calls
        // (and any provider-side tool schemas) continue to resolve. The
        // architecture doc refers to the slot as "shell"; pi calls it
        // "bash". They are the same slot.
        "bash"
    }

    fn label(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command inside the iSH Alpine fakefs. Returns combined stdout+stderr. Paths are relative to the fakefs root (e.g. /root), NOT the iOS host sandbox."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute inside the iSH Alpine fakefs."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 120; set 0 to disable)."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let text = self.call(input)?;
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error: false,
        })
    }
}

/// `ToolFactory` that produces the pi-on-iOS tool registry.
///
/// Layers the standard pi tool registry via
/// [`pi::sdk::default_tool_registry`], then drops the built-in
/// `BashTool` and substitutes an [`IshTool`] backed by the injected
/// [`IshExec`].
pub struct IshToolFactory {
    exec: Arc<dyn IshExec>,
}

impl IshToolFactory {
    /// Create a new factory bound to `exec`.
    pub fn new(exec: Arc<dyn IshExec>) -> Self {
        Self { exec }
    }
}

impl ToolFactory for IshToolFactory {
    fn create_tool_registry(
        &self,
        enabled: &[&str],
        cwd: &Path,
        config: &Config,
    ) -> pi::sdk::ToolRegistry {
        // Start from the standard registry. This is the same shape
        // `create_agent_session` would build on its own and gives us
        // pi's Read/Write/Edit/Grep/Find/Ls operating on the iOS
        // sandbox cwd.
        let base = default_tool_registry(enabled, cwd, config);
        let mut tools = base.into_tools();

        // Drop pi's BashTool (registered under name "bash") in favour
        // of our IshTool. We intentionally iterate and remove in place
        // rather than building the registry from scratch so future
        // additions to pi's default tool set still flow through.
        tools.retain(|t| t.name() != "bash");

        // If "bash" was in the enabled allow-list (the default), swap
        // in the iSH-backed tool. Otherwise leave it dropped — the
        // caller explicitly disabled shell access.
        if enabled.contains(&"bash") {
            tools.push(Box::new(IshTool::new(cwd, Arc::clone(&self.exec))));
        }

        pi::sdk::ToolRegistry::from_tools(tools)
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Recording stub `IshExec` used by tests to assert the tool routes
    /// commands through the runtime exactly once with the expected
    /// arguments.
    #[derive(Default)]
    struct StubExec {
        calls: Mutex<Vec<(String, Option<PathBuf>, Option<u64>)>>,
        reply_stdout: Vec<u8>,
        reply_exit: i32,
    }

    impl StubExec {
        fn with_reply(stdout: &str, exit: i32) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                reply_stdout: stdout.as_bytes().to_vec(),
                reply_exit: exit,
            }
        }
    }

    impl IshExec for StubExec {
        fn exec(
            &self,
            command: &str,
            cwd: Option<&Path>,
            timeout_ms: Option<u64>,
        ) -> IshExecOutput {
            self.calls.lock().expect("stub mutex").push((
                command.to_string(),
                cwd.map(Path::to_path_buf),
                timeout_ms,
            ));
            IshExecOutput {
                stdout: self.reply_stdout.clone(),
                exit_code: self.reply_exit,
            }
        }
    }

    /// Builder for the tool-name allowlist pi's registry expects.
    fn default_enabled() -> Vec<&'static str> {
        vec!["read", "bash", "edit", "write", "grep", "find", "ls"]
    }

    #[test]
    fn ish_tool_factory_registered() {
        let stub = Arc::new(StubExec::with_reply("ROUTED\n", 0));
        let factory = IshToolFactory::new(Arc::clone(&stub) as Arc<dyn IshExec>);

        let enabled = default_enabled();
        let cwd = std::env::temp_dir();
        let config = Config::default();
        let registry = factory.create_tool_registry(&enabled, &cwd, &config);

        // The `bash` slot must resolve and have the canonical name.
        let tool = registry
            .get("bash")
            .expect("bash slot present in pi registry");
        assert_eq!(tool.name(), "bash");

        // Drive the registered tool. If it really is our IshTool it
        // will route through the stub; pi's built-in BashTool would
        // shell out to the host instead and the stub would record
        // nothing.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let output = rt
            .block_on(async {
                tool.execute(
                    "test-call-id",
                    serde_json::json!({"command": "echo ROUTED"}),
                    None,
                )
                .await
            })
            .expect("tool execute ok");

        // Stub must have observed the exec.
        let calls = stub.calls.lock().expect("stub mutex");
        assert_eq!(
            calls.len(),
            1,
            "registry's bash slot did not route through IshExec (stub recorded {} calls)",
            calls.len()
        );
        assert_eq!(calls[0].0, "echo ROUTED");

        // And the wrapped text must come from the stub's stdout.
        match output.content.first() {
            Some(pi::sdk::ContentBlock::Text(t)) => assert_eq!(t.text, "ROUTED\n"),
            other => panic!("unexpected tool output content: {other:?}"),
        }

        // Read/Write/Edit/Grep/Find/Ls must remain in the registry.
        for name in ["read", "write", "edit", "grep", "find", "ls"] {
            assert!(
                registry.get(name).is_some(),
                "pure-Rust tool `{name}` missing from registry"
            );
        }
    }

    #[test]
    fn ish_tool_routes_through_ish_runtime() {
        let stub = Arc::new(StubExec::with_reply("hello\n", 0));
        let tool = IshTool::new(Path::new("/root"), Arc::clone(&stub) as Arc<dyn IshExec>);

        let output = tool
            .call(serde_json::json!({"command": "echo hello"}))
            .expect("tool call ok");
        assert_eq!(output, "hello\n", "tool returned stub stdout verbatim");

        let calls = stub.calls.lock().expect("stub mutex");
        assert_eq!(calls.len(), 1, "stub exec called exactly once");
        assert_eq!(calls[0].0, "echo hello", "literal command forwarded");
        assert_eq!(
            calls[0].1.as_deref(),
            Some(Path::new("/root")),
            "cwd forwarded"
        );
        assert_eq!(calls[0].2, None, "no timeout supplied");
    }
}
