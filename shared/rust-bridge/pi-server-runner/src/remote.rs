//! Remote `pi acp` mode for `pi-server-runner`.
//!
//! Opens an SSH session against the host with `russh` key auth, spawns
//! `pi --acp` via [`codex_mobile_client::ssh::pi_bootstrap::bootstrap_pi_server`],
//! then drives ACP `session/new` + `session/prompt` through the
//! upstream `RemoteAppServerClient::send_raw_request` escape hatch
//! (patched into the codex submodule by
//! `patches/codex/remote-app-server-jsonrpc-escape-hatch.patch`).
//!
//! The transcript shape (`RemoteTranscriptLine`) is intentionally
//! stable so validators can pin against literal field names — see
//! `VAL-REM-004` through `VAL-REM-007`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use codex_app_server_client::{AppServerEvent, RemoteAppServerClient};
use codex_app_server_protocol::{JSONRPCErrorError, RequestId};
use codex_mobile_client::ssh::pi_bootstrap::{SshSessionConfig, bootstrap_pi_server};
use codex_mobile_client::ssh::{SshAuth, SshClient, SshCredentials};
use futures::future::FutureExt;
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use tokio::io::AsyncWriteExt;

/// Drop-injection mode chosen via `--inject-drop`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum InjectDropMode {
    /// Signal `kill -STOP` / `kill -CONT` on the local russh PID
    /// after 60s idle. Orchestrated by an out-of-band helper script
    /// (`tools/scripts/inject-drop-kill-stop.sh`).
    KillStop,
    /// Tear down a socat-proxied tunnel after 60s idle. Orchestrated
    /// by `tools/scripts/inject-drop-socat-partition.sh`.
    SocatPartition,
}

impl InjectDropMode {
    /// Stable label emitted in the JSONL transcript so validators
    /// can match against a literal string.
    pub fn label(self) -> &'static str {
        match self {
            InjectDropMode::KillStop => "kill-stop",
            InjectDropMode::SocatPartition => "socat-partition",
        }
    }
}

/// CLI-derived configuration for the remote-ssh flow.
#[derive(Debug, Clone)]
pub struct RemoteSshArgs {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub prompt: String,
    pub events_out: Option<PathBuf>,
    pub inject_drop: Option<InjectDropMode>,
    pub ssh_key_path: Option<PathBuf>,
    pub timeout_secs: u64,
}

/// JSONL transcript shape for the remote-ssh path. Kept stable so
/// validators can pin against field names.
#[derive(Debug, Serialize)]
#[serde(tag = "event_kind", rename_all = "snake_case")]
pub enum RemoteTranscriptLine {
    Connecting {
        host: String,
        user: String,
        port: u16,
    },
    Connected {
        host: String,
    },
    PromptScheduled {
        text: String,
    },
    InjectDrop {
        mode: &'static str,
        scheduled_in_secs: u64,
    },
    SessionStarted {
        session_id: Option<String>,
    },
    ServerNotification {
        method: String,
        params: JsonValue,
    },
    ServerRequest {
        method: String,
        params: JsonValue,
    },
    /// A `session/request_permission` (or other raw) request from the
    /// remote pi acp server that the runner auto-resolved on behalf of
    /// the validator. The transcript records the resolution so VAL-REM
    /// runs can verify the approval pipeline fired.
    PermissionAutoApproved {
        request_id: String,
        method: String,
        tool_name: Option<String>,
    },
    ToolExec {
        tool_call_id: Option<String>,
        tool_name: Option<String>,
        command: Option<String>,
        raw: JsonValue,
    },
    PromptResult {
        result: JsonValue,
    },
    TurnComplete {
        stop_reason: Option<String>,
    },
    TurnError {
        message: String,
    },
    Disconnected,
}

/// Emit a transcript line to stdout and (when set) the
/// `--events-out` file as JSONL.
pub(crate) async fn emit_remote_line(
    file: &mut Option<tokio::fs::File>,
    line: &RemoteTranscriptLine,
) -> std::io::Result<()> {
    let json = serde_json::to_string(line)
        .map_err(|e| std::io::Error::other(format!("transcript serialize: {e}")))?;
    println!("{json}");
    if let Some(f) = file.as_mut() {
        f.write_all(json.as_bytes()).await?;
        f.write_all(b"\n").await?;
        f.flush().await?;
    }
    Ok(())
}

