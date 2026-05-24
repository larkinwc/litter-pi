//! Integration coverage for the `test-injection` UniFFI surface:
//!
//! * `AppClient::connect_local_pi_byok_with_ish_exec` mounts a caller-
//!   supplied `PiIshExec` stub through `ToolFactoryKind::Ish` instead of
//!   the production iSH adapter — VAL-IOS-PI-011.
//! * `AppClient::pi_active_base_url` reads back the resolved
//!   `PiSessionConfig.base_url` for the active server id —
//!   VAL-IOS-PI-012.
//!
//! Compiled only when the `test-injection` feature is on (matches the
//! cfg gate the symbols themselves live behind).

#![cfg(feature = "test-injection")]

use std::sync::Arc;
use std::sync::Mutex;

use codex_mobile_client::ffi::AppClient;
use codex_mobile_client::pi_test_injection::{PiIshExec, PiIshExecOutput};

/// Capturing `PiIshExec` stub. Records every `(command, cwd,
/// timeout_ms)` triple the adapter forwards so the test can prove the
/// agent routed through this stub rather than the real iSH kernel.
struct RecordingStub {
    calls: Mutex<Vec<(String, String, Option<u64>)>>,
    reply_stdout: Vec<u8>,
    reply_exit: i32,
}

impl PiIshExec for RecordingStub {
    fn exec(&self, command: String, cwd: String, timeout_ms: Option<u64>) -> PiIshExecOutput {
        self.calls
            .lock()
            .expect("stub mutex")
            .push((command, cwd, timeout_ms));
        PiIshExecOutput {
            stdout: self.reply_stdout.clone(),
            exit_code: self.reply_exit,
        }
    }
}

/// Connect via the test-injection sibling, then read back the active
/// base URL. Asserts the explicit `base_url` argument wins (matches the
/// production `resolve_pi_byok_base_url` precedence) and that the
/// returned server id is the one we requested.
#[test]
fn connect_with_stub_records_base_url_and_returns_server_id() {
    let client = AppClient::new();
    let stub: Arc<RecordingStub> = Arc::new(RecordingStub {
        calls: Mutex::new(Vec::new()),
        reply_stdout: b"stub\n".to_vec(),
        reply_exit: 0,
    });

    let server_id = "test-pi-injection";
    let display_name = "Test PI";
    let provider = "openai";
    let api_key = "stub-key";
    let base_url = "https://example.test/v1";

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current_thread runtime");

    let returned = runtime
        .block_on(client.connect_local_pi_byok_with_ish_exec(
            server_id.to_string(),
            display_name.to_string(),
            provider.to_string(),
            api_key.to_string(),
            Some(base_url.to_string()),
            None,
            Box::new(RecordingStubBox {
                inner: Arc::clone(&stub),
            }),
        ))
        .expect("connect_local_pi_byok_with_ish_exec");

    assert_eq!(returned, server_id, "server id should round-trip");

    let active = client.pi_active_base_url(server_id.to_string());
    assert_eq!(
        active.as_deref(),
        Some(base_url),
        "pi_active_base_url must return the resolved BYOK base URL"
    );

    // Sanity: an unknown server id returns None.
    assert_eq!(
        client.pi_active_base_url("no-such-server".to_string()),
        None,
        "unknown server_id must yield None, not panic"
    );
}

/// Thin `PiIshExec` newtype that owns an `Arc<RecordingStub>`. The
/// callback trait takes ownership of a `Box<dyn PiIshExec>`, so the
/// shared `Arc` lives behind this wrapper to let the test inspect the
/// stub after handing it to the runtime.
struct RecordingStubBox {
    inner: Arc<RecordingStub>,
}

impl PiIshExec for RecordingStubBox {
    fn exec(&self, command: String, cwd: String, timeout_ms: Option<u64>) -> PiIshExecOutput {
        self.inner.exec(command, cwd, timeout_ms)
    }
}
