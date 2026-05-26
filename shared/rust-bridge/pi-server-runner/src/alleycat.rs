//! Alleycat-pair mode for `pi-server-runner`.
//!
//! `--alleycat-pair <payload>` reads the JSON pair payload (from the
//! literal flag value or `$PI_ALLEYCAT_PAIR_PAYLOAD`), connects to
//! the alleycat host over iroh QUIC, lists the advertised agents,
//! builds a litter-side [`LitterAlleycatManifest`], caches it to
//! disk (default `artifacts/val-rem/008-alleycat-manifest.json`),
//! and drives a single `session/new` + `session/prompt` ACP turn
//! against the alleycat-discovered pi entry.
//!
//! The transcript shape is intentionally identical to the SSH path
//! (`RemoteTranscriptLine`) so VAL-REM-010 can `diff` ordered
//! `event_kind` lists between the two paths.
//!
//! An optional `--serve-manifest <ADDR>` flag spawns a one-shot HTTP
//! server that returns the cached manifest body verbatim. Validators
//! `curl` against it to verify VAL-REM-011 (HTTP/1.1 200 + matching
//! sha256). The listener is process-scoped — it dies with the
//! runner — so it does not count as a new long-lived service.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use codex_app_server_client::{AppServerClient, AppServerEvent, RemoteAppServerClient};
use codex_app_server_protocol::{JSONRPCErrorError, RequestId};
use codex_mobile_client::alleycat::{
    AgentInfo, agent_runtime_kind, bind_alleycat_endpoint, build_litter_manifest,
    connect_app_server_client, list_agents, parse_pair_payload,
};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::remote::{RemoteTranscriptLine, emit_remote_line};

/// CLI-derived configuration for `--alleycat-pair`.
#[derive(Debug, Clone)]
pub struct AlleycatPairArgs {
    /// Raw pair payload JSON (the literal value of `--alleycat-pair`
    /// or `$PI_ALLEYCAT_PAIR_PAYLOAD`).
    pub payload: String,
    /// Prompt to drive against the alleycat-discovered pi entry.
    pub prompt: String,
    /// Where to write the JSONL transcript (in addition to stdout).
    pub events_out: Option<PathBuf>,
    /// Where to cache the resolved [`LitterAlleycatManifest`] JSON.
    /// Defaults to `artifacts/val-rem/008-alleycat-manifest.json`.
    pub manifest_out: Option<PathBuf>,
    /// Optional one-shot HTTP listener for VAL-REM-011. When set, the
    /// runner binds the address, advertises it, and serves the cached
    /// manifest body verbatim. The listener stops once `serve_max_hits`
    /// requests have been answered (or when the runner exits).
    pub serve_manifest: Option<SocketAddr>,
    /// How many `curl` requests the optional manifest server should
    /// answer before shutting down. Default 1.
    pub serve_max_hits: u32,
    /// Skip the actual turn (`thread/start` + `turn/start`) and just
    /// cache the manifest + optionally serve it. Useful when the
    /// alleycat-side pi process is unhealthy but VAL-REM-008/009/011
    /// still need their evidence.
    pub skip_turn: bool,
    /// Wall-clock budget for the turn.
    pub timeout_secs: u64,
}

/// Default path the cached manifest lands at when `--manifest-out`
/// is not provided. Pinned to the VAL-REM-008 evidence path so
/// validators can `jq` it without further wiring.
pub const DEFAULT_MANIFEST_OUT: &str = "artifacts/val-rem/008-alleycat-manifest.json";