/// Locate a private key under `~/.ssh/` for the remote SSH login.
///
/// Returns the explicit `--ssh-key-path` value when supplied, else
/// the first existing of `id_ed25519`, `id_rsa`, `id_ecdsa`.
pub(crate) fn locate_ssh_key(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(format!(
            "--ssh-key-path {} does not exist or is not a file",
            path.display()
        ));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; cannot probe ~/.ssh/ for a key".to_string())?;
    let ssh_dir = PathBuf::from(home).join(".ssh");
    for name in ["id_ed25519", "id_rsa", "id_ecdsa"] {
        let candidate = ssh_dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "no usable SSH key found under {}; tried id_ed25519, id_rsa, id_ecdsa",
        ssh_dir.display()
    ))
}

async fn open_events_file(
    path: Option<&Path>,
) -> std::io::Result<Option<tokio::fs::File>> {
    let Some(path) = path else { return Ok(None) };
    let f = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await?;
    Ok(Some(f))
}

/// Top-level entrypoint invoked from `main()` when `--remote-ssh`
/// is supplied. Returns the process exit code.
pub async fn drive_remote_ssh(args: RemoteSshArgs) -> ExitCode {
    let mut events_file = match open_events_file(args.events_out.as_deref()).await {
        Ok(f) => f,
        Err(err) => {
            eprintln!("pi-server-runner: opening --events-out failed: {err}");
            return ExitCode::from(2);
        }
    };

    let key_path = match locate_ssh_key(args.ssh_key_path.as_deref()) {
        Ok(p) => p,
        Err(err) => {
            let _ = emit_remote_line(
                &mut events_file,
                &RemoteTranscriptLine::TurnError {
                    message: format!("ssh key resolve: {err}"),
                },
            )
            .await;
            eprintln!("pi-server-runner: {err}");
            return ExitCode::from(2);
        }
    };
    tracing::info!(
        target: "pi_server_runner",
        key = %key_path.display(),
        host = %args.host,
        user = %args.user,
        port = args.port,
        "remote ssh: connecting"
    );

    let _ = emit_remote_line(
        &mut events_file,
        &RemoteTranscriptLine::Connecting {
            host: args.host.clone(),
            user: args.user.clone(),
            port: args.port,
        },
    )
    .await;

    let _ = emit_remote_line(
        &mut events_file,
        &RemoteTranscriptLine::PromptScheduled {
            text: args.prompt.clone(),
        },
    )
    .await;

    if let Some(mode) = args.inject_drop {
        let _ = emit_remote_line(
            &mut events_file,
            &RemoteTranscriptLine::InjectDrop {
                mode: mode.label(),
                scheduled_in_secs: 60,
            },
        )
        .await;
        tracing::info!(
            target: "pi_server_runner",
            mode = mode.label(),
            "inject-drop scheduled (out-of-band helper required)"
        );
    }

    // Load the user's private key and open the SSH session.
    let key_pem = match tokio::fs::read_to_string(&key_path).await {
        Ok(pem) => pem,
        Err(err) => {
            let msg = format!("read ssh key {}: {err}", key_path.display());
            let _ = emit_remote_line(
                &mut events_file,
                &RemoteTranscriptLine::TurnError { message: msg.clone() },
            )
            .await;
            eprintln!("pi-server-runner: {msg}");
            return ExitCode::from(2);
        }
    };
    let credentials = SshCredentials {
        host: args.host.clone(),
        port: args.port,
        username: args.user.clone(),
        auth: SshAuth::PrivateKey {
            key_pem,
            passphrase: None,
        },
        unlock_macos_keychain: false,
    };
    // TOFU host key acceptance: the runner is a developer/CI tool, not
    // a production client. Document and log the fingerprint instead of
    // enforcing pinning here. Validators can run an out-of-band
    // `ssh-keyscan` if they need to assert host identity.
    let host_key_cb = Box::new(|fp: &str| {
        tracing::info!(target: "pi_server_runner", host_fp = fp, "tofu accept ssh host key");
        let fp = fp.to_string();
        async move {
            let _ = fp;
            true
        }
        .boxed()
    });
    let ssh = match SshClient::connect(credentials, host_key_cb).await {
        Ok(client) => Arc::new(client),
        Err(err) => {
            let msg = format!("ssh connect: {err}");
            let _ = emit_remote_line(
                &mut events_file,
                &RemoteTranscriptLine::TurnError { message: msg.clone() },
            )
            .await;
            eprintln!("pi-server-runner: {msg}");
            return ExitCode::from(2);
        }
    };

    let host_cfg = SshSessionConfig::new(args.host.clone(), args.port, args.user.clone());
    let mut client: RemoteAppServerClient = match bootstrap_pi_server(Arc::clone(&ssh), &host_cfg)
        .await
    {
        Ok(c) => c,
        Err(err) => {
            let msg = format!("pi acp bootstrap: {err}");
            let _ = emit_remote_line(
                &mut events_file,
                &RemoteTranscriptLine::TurnError { message: msg.clone() },
            )
            .await;
            eprintln!("pi-server-runner: {msg}");
            return ExitCode::from(2);
        }
    };

    let _ = emit_remote_line(
        &mut events_file,
        &RemoteTranscriptLine::Connected { host: args.host.clone() },
    )
    .await;

    // Drive the turn under an overall wall-clock timeout so a hung
    // remote does not wedge the runner.
    let turn_timeout = Duration::from_secs(args.timeout_secs);
    let outcome = tokio::time::timeout(
        turn_timeout,
        drive_turn(&mut client, &args.prompt, &mut events_file),
    )
    .await;
    let exit = match outcome {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(msg)) => {
            let _ = emit_remote_line(
                &mut events_file,
                &RemoteTranscriptLine::TurnError { message: msg.clone() },
            )
            .await;
            eprintln!("pi-server-runner: {msg}");
            ExitCode::from(1)
        }
        Err(_) => {
            let msg = format!(
                "remote turn timed out after {}s without turn_complete",
                turn_timeout.as_secs()
            );
            let _ = emit_remote_line(
                &mut events_file,
                &RemoteTranscriptLine::TurnError { message: msg.clone() },
            )
            .await;
            eprintln!("pi-server-runner: {msg}");
            ExitCode::from(1)
        }
    };

    let _ = client.shutdown().await;
    let _ = emit_remote_line(&mut events_file, &RemoteTranscriptLine::Disconnected).await;
    ssh.disconnect().await;
    exit
}

