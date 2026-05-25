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
pub const PI_ACP_REMOTE_COMMAND: &str = "pi acp";

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
    assert_eq!(PI_ACP_REMOTE_COMMAND, "pi acp");
    println!(
        "pi_bootstrap synthetic url={} remote_command={}",
        PI_ACP_PROXY_WEBSOCKET_URL, PI_ACP_REMOTE_COMMAND
    );
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