/// Top-level entrypoint for `--alleycat-pair`.
pub async fn drive_alleycat_pair(args: AlleycatPairArgs) -> ExitCode {
    let mut events_file = match open_events_file(args.events_out.as_deref()).await {
        Ok(file) => file,
        Err(err) => {
            eprintln!("pi-server-runner: opening --events-out failed: {err}");
            return ExitCode::from(2);
        }
    };

    // Parse the pair payload. Anything malformed exits 2 so validators
    // get a clear stderr message without a half-built transcript.
    let params = match parse_pair_payload(args.payload.trim()) {
        Ok(p) => p,
        Err(err) => {
            let msg = format!("pair payload parse: {err}");
            let _ = emit_remote_line(
                &mut events_file,
                &RemoteTranscriptLine::TurnError { message: msg.clone() },
            )
            .await;
            eprintln!("pi-server-runner: {msg}");
            return ExitCode::from(2);
        }
    };
    let host_label = params
        .host_name
        .clone()
        .unwrap_or_else(|| params.node_id.clone());

    let _ = emit_remote_line(
        &mut events_file,
        &RemoteTranscriptLine::Connecting {
            host: host_label.clone(),
            user: "alleycat".to_string(),
            port: 0,
        },
    )
    .await;
    let _ = emit_remote_line(
        &mut events_file,
        &RemoteTranscriptLine::PromptScheduled { text: args.prompt.clone() },
    )
    .await;

    // Bind a fresh iroh endpoint for this one-shot run. The runner
    // never persists a device key — each invocation generates a new
    // one. That's fine for a developer/CI helper.
    let endpoint = match bind_alleycat_endpoint(None).await {
        Ok(ep) => ep,
        Err(err) => {
            return fail(&mut events_file, format!("alleycat bind: {err}")).await;
        }
    };

    let agents = match list_agents(&endpoint, params.clone()).await {
        Ok(list) => list,
        Err(err) => {
            return fail(&mut events_file, format!("alleycat list_agents: {err}")).await;
        }
    };
    let manifest = build_litter_manifest(&params, &agents);
    let manifest_path = args
        .manifest_out
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST_OUT));
    let manifest_body = match serde_json::to_vec_pretty(&manifest) {
        Ok(bytes) => bytes,
        Err(err) => {
            return fail(
                &mut events_file,
                format!("manifest serialize: {err}"),
            )
            .await;
        }
    };
    if let Err(err) = write_manifest(&manifest_path, &manifest_body).await {
        return fail(
            &mut events_file,
            format!("manifest write {}: {err}", manifest_path.display()),
        )
        .await;
    }
    let manifest_sha = sha256_hex(&manifest_body);
    tracing::info!(
        target: "pi_server_runner",
        path = %manifest_path.display(),
        sha256 = %manifest_sha,
        "alleycat manifest cached",
    );
    let _ = emit_remote_line(
        &mut events_file,
        &RemoteTranscriptLine::Connected { host: host_label.clone() },
    )
    .await;

    // Optional manifest server for VAL-REM-011. The body is shared
    // immutably with the spawned task so concurrent curl hits read the
    // exact bytes we wrote to disk.
    let manifest_arc: Arc<Vec<u8>> = Arc::new(manifest_body.clone());
    let manifest_server = if let Some(addr) = args.serve_manifest {
        match spawn_manifest_server(addr, Arc::clone(&manifest_arc), args.serve_max_hits).await {
            Ok(state) => Some(state),
            Err(err) => {
                return fail(
                    &mut events_file,
                    format!("manifest serve bind {addr}: {err}"),
                )
                .await;
            }
        }
    } else {
        None
    };

    if args.skip_turn {
        tracing::info!(
            target: "pi_server_runner",
            "alleycat --skip-turn: manifest cached, leaving the listener up for VAL-REM-011"
        );
        // When `--serve-manifest` is also set, wait for the listener
        // task to finish (i.e. it served `serve_max_hits` requests).
        if let Some(server) = manifest_server {
            server.wait_until_done().await;
        }
        let _ = emit_remote_line(
            &mut events_file,
            &RemoteTranscriptLine::Disconnected,
        )
        .await;
        return ExitCode::SUCCESS;
    }

    // Find the pi entry the validator wants to drive. If the host
    // does not advertise pi we cannot satisfy VAL-REM-010 — exit 2.
    let pi_entry = match find_pi_entry(&agents) {
        Some(e) => e,
        None => {
            return fail(
                &mut events_file,
                "alleycat manifest does not advertise a pi agent".to_string(),
            )
            .await;
        }
    };

    // Bring up the alleycat-side ACP client and drive a single turn
    // via the shared `drive_turn` helper from the SSH path so the
    // emitted event_kind ordering matches.
    let (client, _session) = match connect_app_server_client(
        &endpoint,
        params.clone(),
        pi_entry.name.clone(),
        pi_entry.wire,
        None,
        None,
    )
    .await
    {
        Ok(pair) => pair,
        Err(err) => {
            return fail(&mut events_file, format!("alleycat connect pi: {err}")).await;
        }
    };
    let mut remote_client: RemoteAppServerClient = match client {
        AppServerClient::Remote(remote) => remote,
        AppServerClient::InProcess(_) => {
            return fail(
                &mut events_file,
                "alleycat returned in-process client; expected remote".to_string(),
            )
            .await;
        }
    };

    let turn_timeout = Duration::from_secs(args.timeout_secs);
    let outcome = tokio::time::timeout(
        turn_timeout,
        drive_alleycat_turn(&mut remote_client, &args.prompt, &mut events_file),
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
                "alleycat turn timed out after {}s without turn_complete",
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

    let _ = remote_client.shutdown().await;
    let _ = emit_remote_line(&mut events_file, &RemoteTranscriptLine::Disconnected).await;

    if let Some(server) = manifest_server {
        server.shutdown().await;
    }

    exit
}