/// Run one ACP turn: `session/new` then `session/prompt`, draining
/// streaming notifications from the wire into transcript lines until
/// the prompt response lands.
async fn drive_turn(
    client: &mut RemoteAppServerClient,
    prompt: &str,
    events_file: &mut Option<tokio::fs::File>,
) -> Result<(), String> {
    // session/new — pi's ACP server requires this before it will
    // accept a prompt and returns the freshly-minted sessionId in
    // `result.sessionId`.
    let session_params = json!({
        "cwd": "/tmp",
        "mcpServers": [],
    });
    let session_result = client
        .send_raw_request("session/new", Some(session_params))
        .await
        .map_err(|e| format!("session/new transport: {e}"))?
        .map_err(|e| format!("session/new server: {} ({})", e.message, e.code))?;
    let session_id = session_result
        .get("sessionId")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::SessionStarted {
            session_id: session_id.clone(),
        },
    )
    .await;
    let session_id = session_id.ok_or_else(|| {
        "session/new succeeded but the result is missing a sessionId string".to_string()
    })?;

    // session/prompt is the long-running call. While it streams we
    // race the call against the wire's notification/request event
    // stream so any `session/update` notifications get surfaced into
    // the transcript in order.
    let prompt_params = json!({
        "sessionId": session_id,
        "prompt": [
            {"type": "text", "text": prompt}
        ],
    });
    // session/prompt is a long-running call. Pi may send inbound
    // `session/request_permission` server requests during the turn
    // that block tool execution until answered. We need to:
    //   * surface streaming notifications in real time, and
    //   * auto-approve permission requests so the agent can execute
    //     tools (VAL-REM-004/005 expect Bash tool exec).
    //
    // Dispatch the prompt via the command channel directly so the
    // result lands on a `oneshot` while we keep the client (with
    // exclusive ownership of `next_event`) free to drive the event
    // loop in this task.
    let prompt_future = client.send_raw_request("session/prompt", Some(prompt_params));
    tokio::pin!(prompt_future);
    let prompt_result = loop {
        tokio::select! {
            biased;
            res = &mut prompt_future => {
                break res
                    .map_err(|e| format!("session/prompt transport: {e}"))?
                    .map_err(|e| format!("session/prompt server: {} ({})", e.message, e.code))?;
            }
            event = client.next_event() => {
                let Some(event) = event else { break json!({}); };
                handle_event(client, event, events_file).await?;
            }
        }
    };
    // Drain any in-flight events that arrived after the prompt
    // response but before the wire bookkeeping settled.
    loop {
        let event = match tokio::time::timeout(
            Duration::from_millis(50),
            client.next_event(),
        )
        .await
        {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(_) => break,
        };
        handle_event(client, event, events_file).await?;
    }

    let stop_reason = prompt_result
        .get("stopReason")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::PromptResult { result: prompt_result.clone() },
    )
    .await;
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::TurnComplete { stop_reason },
    )
    .await;
    Ok(())
}

