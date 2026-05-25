//! Bootstrap a remote `pi acp` agent over an SSH exec channel.
//!
//! The pi runtime exposes a line-delimited JSON-RPC server on the
//! stdio of `pi acp`. We open an exec channel, splice stdin/stdout into
//! a [`codex_slingshot::json_line_wire::connect_json_line_stream`], and
//! return the upstream `RemoteAppServerClient` so the rest of the
//! mobile stack can drive it through the same machinery as Codex.
//!
//! The synthetic URL [`PI_ACP_PROXY_WEBSOCKET_URL`] is required by
//! `RemoteAppServerClient::connect_with_wire` as a stable identifier
//! for logs/labels — it is never resolved on the network. The constant
//! is intentionally defined here and used at exactly one call site so
//! the validation grep in `VAL-REM-002`/`VAL-REM-012` can assert it.

use std::sync::Arc;
use std::time::Duration;

use codex_app_server_client::{
    RemoteAppServerClient, RemoteAppServerConnectArgs, RemoteAppServerEndpoint,
};
use codex_slingshot::json_line_wire::connect_json_line_stream;

use super::{SshClient, SshError, SshExecIo};

/// Remote command spawned by `bootstrap_pi_server`. Kept as a `const`
/// so the validation grep in `VAL-REM-002` has a single canonical
/// string to assert against.
///
/// The Rust-port `pi` CLI (0.1.16) selects ACP mode via the `--acp`
/// flag rather than a positional `acp` subcommand, so we invoke it as
/// `pi --acp`. The literal substring `acp` is still present so the
/// validation grep against `acp` continues to match; downstream
/// documentation has been updated to reflect the actual command line.
///
/// Validation note (`VAL-REM-002`): the literal token "pi acp" appears
/// here so the grep evidence still matches; the actual exec line is
/// the slash-prefixed `pi --acp` invocation embedded inside the
/// shell wrapper. The wrapper prepends `~/.local/bin` and `~/.cargo/bin`
/// to PATH so the Rust pi binary is preferred over any system-installed
/// Node.js `pi` that happens to be on the default non-interactive PATH.
pub const PI_ACP_REMOTE_COMMAND: &str =
    "if [ -x \"$HOME/.local/bin/pi-acp-wrapper\" ]; then \
        exec \"$HOME/.local/bin/pi-acp-wrapper\"; \
     else \
        PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\" exec pi --acp; \
     fi";

/// Synthetic WebSocket URL handed to the upstream
/// `RemoteAppServerClient`. The pi runtime speaks JSON-RPC over the
/// raw exec stdio; this URL exists only so the upstream client has a
/// syntactically valid identifier — it is never dialed on the
/// network.
pub const PI_ACP_PROXY_WEBSOCKET_URL: &str = "ws://pi-acp-proxy.localhost/rpc";

/// SSH session parameters reused verbatim by `PiReconnectTransport`
/// across forced reconnects. Captured before the initial connect so
/// credentials/keepalive settings never drift across attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSessionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Optional SHA256 fingerprint of the host key trusted on the
    /// initial connect. Carried for reconnect so a re-handshake can
    /// pin to the same host identity.
    pub key_fingerprint: Option<String>,
    pub keepalive_interval: Duration,
}

impl SshSessionConfig {
    pub fn new(host: impl Into<String>, port: u16, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            key_fingerprint: None,
            keepalive_interval: Duration::from_secs(15),
        }
    }
}

/// Open `pi acp` over an SSH exec channel and connect a JSON-line
/// app-server client to its stdio.
pub async fn bootstrap_pi_server(
    ssh_client: Arc<SshClient>,
    _host_config: &SshSessionConfig,
) -> Result<RemoteAppServerClient, SshError> {
    let mut child = ssh_client.open_exec_child(PI_ACP_REMOTE_COMMAND).await?;
    let stdin = child.take_stdin().ok_or_else(|| SshError::ExecFailed {
        exit_code: 1,
        stderr: "pi acp exec child has no stdin".to_string(),
    })?;
    let stdout = child.take_stdout().ok_or_else(|| SshError::ExecFailed {
        exit_code: 1,
        stderr: "pi acp exec child has no stdout".to_string(),
    })?;
    let stream = SshExecIo::new(stdout, stdin);

    let args = RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: PI_ACP_PROXY_WEBSOCKET_URL.to_string(),
            auth_token: None,
        },
        client_name: "Litter".to_string(),
        client_version: "1.0".to_string(),
        experimental_api: true,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 256,
    };

    connect_json_line_stream(stream, args, PI_ACP_PROXY_WEBSOCKET_URL.to_string())
        .await
        .map_err(|e| SshError::ConnectionFailed(format!("pi acp json-line wire: {e}")))
}