/// Run one alleycat-path turn against pi via the codex `thread/start` +
/// `turn/start` JSON-RPC shape that the alleycat-pi-bridge actually
/// implements. The emitted [`RemoteTranscriptLine`] sequence is
/// intentionally identical to the SSH-path `drive_turn` shape so
/// VAL-REM-010 can diff the two `event_kind` orderings.
async fn drive_alleycat_turn(
    client: &mut RemoteAppServerClient,
    prompt: &str,
    events_file: &mut Option<tokio::fs::File>,
) -> Result<(), String> {
    // thread/start mirrors session/new on the ACP path: it returns
    // the freshly-minted thread id we attach the subsequent turn to.
    let thread_params = json!({
        "cwd": "/tmp",
    });
    let thread_result = client
        .send_raw_request("thread/start", Some(thread_params))
        .await
        .map_err(|e| format!("thread/start transport: {e}"))?
        .map_err(|e| format!("thread/start server: {} ({})", e.message, e.code))?;
    // alleycat-pi-bridge returns the thread under `result.thread.id`
    // (matching its full Thread record shape); older codex-style
    // bridges put it at `result.threadId`. Probe both so this works
    // against either shape.
    let thread_id = thread_result
        .get("thread")
        .and_then(|v| v.get("id"))
        .and_then(JsonValue::as_str)
        .or_else(|| thread_result.get("threadId").and_then(JsonValue::as_str))
        .map(str::to_string);
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::SessionStarted {
            session_id: thread_id.clone(),
        },
    )
    .await;
    let thread_id = thread_id.ok_or_else(|| {
        "thread/start succeeded but the result is missing a threadId string".to_string()
    })?;

    // turn/start streams notifications until the turn completes.
    let turn_params = json!({
        "threadId": thread_id,
        "input": [
            {"type": "text", "text": prompt}
        ],
    });
    let turn_future = client.send_raw_request("turn/start", Some(turn_params));
    tokio::pin!(turn_future);
    let mut turn_result: Option<JsonValue> = None;
    let mut turn_completed_params: Option<JsonValue> = None;
    // alleycat-pi-bridge acknowledges `turn/start` immediately with an
    // `inProgress` turn record, then streams `turn/started` /
    // `item/started` / `item/completed` notifications and finally a
    // `turn/completed` notification when the agent is done. We have
    // to keep draining events past the request ack until that
    // terminal notification arrives so VAL-REM-010 can observe the
    // full event ordering.
    loop {
        tokio::select! {
            biased;
            res = &mut turn_future, if turn_result.is_none() => {
                let result = res
                    .map_err(|e| format!("turn/start transport: {e}"))?
                    .map_err(|e| format!("turn/start server: {} ({})", e.message, e.code))?;
                turn_result = Some(result);
            }
            event = client.next_event() => {
                let Some(event) = event else { break; };
                if let Some(params) = turn_completed_from_event(&event) {
                    handle_alleycat_event(client, event, events_file).await?;
                    turn_completed_params = Some(params);
                    break;
                }
                handle_alleycat_event(client, event, events_file).await?;
            }
        }
    }
    // Drain any trailing notifications that arrive in the small window
    // after `turn/completed` (e.g. `thread/tokenUsage/updated`).
    loop {
        let event = match tokio::time::timeout(
            Duration::from_millis(100),
            client.next_event(),
        )
        .await
        {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(_) => break,
        };
        handle_alleycat_event(client, event, events_file).await?;
    }

    let turn_result = turn_result.unwrap_or(JsonValue::Null);
    let stop_reason = turn_completed_params
        .as_ref()
        .and_then(|p| p.get("turn"))
        .and_then(|t| t.get("stopReason").or_else(|| t.get("status")))
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .or_else(|| {
            turn_result
                .get("stopReason")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        });
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::PromptResult { result: turn_result.clone() },
    )
    .await;
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::TurnComplete { stop_reason },
    )
    .await;
    Ok(())
}