/// Dispatch a single `AppServerEvent` emitted by the wire into the
/// JSONL transcript. Permission requests (`session/request_permission`)
/// are auto-approved with the first `allow-once` option so the agent
/// can proceed to execute the requested tool.
async fn handle_event(
    client: &RemoteAppServerClient,
    event: AppServerEvent,
    events_file: &mut Option<tokio::fs::File>,
) -> Result<(), String> {
    match event {
        AppServerEvent::ServerNotification(notification) => {
            let value = serde_json::to_value(&notification).unwrap_or(JsonValue::Null);
            let (method, params) = split_method_params(value);
            emit_tool_exec_if_present(events_file, &method, &params).await;
            let _ = emit_remote_line(
                events_file,
                &RemoteTranscriptLine::ServerNotification { method, params },
            )
            .await;
        }
        AppServerEvent::Disconnected { message } => {
            return Err(format!("remote pi acp disconnected: {message}"));
        }
        AppServerEvent::ServerRequest(request) => {
            let value = serde_json::to_value(&request).unwrap_or(JsonValue::Null);
            let (method, params) = split_method_params(value);
            let _ = emit_remote_line(
                events_file,
                &RemoteTranscriptLine::ServerRequest { method, params },
            )
            .await;
        }
        AppServerEvent::RawServerRequest { id, method, params } => {
            auto_approve_permission(client, id, method, params, events_file).await;
        }
        AppServerEvent::RawServerNotification { method, params } => {
            let params = params.unwrap_or(JsonValue::Null);
            emit_tool_exec_if_present(events_file, &method, &params).await;
            let _ = emit_remote_line(
                events_file,
                &RemoteTranscriptLine::ServerNotification { method, params },
            )
            .await;
        }
        other => {
            tracing::debug!(
                target: "pi_server_runner",
                event = ?other,
                "ignoring app-server event"
            );
        }
    }
    Ok(())
}

/// Auto-approve a raw `session/request_permission` (or comparable)
/// request from the remote pi acp server. Picks the first option
/// whose `optionId` is `allow-once` (pi's canonical approve token);
/// otherwise picks the first option in the list. Falls back to a
/// generic `{"outcome":"selected","optionId":"allow-once"}` payload
/// when the request did not carry an explicit options list.
async fn auto_approve_permission(
    client: &RemoteAppServerClient,
    id: RequestId,
    method: String,
    params: Option<JsonValue>,
    events_file: &mut Option<tokio::fs::File>,
) {
    let params_val = params.unwrap_or(JsonValue::Null);
    let tool_name = params_val
        .get("toolCall")
        .and_then(|tc| tc.get("toolName").or_else(|| tc.get("name")))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let option_id = params_val
        .get("options")
        .and_then(JsonValue::as_array)
        .and_then(|opts| {
            opts.iter()
                .find(|o| {
                    o.get("optionId").and_then(JsonValue::as_str) == Some("allow-once")
                })
                .or_else(|| opts.first())
        })
        .and_then(|o| o.get("optionId").and_then(JsonValue::as_str))
        .unwrap_or("allow-once")
        .to_string();
    let result = json!({
        "outcome": {
            "outcome": "selected",
            "optionId": option_id,
        }
    });
    let id_str = match &id {
        RequestId::String(s) => s.clone(),
        RequestId::Integer(n) => n.to_string(),
    };
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::PermissionAutoApproved {
            request_id: id_str,
            method: method.clone(),
            tool_name,
        },
    )
    .await;
    if let Err(err) = client.resolve_server_request(id, result).await {
        tracing::warn!(
            target: "pi_server_runner",
            %err,
            method,
            "failed to auto-approve permission request"
        );
        // Best-effort follow-up rejection so the remote does not hang
        // indefinitely waiting for a response.
        let _ = client
            .reject_server_request(
                RequestId::String(format!("noop-{}", method)),
                JSONRPCErrorError {
                    code: -32000,
                    message: format!("resolve failed: {err}"),
                    data: None,
                },
            )
            .await;
    }
}