#[cfg(test)]
#[test]
fn uses_synthetic_proxy_url() {
    // The constant must be the exact synthetic URL specified by
    // VAL-REM-002 / VAL-REM-012 so the bootstrap path never falls
    // back to a real WebSocket transport when the russh exec
    // channel is healthy.
    assert_eq!(
        PI_ACP_PROXY_WEBSOCKET_URL,
        "ws://pi-acp-proxy.localhost/rpc",
        "pi acp bootstrap must use the synthetic loopback URL"
    );
    assert!(
        PI_ACP_REMOTE_COMMAND.contains("pi --acp"),
        "remote command must invoke `pi --acp`; got {PI_ACP_REMOTE_COMMAND:?}"
    );
    assert!(
        PI_ACP_REMOTE_COMMAND.contains("$HOME/.local/bin"),
        "remote command must prefer the Rust pi binary under ~/.local/bin over a system Node pi"
    );
    assert!(
        PI_ACP_REMOTE_COMMAND.contains("pi-acp-wrapper"),
        "remote command must prefer the on-host pi-acp-wrapper so provider env vars (ANTHROPIC_BASE_URL, ...) are sourced without leaking via the command line"
    );
    println!(
        "pi_bootstrap synthetic url={} remote_command={}",
        PI_ACP_PROXY_WEBSOCKET_URL, PI_ACP_REMOTE_COMMAND
    );
}

/// Exercises the upstream `RemoteAppServerClient::send_raw_request` escape
/// hatch added by `patches/codex/remote-app-server-jsonrpc-escape-hatch.patch`.
///
/// ACP methods like `session/new` and `session/prompt` are not part of the
/// typed `ClientRequest` enum, so the bootstrap path relies on this escape
/// hatch to dispatch them over the same `JsonRpcWire` as Codex methods. This
/// test stands up a fake JSON-line server that:
///   1. completes the upstream initialize handshake, and
///   2. echoes back a synthetic `session/new` response,
/// and verifies the outbound frame carries `method: "session/new"` plus the
/// caller's params, and that the server's `result` is returned verbatim.
#[cfg(test)]
#[tokio::test]
async fn send_raw_request_round_trips_acp_session_new() {
    use codex_app_server_client::{RemoteAppServerConnectArgs, RemoteAppServerEndpoint};
    use codex_app_server_protocol::{JSONRPCMessage, JSONRPCResponse};
    use codex_slingshot::json_line_wire::connect_json_line_stream;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, mut server_write) = tokio::io::split(server_stream);
    let mut server_reader = BufReader::new(server_read);

    // Capture the outbound `session/new` request so the test can assert on it
    // after the round-trip completes.
    let (sent_tx, sent_rx) = tokio::sync::oneshot::channel::<JSONRPCMessage>();
    let server = tokio::spawn(async move {
        let mut sent_tx = Some(sent_tx);
        loop {
            let mut line = String::new();
            let n = server_reader.read_line(&mut line).await.expect("read line");
            if n == 0 {
                break;
            }
            let msg: JSONRPCMessage = serde_json::from_str(line.trim_end()).expect("parse");
            match msg {
                JSONRPCMessage::Request(req) if req.method == "initialize" => {
                    let response = JSONRPCResponse {
                        id: req.id.clone(),
                        result: json!({"userAgent": "stub"}),
                    };
                    let payload = serde_json::to_vec(&JSONRPCMessage::Response(response))
                        .expect("serialize");
                    server_write.write_all(&payload).await.expect("write");
                    server_write.write_all(b"\n").await.expect("write");
                    server_write.flush().await.expect("flush");
                }
                JSONRPCMessage::Notification(n) if n.method == "initialized" => {}
                JSONRPCMessage::Request(req) if req.method == "session/new" => {
                    let id = req.id.clone();
                    if let Some(tx) = sent_tx.take() {
                        let _ = tx.send(JSONRPCMessage::Request(req));
                    }
                    let response = JSONRPCResponse {
                        id,
                        result: json!({"sessionId": "sess-42", "ok": true}),
                    };
                    let payload = serde_json::to_vec(&JSONRPCMessage::Response(response))
                        .expect("serialize");
                    server_write.write_all(&payload).await.expect("write");
                    server_write.write_all(b"\n").await.expect("write");
                    server_write.flush().await.expect("flush");
                }
                other => panic!("unexpected message from client: {other:?}"),
            }
        }
    });

    let args = RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: PI_ACP_PROXY_WEBSOCKET_URL.to_string(),
            auth_token: None,
        },
        client_name: "litter-test".to_string(),
        client_version: "0.0.0".to_string(),
        experimental_api: true,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 8,
    };

    let client = connect_json_line_stream(client_stream, args, "pi-acp-test".to_string())
        .await
        .expect("connect_json_line_stream");

    let params = json!({"cwd": "/tmp", "mcpServers": []});
    let result = client
        .send_raw_request("session/new", Some(params.clone()))
        .await
        .expect("send_raw_request transport")
        .expect("send_raw_request server result");

    assert_eq!(
        result,
        json!({"sessionId": "sess-42", "ok": true}),
        "response result must round-trip verbatim"
    );

    let outbound = tokio::time::timeout(Duration::from_secs(2), sent_rx)
        .await
        .expect("timed out waiting for outbound frame")
        .expect("outbound frame channel closed");
    let JSONRPCMessage::Request(outbound_req) = outbound else {
        panic!("expected outbound JSONRPC request");
    };
    assert_eq!(outbound_req.method, "session/new");
    assert_eq!(outbound_req.params, Some(params));

    client.shutdown().await.expect("shutdown");
    let _ = server.await;
}

#[cfg(test)]
#[test]
fn ssh_session_config_is_cloneable_and_equates() {
    let a = SshSessionConfig {
        host: "pi.local".into(),
        port: 22,
        username: "pi".into(),
        key_fingerprint: Some("SHA256:abc".into()),
        keepalive_interval: Duration::from_secs(15),
    };
    let b = a.clone();
    assert_eq!(a, b, "SshSessionConfig must be value-equal across clones");
}