/// Detect the `turn/completed` notification — the canonical terminal
/// event on the alleycat-pi-bridge turn lifecycle. Returns the
/// notification params when present so callers can extract
/// `stopReason` / `status`.
fn turn_completed_from_event(event: &AppServerEvent) -> Option<JsonValue> {
    match event {
        AppServerEvent::ServerNotification(notification) => {
            let value = serde_json::to_value(notification).ok()?;
            let method = value.get("method").and_then(JsonValue::as_str)?;
            if method == "turn/completed" {
                Some(value.get("params").cloned().unwrap_or(JsonValue::Null))
            } else {
                None
            }
        }
        AppServerEvent::RawServerNotification { method, params } => {
            if method == "turn/completed" {
                Some(params.clone().unwrap_or(JsonValue::Null))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Translate a raw alleycat `AppServerEvent` into the canonical
/// `RemoteTranscriptLine` shape used by the SSH path. Tool execution
/// surface (alleycat's `item/started` / `item/completed` carrying
/// `kind == "execution_started"`) maps to `tool_exec`; everything
/// else falls back to `server_notification` (or
/// `permission_auto_approved` for the inbound permission requests).
async fn handle_alleycat_event(
    client: &RemoteAppServerClient,
    event: AppServerEvent,
    events_file: &mut Option<tokio::fs::File>,
) -> Result<(), String> {
    match event {
        AppServerEvent::ServerNotification(notification) => {
            let value = serde_json::to_value(&notification).unwrap_or(JsonValue::Null);
            let (method, params) = split_method_params(value);
            emit_alleycat_tool_exec_if_present(events_file, &method, &params).await;
            let _ = emit_remote_line(
                events_file,
                &RemoteTranscriptLine::ServerNotification { method, params },
            )
            .await;
        }
        AppServerEvent::Disconnected { message } => {
            return Err(format!("alleycat pi disconnected: {message}"));
        }
        AppServerEvent::ServerRequest(request) => {
            // alleycat-pi-bridge surfaces tool-exec approval as a
            // typed ServerRequest (e.g.
            // `item/commandExecution/requestApproval`). The codex
            // AppServerClient routes typed permission requests through
            // here; auto-approve them so the bridge can resume the
            // turn instead of hanging on the operator confirmation.
            let value = serde_json::to_value(&request).unwrap_or(JsonValue::Null);
            let id = value
                .get("id")
                .and_then(|v| {
                    if let Some(s) = v.as_str() {
                        Some(RequestId::String(s.to_string()))
                    } else {
                        v.as_i64().map(RequestId::Integer)
                    }
                })
                .unwrap_or_else(|| RequestId::String(String::new()));
            let (method, params) = split_method_params(value);
            // Skip the `ServerRequest` transcript line: the SSH path
            // does not surface a server_request kind, only the
            // resulting `permission_auto_approved`. VAL-REM-010
            // normalises on event_kind ordering, so we keep the
            // alleycat transcript aligned by emitting only the auto-
            // approve line.
            auto_approve_alleycat_permission(client, id, method, Some(params), events_file)
                .await;
        }
        AppServerEvent::RawServerRequest { id, method, params } => {
            auto_approve_alleycat_permission(client, id, method, params, events_file).await;
        }
        AppServerEvent::RawServerNotification { method, params } => {
            let params = params.unwrap_or(JsonValue::Null);
            emit_alleycat_tool_exec_if_present(events_file, &method, &params).await;
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
                "ignoring alleycat app-server event"
            );
        }
    }
    Ok(())
}

/// Surface any alleycat tool-exec notification (`item/started` or
/// `item/completed` carrying `kind == "execution_started"` /
/// `"execution_completed"`) as a dedicated `tool_exec` transcript
/// line. Mirrors the SSH-path helper closely so VAL-REM-010's diff
/// stays clean.
async fn emit_alleycat_tool_exec_if_present(
    events_file: &mut Option<tokio::fs::File>,
    method: &str,
    params: &JsonValue,
) {
    if method != "item/started" && method != "item/completed" {
        return;
    }
    let item = params
        .get("item")
        .or_else(|| params.get("itemUpdate"))
        .unwrap_or(params);
    // alleycat-pi-bridge surfaces tool execution via item objects
    // with `type == "commandExecution"`. Older codex shapes used the
    // string field `kind == "execution_started"` etc — both are
    // accepted so a single helper handles both transports.
    let kind = item
        .get("type")
        .and_then(JsonValue::as_str)
        .or_else(|| item.get("kind").and_then(JsonValue::as_str));
    if !matches!(
        kind,
        Some("execution_started")
            | Some("execution_completed")
            | Some("command_exec")
            | Some("tool_exec")
            | Some("commandExecution")
    ) {
        return;
    }
    let tool_call_id = item
        .get("id")
        .or_else(|| item.get("itemId"))
        .or_else(|| item.get("commandExecId"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let tool_name = item
        .get("toolName")
        .or_else(|| item.get("tool_name"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let command = item
        .get("command")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .or_else(|| {
            item.get("input")
                .and_then(|v| v.get("command"))
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        });
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::ToolExec {
            tool_call_id,
            tool_name,
            command,
            raw: item.clone(),
        },
    )
    .await;
}

async fn auto_approve_alleycat_permission(
    client: &RemoteAppServerClient,
    id: RequestId,
    method: String,
    params: Option<JsonValue>,
    events_file: &mut Option<tokio::fs::File>,
) {
    let params_val = params.unwrap_or(JsonValue::Null);
    let tool_name = params_val
        .get("toolName")
        .or_else(|| params_val.get("command"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let decision = json!({"decision": "approved"});
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
    if let Err(err) = client.resolve_server_request(id, decision).await {
        tracing::warn!(
            target: "pi_server_runner",
            %err,
            method,
            "failed to auto-approve alleycat permission request"
        );
        let _ = client
            .reject_server_request(
                RequestId::String(format!("noop-{method}")),
                JSONRPCErrorError {
                    code: -32000,
                    message: format!("resolve failed: {err}"),
                    data: None,
                },
            )
            .await;
    }
}

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

fn find_pi_entry(agents: &[AgentInfo]) -> Option<&AgentInfo> {
    agents.iter().find(|agent| {
        agent_runtime_kind(&agent.name, &agent.display_name).as_deref() == Some("pi")
    })
}

async fn open_events_file(
    path: Option<&Path>,
) -> std::io::Result<Option<tokio::fs::File>> {
    let Some(path) = path else { return Ok(None) };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let f = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await?;
    Ok(Some(f))
}

async fn write_manifest(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, body).await
}

fn sha256_hex(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

async fn fail(
    events_file: &mut Option<tokio::fs::File>,
    msg: String,
) -> ExitCode {
    let _ = emit_remote_line(
        events_file,
        &RemoteTranscriptLine::TurnError { message: msg.clone() },
    )
    .await;
    eprintln!("pi-server-runner: {msg}");
    ExitCode::from(2)
}

/// One-shot HTTP server for VAL-REM-011. Returns once the listener
/// is bound so the caller can advertise the resolved socket address.
struct ManifestServerHandle {
    join: tokio::task::JoinHandle<()>,
    cancel: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    // Mainly useful in tests that bind `127.0.0.1:0` and need to
    // discover the kernel-assigned port. Production callers read
    // the advertised addr from the stdout `manifest_serving` event.
    #[allow(dead_code)]
    advertised: SocketAddr,
}

impl ManifestServerHandle {
    /// Wait for the listener task to exit on its own (i.e. after
    /// `max_hits` requests have been served).
    async fn wait_until_done(self) {
        let _ = self.join.await;
    }

    async fn shutdown(self) {
        let mut guard = self.cancel.lock().await;
        if let Some(tx) = guard.take() {
            let _ = tx.send(());
        }
        drop(guard);
        // Best-effort wait for the listener task to exit.
        let _ = self.join.await;
    }
}

async fn spawn_manifest_server(
    addr: SocketAddr,
    body: Arc<Vec<u8>>,
    max_hits: u32,
) -> std::io::Result<ManifestServerHandle> {
    let listener = TcpListener::bind(addr).await?;
    let advertised = listener.local_addr()?;
    println!(
        "{{\"event_kind\":\"manifest_serving\",\"addr\":\"{advertised}\",\"max_hits\":{max_hits}}}"
    );
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let cancel = Arc::new(Mutex::new(Some(cancel_tx)));
    let join = tokio::spawn(async move {
        run_manifest_server(listener, body, max_hits, cancel_rx).await;
    });
    Ok(ManifestServerHandle {
        join,
        cancel,
        advertised,
    })
}

async fn run_manifest_server(
    listener: TcpListener,
    body: Arc<Vec<u8>>,
    max_hits: u32,
    mut cancel: tokio::sync::oneshot::Receiver<()>,
) {
    let mut remaining = max_hits;
    loop {
        tokio::select! {
            biased;
            _ = &mut cancel => return,
            accepted = listener.accept() => {
                match accepted {
                    Ok((mut stream, _peer)) => {
                        let body = Arc::clone(&body);
                        // Read the request line + headers (best-effort
                        // — we only need to consume the request before
                        // writing the response).
                        let mut buf = [0u8; 1024];
                        let _ = tokio::time::timeout(
                            Duration::from_secs(5),
                            stream.read(&mut buf),
                        )
                        .await;
                        let response = build_http_200(&body);
                        if let Err(err) = stream.write_all(&response).await {
                            tracing::warn!(target: "pi_server_runner", %err, "manifest server write failed");
                        }
                        let _ = stream.shutdown().await;
                        if max_hits > 0 {
                            remaining = remaining.saturating_sub(1);
                            if remaining == 0 {
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(target: "pi_server_runner", %err, "manifest server accept failed");
                        return;
                    }
                }
            }
        }
    }
}

fn build_http_200(body: &[u8]) -> Vec<u8> {
    let mut response = Vec::with_capacity(body.len() + 128);
    response.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    response.extend_from_slice(b"Content-Type: application/json\r\n");
    response.extend_from_slice(
        format!("Content-Length: {}\r\n", body.len()).as_bytes(),
    );
    response.extend_from_slice(b"Connection: close\r\n");
    response.extend_from_slice(b"\r\n");
    response.extend_from_slice(body);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_mobile_client::alleycat::{AgentWire, AlleycatError};

    #[test]
    fn sha256_hex_matches_known_value() {
        // Known vector: sha256("hello world") in lowercase hex.
        let h = sha256_hex(b"hello world");
        assert_eq!(
            h,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn build_http_200_includes_content_length_and_body() {
        let body = b"{\"x\":1}";
        let response = build_http_200(body);
        let text = std::str::from_utf8(&response).expect("utf8");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 7\r\n"));
        assert!(text.ends_with("{\"x\":1}"));
    }

    #[tokio::test]
    async fn manifest_server_serves_body_and_shuts_down_after_max_hits() {
        let body = Arc::new(b"{\"manifest\":true}".to_vec());
        let server = spawn_manifest_server(
            "127.0.0.1:0".parse().unwrap(),
            Arc::clone(&body),
            1,
        )
        .await
        .expect("bind manifest server");
        let addr = server.advertised;

        // One client hit — should return the body.
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /manifest HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write request");
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            stream.read_to_end(&mut buf),
        )
        .await;
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("HTTP/1.1 200 OK"), "got: {text}");
        assert!(
            text.contains("{\"manifest\":true}"),
            "body missing in response: {text}"
        );

        // The server task should exit on its own once max_hits is hit.
        // Wait briefly via shutdown helper to avoid a hung test.
        server.shutdown().await;
    }

    #[test]
    fn find_pi_entry_picks_pi_runtime_kind() {
        let agents = vec![
            AgentInfo {
                name: "codex".into(),
                display_name: "Codex".into(),
                wire: AgentWire::Websocket,
                available: true,
                presentation: None,
                capabilities: None,
            },
            AgentInfo {
                name: "pi".into(),
                display_name: "Pi".into(),
                wire: AgentWire::Jsonl,
                available: true,
                presentation: None,
                capabilities: None,
            },
        ];
        let entry = find_pi_entry(&agents).expect("pi entry");
        assert_eq!(entry.name, "pi");
    }

    #[test]
    fn find_pi_entry_returns_none_when_absent() {
        let agents = vec![AgentInfo {
            name: "codex".into(),
            display_name: "Codex".into(),
            wire: AgentWire::Websocket,
            available: true,
            presentation: None,
            capabilities: None,
        }];
        assert!(find_pi_entry(&agents).is_none());
    }

    // `_` AlleycatError import keeps the module honest about which
    // crates this driver reaches into without dragging in a real
    // network test here.
    #[allow(dead_code)]
    fn _alleycat_error_type_is_re_exported(_e: AlleycatError) {}

    #[tokio::test]
    async fn tool_exec_emitter_recognises_command_execution_type() {
        // alleycat-pi-bridge surfaces tool execution via
        // `item/started` notifications whose item carries
        // `type == "commandExecution"`. The emitter must surface a
        // `tool_exec` transcript line for that shape so VAL-REM-010
        // can diff the alleycat path against the SSH path.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let mut events_file = Some(
            tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .await
                .expect("open"),
        );
        let params = json!({
            "item": {
                "type": "commandExecution",
                "command": "ls /root",
                "id": "tool-1",
                "status": "inProgress",
            }
        });
        emit_alleycat_tool_exec_if_present(&mut events_file, "item/started", &params).await;
        drop(events_file);

        let lines = std::fs::read_to_string(&path).expect("read");
        let parsed: serde_json::Value =
            serde_json::from_str(lines.lines().next().expect("line")).expect("json");
        assert_eq!(parsed["event_kind"], "tool_exec");
        assert_eq!(parsed["command"], "ls /root");
        assert_eq!(parsed["tool_call_id"], "tool-1");
    }

    #[test]
    fn turn_completed_from_event_matches_raw_notification() {
        let params = json!({"threadId": "t1", "turn": {"status": "completed"}});
        let event = AppServerEvent::RawServerNotification {
            method: "turn/completed".to_string(),
            params: Some(params.clone()),
        };
        let extracted = turn_completed_from_event(&event).expect("Some");
        assert_eq!(extracted, params);

        let not_terminal = AppServerEvent::RawServerNotification {
            method: "turn/started".to_string(),
            params: None,
        };
        assert!(turn_completed_from_event(&not_terminal).is_none());
    }
}