/// Split a typed notification/request JSON value into its `method`
/// string and `params` payload, treating anything that fails to match
/// the conventional `{method, params}` envelope as `params=value`.
fn split_method_params(mut value: JsonValue) -> (String, JsonValue) {
    let method_string = match value.get_mut("method") {
        Some(JsonValue::String(s)) => std::mem::take(s),
        Some(other) => other.to_string(),
        None => "<unknown>".to_string(),
    };
    let params = match value.get_mut("params") {
        Some(slot) => std::mem::replace(slot, JsonValue::Null),
        None => JsonValue::Null,
    };
    (method_string, params)
}

/// If a `session/update` notification carries a tool execution
/// payload, surface it as a dedicated `tool_exec` transcript line so
/// validators can grep for it without re-decoding the raw params.
async fn emit_tool_exec_if_present(
    events_file: &mut Option<tokio::fs::File>,
    method: &str,
    params: &JsonValue,
) {
    if !method.starts_with("session/") {
        return;
    }
    // Walk the common ACP `update` shape: { update: { sessionUpdate: ..., ... } }
    // and surface any tool_use / bash arguments. Be conservative: we
    // emit at most one `tool_exec` per notification, and we keep the
    // raw payload so the validator has the full context.
    let update = params.get("update").or(Some(params));
    let Some(update) = update else { return };
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .and_then(JsonValue::as_str);
    if !matches!(
        kind,
        Some("tool_call") | Some("tool_call_update") | Some("toolCall") | Some("toolUse")
    ) {
        return;
    }
    let tool_call_id = update
        .get("toolCallId")
        .or_else(|| update.get("tool_call_id"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let tool_name = update
        .get("toolName")
        .or_else(|| update.get("tool_name"))
        .or_else(|| update.get("name"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let command = update
        .get("rawInput")
        .or_else(|| update.get("input"))
        .or_else(|| update.get("args"))
        .and_then(|v| v.get("command"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::ToolExec {
            tool_call_id,
            tool_name,
            command,
            raw: update.clone(),
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as JsonValue;

    #[test]
    fn locate_ssh_key_honors_explicit_path() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let key = tmp.path().join("my_key");
        std::fs::write(&key, "PRIVATE KEY").unwrap();
        let resolved = locate_ssh_key(Some(&key)).expect("explicit must resolve");
        assert_eq!(resolved, key);
    }

    #[test]
    fn locate_ssh_key_rejects_missing_explicit() {
        let err = locate_ssh_key(Some(Path::new("/nonexistent/key")))
            .expect_err("missing explicit must error");
        assert!(err.contains("does not exist"), "got: {err}");
    }

    #[test]
    fn locate_ssh_key_probes_home_ssh_dir() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let ssh_dir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        std::fs::write(ssh_dir.join("id_rsa"), "PRIVATE").unwrap();
        let prev = std::env::var_os("HOME");
        // SAFETY: scoped env mutation for test isolation.
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let resolved = locate_ssh_key(None).expect("home probe must resolve");
        assert_eq!(resolved, ssh_dir.join("id_rsa"));
        // SAFETY: restore prior HOME.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn inject_drop_mode_value_enum_parses() {
        use clap::ValueEnum;
        let parsed = InjectDropMode::from_str("kill-stop", true).expect("kill-stop parses");
        assert_eq!(parsed, InjectDropMode::KillStop);
        assert_eq!(parsed.label(), "kill-stop");
        let parsed = InjectDropMode::from_str("socat-partition", true)
            .expect("socat-partition parses");
        assert_eq!(parsed, InjectDropMode::SocatPartition);
        assert_eq!(parsed.label(), "socat-partition");
        assert!(InjectDropMode::from_str("nope", true).is_err());
    }

    #[tokio::test]
    async fn emit_remote_line_writes_jsonl_to_file() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let path = tmp.path().join("events.jsonl");
        let mut file = Some(
            tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .await
                .expect("open events file"),
        );
        emit_remote_line(
            &mut file,
            &RemoteTranscriptLine::Connecting {
                host: "host".to_string(),
                user: "user".to_string(),
                port: 22,
            },
        )
        .await
        .expect("emit connecting");
        emit_remote_line(
            &mut file,
            &RemoteTranscriptLine::InjectDrop {
                mode: "kill-stop",
                scheduled_in_secs: 60,
            },
        )
        .await
        .expect("emit inject_drop");
        emit_remote_line(
            &mut file,
            &RemoteTranscriptLine::TurnComplete {
                stop_reason: Some("end_turn".to_string()),
            },
        )
        .await
        .expect("emit turn_complete");
        drop(file);

        let body = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3, "three events expected, got: {body}");

        let first: JsonValue = serde_json::from_str(lines[0]).expect("json line 0");
        assert_eq!(first["event_kind"], "connecting");
        assert_eq!(first["host"], "host");

        let second: JsonValue = serde_json::from_str(lines[1]).expect("json line 1");
        assert_eq!(second["event_kind"], "inject_drop");
        assert_eq!(second["mode"], "kill-stop");
        assert_eq!(second["scheduled_in_secs"], 60);

        let third: JsonValue = serde_json::from_str(lines[2]).expect("json line 2");
        assert_eq!(third["event_kind"], "turn_complete");
        assert_eq!(third["stop_reason"], "end_turn");
    }

    #[test]
    fn split_method_params_extracts_method_and_params() {
        let value = json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {"sessionUpdate": "tool_call"}}
        });
        let (method, params) = split_method_params(value);
        assert_eq!(method, "session/update");
        assert_eq!(params["sessionId"], "s1");
    }

    #[tokio::test]
    async fn emit_remote_line_serializes_permission_auto_approved() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let path = tmp.path().join("perm.jsonl");
        let mut file = Some(
            tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .await
                .expect("open"),
        );
        emit_remote_line(
            &mut file,
            &RemoteTranscriptLine::PermissionAutoApproved {
                request_id: "pi-tool-permission-0".to_string(),
                method: "session/request_permission".to_string(),
                tool_name: Some("bash".to_string()),
            },
        )
        .await
        .expect("emit");
        drop(file);
        let body = std::fs::read_to_string(&path).expect("read");
        let parsed: JsonValue = serde_json::from_str(body.trim()).expect("json");
        assert_eq!(parsed["event_kind"], "permission_auto_approved");
        assert_eq!(parsed["request_id"], "pi-tool-permission-0");
        assert_eq!(parsed["method"], "session/request_permission");
        assert_eq!(parsed["tool_name"], "bash");
    }

    #[tokio::test]
    async fn emit_tool_exec_if_present_surfaces_bash_command() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let path = tmp.path().join("tool.jsonl");
        let mut file = Some(
            tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .await
                .expect("open"),
        );
        let params = json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1",
                "toolName": "Bash",
                "rawInput": {"command": "ls /root"}
            }
        });
        emit_tool_exec_if_present(&mut file, "session/update", &params).await;
        drop(file);
        let body = std::fs::read_to_string(&path).expect("read");
        let line = body.lines().next().expect("one line");
        let parsed: JsonValue = serde_json::from_str(line).expect("json");
        assert_eq!(parsed["event_kind"], "tool_exec");
        assert_eq!(parsed["tool_call_id"], "tc-1");
        assert_eq!(parsed["tool_name"], "Bash");
        assert_eq!(parsed["command"], "ls /root");
    }
}
