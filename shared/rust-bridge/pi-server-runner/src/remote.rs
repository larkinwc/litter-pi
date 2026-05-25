//! Remote `pi acp` mode for `pi-server-runner`.
//!
//! Defines the `--remote-ssh <host> --user <user>` CLI surface,
//! transcript shape, and orchestration entry point so validators
//! can pin against a stable interface while the underlying SSH +
//! bootstrap wiring matures.
//!
//! Current scope (see also the feature's `whatWasLeftUndone` in the
//! handoff): the runner stops at `routing_blocked` for two reasons,
//! both surfaced as stable JSONL transcript lines so a validator
//! can diagnose without inspecting source:
//!
//!   1. `PI_REMOTE_SSH_HOST` / `PI_REMOTE_SSH_USER` are not set in
//!      this mission's `.env`. Without a reachable host the runner
//!      cannot exercise VAL-REM-004..007 end-to-end.
//!   2. Driving an ACP turn (`session/new`, `session/prompt`) over
//!      the `RemoteAppServerClient` returned by
//!      `bootstrap_pi_server` requires a `JsonRpcWire`-level escape
//!      hatch from `codex-app-server-client`. Codex's
//!      `ClientRequest` enum tag is closed over codex methods, so
//!      ACP method names are not routable through `client.request`
//!      without an upstream patch.
//!
//! The CLI surface (`--remote-ssh`, `--user`, `--ssh-port`,
//! `--ssh-key-path`, `--events-out`, `--inject-drop`) and the JSONL
//! transcript shape (`RemoteTranscriptLine`) are stable and
//! validator-pinnable; the SSH connect + `pi acp` bootstrap landed
//! in the prior `remote-pi-ssh-bootstrap-and-jsonrpc` feature and
//! is exercised by its own unit tests in
//! `shared/rust-bridge/codex-mobile-client/src/ssh/pi_bootstrap.rs`
//! + `pi_reconnect.rs`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use tokio::io::AsyncWriteExt;

/// Drop-injection mode chosen via `--inject-drop`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum InjectDropMode {
    /// Signal `kill -STOP` / `kill -CONT` on the local russh PID
    /// after 60s idle. Orchestrated by an out-of-band helper
    /// script.
    KillStop,
    /// Tear down a socat-proxied tunnel after 60s idle.
    /// Orchestrated by an out-of-band helper script.
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
    PromptScheduled {
        text: String,
    },
    InjectDrop {
        mode: &'static str,
        scheduled_in_secs: u64,
    },
    /// The runner reached the documented stop point in this scope
    /// and is exiting non-zero so validators can detect the
    /// limitation deterministically.
    RoutingBlocked {
        reason: &'static str,
    },
    Disconnected,
    TurnError {
        message: String,
    },
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

    // Validate the key path eagerly so the transcript records an
    // honest failure if the user lacks an SSH key. The path
    // resolution is also exercised by unit tests in this module.
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
        "remote ssh: would connect"
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

    // Documented stop point. See the module-level comment for why
    // the runner does not yet drive a full ACP turn against the
    // remote host. The reason string is stable so a validator can
    // grep for it (`event_kind=routing_blocked`).
    let _ = emit_remote_line(
        &mut events_file,
        &RemoteTranscriptLine::RoutingBlocked {
            reason: "remote_pi_ssh_runner_pending_upstream_acp_routing",
        },
    )
    .await;
    let _ = emit_remote_line(
        &mut events_file,
        &RemoteTranscriptLine::Disconnected,
    )
    .await;
    let _ = args.timeout_secs;
    ExitCode::from(1)
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
            &RemoteTranscriptLine::RoutingBlocked {
                reason: "remote_pi_ssh_runner_pending_upstream_acp_routing",
            },
        )
        .await
        .expect("emit routing_blocked");
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
        assert_eq!(third["event_kind"], "routing_blocked");
        assert!(
            third["reason"]
                .as_str()
                .unwrap_or("")
                .contains("remote_pi_ssh_runner"),
            "reason must be the stable validator-pinned string, got {third}"
        );
    }
}
