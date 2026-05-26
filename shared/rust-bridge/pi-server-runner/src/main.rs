//! Headless host runner for exercising the in-process pi coding-agent
//! runtime end-to-end without booting iOS.
//!
//! Today the runner ships the `--local` mode:
//!
//! * Builds a [`pi_mobile_client::PiSessionConfig`] selecting the
//!   [`PtyDevToolFactory`](pi_mobile_client::ToolFactoryKind::PtyDev)
//!   (macOS host shell — the dev/CI stand-in for iSH on iOS).
//! * Calls [`pi_mobile_client::start_in_process`] with that config so
//!   the same asupersync↔tokio bridge used by `connect_local_pi`
//!   drives the prompt.
//! * Sends `--prompt <text>` (or, with `--stdin`, the contents of
//!   stdin) as a single user turn.
//! * Prints every [`pi_mobile_client::PiEvent`] it observes as a
//!   newline-delimited JSON record on stdout.
//!
//! Exit codes:
//!
//! * `0` — the runtime emitted `PiEvent::TurnComplete` and shut down
//!   cleanly.
//! * `1` — the runtime emitted `PiEvent::TurnError` (auth failure,
//!   provider error, …) or the broadcast channel closed before any
//!   turn-terminal event arrived.
//! * `2` — the CLI/runtime could not start at all (e.g. missing
//!   `--prompt` and `--stdin`, no BYOK key, IO error reading stdin).
//!
//! BYOK env vars consulted before pi loads:
//!
//! * `ANTHROPIC_API_KEY` — selects the Anthropic provider when no
//!   `--provider` is supplied.
//! * `ANTHROPIC_BASE_URL` — optional custom Anthropic-compatible
//!   endpoint (e.g. a BYOK proxy). Forwarded into
//!   `PiSessionConfig.base_url` when the active provider is
//!   `anthropic`.
//! * `OPENAI_API_KEY` + `OPENAI_BASE_URL` — fall back to the OpenAI-
//!   compatible provider. `OPENAI_BASE_URL` is forwarded into
//!   `PiSessionConfig.base_url` when the active provider is
//!   `openai` (or any non-`anthropic` provider). OAuth lands in a
//!   later milestone.
//!
//! Base-URL precedence: the env var matching the *active* provider
//! wins. If both `ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` are set
//! and `--provider anthropic` is used, the Anthropic URL is applied;
//! the OpenAI URL is ignored (and vice versa).

use std::io::{BufRead, IsTerminal, Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Command as ShellCommand, ExitCode, Stdio};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use pi_mobile_client::auth::{
    AnthropicOAuthConfig, AnthropicOAuthDriver, AuthEvent, AuthEventSource, AuthorizeHandshake,
    ClaudeImportConfig, ClaudeImportSummary, complete_anthropic_oauth_paste,
    import_claude_credentials, pi_byok_set_blocking, snapshot_anthropic_oauth,
};
use pi_mobile_client::{
    Command, InProcessStartArgs, PiEvent, PiSessionConfig, ToolFactoryKind, start_in_process,
};
use serde::Serialize;

mod alleycat;
mod remote;

use alleycat::{AlleycatPairArgs, drive_alleycat_pair};
use remote::{InjectDropMode, RemoteSshArgs, drive_remote_ssh};

/// Which built-in tool factory the runner should mount.
///
/// `pty-dev` is the macOS host stand-in for iSH (iOS) and proot
/// (Android). It runs pi's stock `BashTool` directly against the
/// host shell so the in-process agent loop can be smoke-tested on
/// a developer machine without booting a simulator or emulator.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum ToolFactoryArg {
    /// macOS host shell stand-in. Default. Used as the smoke double
    /// for both the iOS iSH factory and the Android proot factory.
    PtyDev,
}

impl ToolFactoryArg {
    /// Human-readable label printed at startup so validators can
    /// confirm which factory was selected.
    fn display_name(self) -> &'static str {
        match self {
            ToolFactoryArg::PtyDev => "pty-dev",
        }
    }

    fn to_kind(self) -> ToolFactoryKind {
        match self {
            ToolFactoryArg::PtyDev => ToolFactoryKind::PtyDev,
        }
    }
}

/// Runner CLI surface.
#[derive(Parser, Debug)]
#[command(
    name = "pi-server-runner",
    version,
    about = "Headless host runner for the in-process pi coding-agent runtime"
)]
struct Cli {
    /// Run an in-process pi session against the host machine.
    #[arg(long)]
    local: bool,

    /// User prompt to send. Mutually exclusive with `--stdin`.
    #[arg(long, value_name = "TEXT")]
    prompt: Option<String>,

    /// Read the prompt from stdin instead of `--prompt`.
    #[arg(long, conflicts_with = "prompt")]
    stdin: bool,

    /// Explicit pi provider id (e.g. `anthropic`, `openai`). Defaults
    /// to a heuristic based on which `*_API_KEY` env var is set.
    #[arg(long)]
    provider: Option<String>,

    /// Explicit pi model id.
    #[arg(long)]
    model: Option<String>,

    /// Maximum wall-clock seconds to wait for the turn to complete.
    /// Defaults to 180s.
    #[arg(long, default_value_t = 180)]
    timeout_secs: u64,

    /// Which built-in tool factory to mount on the in-process pi
    /// runtime. Defaults to `pty-dev` (macOS host shell). Picking
    /// `pty-dev` explicitly mirrors the Android proot/iOS iSH
    /// wiring path: the factory is threaded through the same
    /// `start_in_process` code that the platform builds use, which
    /// lets host-side validators exercise the BashTool plumbing
    /// without booting a simulator or emulator.
    #[arg(long, value_enum, default_value_t = ToolFactoryArg::PtyDev)]
    tool_factory: ToolFactoryArg,

    /// Run the Anthropic OAuth Authorization Code + PKCE handshake
    /// before driving any prompt. The runner prints the authorize URL
    /// to stdout, attempts to open it via `open` (macOS) when a TTY is
    /// attached, then prompts the user to paste the redirect code on
    /// stdin. The exchanged token is persisted to pi's `auth.json`
    /// (configurable via `--auth-path`) so subsequent invocations
    /// without `--oauth-paste` reuse the same credential silently.
    #[arg(long)]
    oauth_paste: bool,

    /// Persist a BYOK API key into pi's `auth.json` before running the
    /// prompt. Combine with `--anthropic-key` for Anthropic BYOK, or
    /// `--openai-key` (+ optional `--openai-base-url`) for an
    /// OpenAI-compatible BYOK profile. The key is written via the
    /// shared `pi_byok_set` helper and reused by future runs without
    /// `--byok`.
    #[arg(long)]
    byok: bool,

    /// BYOK Anthropic API key (requires `--byok`).
    #[arg(long, value_name = "KEY")]
    anthropic_key: Option<String>,

    /// BYOK OpenAI-compatible API key (requires `--byok`).
    #[arg(long, value_name = "KEY")]
    openai_key: Option<String>,

    /// BYOK OpenAI-compatible base URL (requires `--byok`).
    #[arg(long, value_name = "URL")]
    openai_base_url: Option<String>,

    /// Import an Anthropic OAuth credential from a Claude
    /// Code-shaped credentials JSON file into pi's `auth.json`. The
    /// JSON path is read from `--credentials-path <PATH>` or, when
    /// that flag is omitted, from the `CLAUDE_CREDENTIALS_JSON_PATH`
    /// environment variable. The runner exits 0 on success and a
    /// nonzero code with a clear stderr message on parse/IO failure.
    /// Combine with `--auth-path` to redirect the destination away
    /// from the user's installed credentials during validation runs.
    #[arg(long)]
    import_claude_credentials: bool,

    /// Path to the Claude Code credentials JSON file to import. Only
    /// honoured with `--import-claude-credentials`.
    #[arg(long, value_name = "PATH")]
    credentials_path: Option<PathBuf>,

    /// Override the on-disk path of pi's `auth.json`. Defaults to
    /// pi's `Config::auth_path()` (honours `PI_CODING_AGENT_DIR`).
    /// Mostly intended for tests and for redirecting the auth store
    /// during validation runs that should not touch the user's
    /// installed credentials.
    #[arg(long, value_name = "PATH")]
    auth_path: Option<PathBuf>,

    /// Drive a remote `pi acp` session over SSH against `<HOST>`.
    /// Combine with `--user`, optional `--port`, `--prompt`, and
    /// `--events-out` to capture the JSONL transcript. Mutually
    /// exclusive with `--local`; key auth uses the first existing
    /// of `~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, `~/.ssh/id_ecdsa`
    /// (or `--ssh-key-path <PATH>`).
    #[arg(long, value_name = "HOST", conflicts_with = "local")]
    remote_ssh: Option<String>,

    /// Remote login user for `--remote-ssh`. Defaults to `$USER`
    /// when omitted.
    #[arg(long, value_name = "USER", requires = "remote_ssh")]
    user: Option<String>,

    /// Remote SSH port for `--remote-ssh`. Defaults to 22.
    #[arg(long, value_name = "PORT", default_value_t = 22, requires = "remote_ssh")]
    ssh_port: u16,

    /// Override the SSH private-key path used for `--remote-ssh`.
    /// Defaults to probing `~/.ssh/` in canonical order.
    #[arg(long, value_name = "PATH", requires = "remote_ssh")]
    ssh_key_path: Option<PathBuf>,

    /// Append every emitted JSONL transcript line to `<PATH>` in
    /// addition to stdout. Validators consume the file directly
    /// (e.g. `artifacts/val-rem/004-remote-ssh-turn.events.jsonl`).
    #[arg(long, value_name = "PATH")]
    events_out: Option<PathBuf>,

    /// Reconnect-tolerance mode for VAL-REM-006 / VAL-REM-007.
    /// `kill-stop` schedules a `kill -STOP`/`kill -CONT` cycle on
    /// the local russh PID after 60s idle; `socat-partition` tears
    /// down a socat-proxied tunnel. Both modes are orchestrated by
    /// an out-of-band helper script invoked by the validator.
    #[arg(long, value_enum, value_name = "MODE", requires = "remote_ssh")]
    inject_drop: Option<InjectDropMode>,

    /// PID handed to the inject-drop helper script. russh runs
    /// in-process Rust (it has no child PID of its own); for the
    /// validation evidence the orchestrator spawns an out-of-band
    /// `sleep` child and passes its PID here so the helper has a
    /// real PID to `kill -STOP`/`kill -CONT` (kill-stop mode) or
    /// `SIGTERM` (socat-partition mode). When omitted the runner
    /// still emits the contract JSONL triple but skips the actual
    /// helper invocation (useful for smoke-runs).
    #[arg(long, value_name = "PID", requires = "inject_drop")]
    inject_drop_pid: Option<u32>,

    /// Idle wait in seconds before the partition cycle opens.
    /// Defaults to 60s to match the validation contract. Lower in
    /// tests so the runner doesn't spin for a full minute.
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 60,
        requires = "inject_drop"
    )]
    inject_drop_idle_secs: u64,

    /// Pause window in seconds passed to the helper script
    /// (`kill -STOP` pause for kill-stop, partition window for
    /// socat-partition). Defaults to 5s.
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 5,
        requires = "inject_drop"
    )]
    inject_drop_pause_secs: u64,

    /// Override the directory containing the inject-drop helper
    /// scripts. Defaults to `tools/scripts/` relative to the
    /// runner cwd; tests override.
    #[arg(long, value_name = "DIR", requires = "inject_drop")]
    inject_drop_script_dir: Option<PathBuf>,

    /// Drive a turn against the pi entry advertised by an alleycat
    /// host. The flag value is the literal JSON pair payload (the
    /// output of `alleycat pair` on the host). The string `env`
    /// resolves the payload from `$PI_ALLEYCAT_PAIR_PAYLOAD` so the
    /// CI lane can avoid embedding the token in command lines. The
    /// runner pairs, lists agents, caches the litter manifest, and
    /// then drives `--prompt`/`--stdin` against the alleycat-
    /// discovered pi entry. Mutually exclusive with `--local` /
    /// `--remote-ssh`.
    #[arg(
        long,
        value_name = "PAYLOAD",
        conflicts_with_all = ["local", "remote_ssh"],
    )]
    alleycat_pair: Option<String>,

    /// Override the on-disk path of the cached alleycat manifest
    /// JSON. Defaults to `artifacts/val-rem/008-alleycat-manifest.json`
    /// so the VAL-REM-008/009 evidence lands where the validator
    /// expects.
    #[arg(long, value_name = "PATH", requires = "alleycat_pair")]
    manifest_out: Option<PathBuf>,

    /// Optional one-shot HTTP listener that serves the cached
    /// manifest body verbatim. Validators `curl` against it to
    /// confirm VAL-REM-011. The listener dies with the process and
    /// shuts down after `--serve-manifest-max-hits` requests
    /// (default 1).
    #[arg(long, value_name = "ADDR", requires = "alleycat_pair")]
    serve_manifest: Option<std::net::SocketAddr>,

    /// How many HTTP requests `--serve-manifest` should answer
    /// before the listener task shuts down. Defaults to 1 so a
    /// single `curl` validates the endpoint and the listener exits.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 1,
        requires = "serve_manifest"
    )]
    serve_manifest_max_hits: u32,

    /// Skip the actual alleycat-path turn after caching the manifest.
    /// Useful when the alleycat-side pi process is unhealthy but the
    /// manifest evidence (VAL-REM-008/009/011) still needs collecting.
    #[arg(long, requires = "alleycat_pair")]
    alleycat_skip_turn: bool,
}

/// Wire shape of a single JSONL transcript line.
///
/// Each `PiEvent` becomes one of these on stdout. Keeping the shape
/// stable means downstream tools (`tuistory`, integration tests) can
/// pin against the field names rather than the raw `PiEvent` Debug
/// representation.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum TranscriptLine {
    PromptReceived {
        text: String,
    },
    AssistantTextDelta {
        delta: String,
    },
    AssistantText {
        text: String,
    },
    ToolExec {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecResult {
        tool_call_id: String,
        tool_name: String,
        result_text: String,
        is_error: bool,
    },
    TurnComplete,
    TurnError {
        message: String,
    },
    ShuttingDown,
    AuthState {
        /// Mirrors the variants of
        /// [`codex_mobile_client::store::AuthState`]: `unauthenticated`,
        /// `authorizing`, `authorized`, or `failed`.
        state: &'static str,
        /// `oauth` or `byok` for `authorized`; `None` otherwise.
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<&'static str>,
        /// Set when `state == "failed"`.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    OauthAuthorizeUrl {
        url: String,
    },
    OauthPasteWaiting,
    OauthCredentialsLoaded {
        source: &'static str,
    },
    ByokApplied {
        provider: String,
    },
    ClaudeCredentialsImported {
        provider: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        expires_ms: i64,
        expires_in_ms: i64,
    },
}

fn auth_state_line(event: &AuthEvent) -> TranscriptLine {
    match event {
        AuthEvent::Unauthenticated => TranscriptLine::AuthState {
            state: "unauthenticated",
            source: None,
            reason: None,
        },
        AuthEvent::Authorizing => TranscriptLine::AuthState {
            state: "authorizing",
            source: None,
            reason: None,
        },
        AuthEvent::Authorized { source, .. } => TranscriptLine::AuthState {
            state: "authorized",
            source: Some(auth_source_label(*source)),
            reason: None,
        },
        AuthEvent::Failed { reason } => TranscriptLine::AuthState {
            state: "failed",
            source: None,
            reason: Some(reason.clone()),
        },
    }
}

fn auth_source_label(source: AuthEventSource) -> &'static str {
    match source {
        AuthEventSource::Oauth => "oauth",
        AuthEventSource::Byok => "byok",
    }
}

/// True when `event` indicates pi already has a stored, usable
/// credential under the anthropic provider id — covering both OAuth
/// (Authorized{Oauth}) and BYOK (Authorized{Byok}) snapshots. Used by
/// the resolve fallback so a relaunch without env vars after a
/// successful BYOK persistence still reuses the on-disk key instead of
/// re-prompting.
fn is_stored_authorized(event: &AuthEvent) -> bool {
    matches!(event, AuthEvent::Authorized { .. })
}

fn main() -> ExitCode {
    // Best-effort tracing init; ignored if the user already wired one.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let cli = Cli::parse();

    if let Some(raw) = cli.alleycat_pair.clone() {
        let payload = resolve_alleycat_payload(&raw);
        let prompt = if cli.alleycat_skip_turn {
            cli.prompt.clone().unwrap_or_default()
        } else {
            match resolve_prompt(&cli) {
                Ok(p) => p,
                Err(err) => {
                    eprintln!("pi-server-runner: {err}");
                    return ExitCode::from(2);
                }
            }
        };
        let alleycat_args = AlleycatPairArgs {
            payload,
            prompt,
            events_out: cli.events_out.clone(),
            manifest_out: cli.manifest_out.clone(),
            serve_manifest: cli.serve_manifest,
            serve_max_hits: cli.serve_manifest_max_hits,
            skip_turn: cli.alleycat_skip_turn,
            timeout_secs: cli.timeout_secs,
        };
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("pi-server-runner: tokio runtime build failed: {err}");
                return ExitCode::from(2);
            }
        };
        return rt.block_on(drive_alleycat_pair(alleycat_args));
    }

    if let Some(host) = cli.remote_ssh.clone() {
        let user = cli
            .user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "pi".to_string());
        let prompt = match resolve_prompt(&cli) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("pi-server-runner: {err}");
                return ExitCode::from(2);
            }
        };
        let remote_args = RemoteSshArgs {
            host,
            user,
            port: cli.ssh_port,
            prompt,
            events_out: cli.events_out.clone(),
            inject_drop: cli.inject_drop,
            inject_drop_pid: cli.inject_drop_pid,
            inject_drop_idle_secs: cli.inject_drop_idle_secs,
            inject_drop_pause_secs: cli.inject_drop_pause_secs,
            inject_drop_script_dir: cli.inject_drop_script_dir.clone(),
            ssh_key_path: cli.ssh_key_path.clone(),
            timeout_secs: cli.timeout_secs,
        };
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("pi-server-runner: tokio runtime build failed: {err}");
                return ExitCode::from(2);
            }
        };
        return rt.block_on(drive_remote_ssh(remote_args));
    }

    if !cli.local {
        eprintln!(
            "pi-server-runner: pass --local for in-process pi or --remote-ssh <HOST> for remote pi acp."
        );
        return ExitCode::from(2);
    }

    // Cross-client OAuth reuse: import a Claude Code-shaped
    // credentials JSON into pi's auth.json. Runs before OAuth/BYOK
    // so the imported anthropic credential is the baseline that the
    // remaining setup flags (if any) override.
    if cli.import_claude_credentials
        && let Err(code) = run_claude_credentials_import_flow(
            cli.auth_path.clone(),
            cli.credentials_path.clone(),
        )
    {
        return code;
    }

    // OAuth paste-back handshake. Runs before BYOK so that a single
    // `--oauth-paste --byok ...` invocation (a developer setting up
    // both code paths against the same auth.json) sees the OAuth
    // tokens persisted first, then the BYOK key written on top.
    if cli.oauth_paste
        && let Err(code) = run_oauth_paste_flow(cli.auth_path.clone())
    {
        return code;
    }

    // BYOK persistence path. Writes the supplied API key into pi's
    // `auth.json` via the shared `pi_byok_set` helper so subsequent
    // runs (without `--byok`) reuse the credential.
    if cli.byok
        && let Err(code) = run_byok_persist_flow(
            cli.auth_path.clone(),
            cli.anthropic_key.clone(),
            cli.openai_key.clone(),
            cli.openai_base_url.clone(),
        )
    {
        return code;
    }

    // BYOK env plumbing happens here, on the still-single-threaded
    // main thread. Reading env vars is safe; `pi-mobile-client` is
    // `#![forbid(unsafe_code)]` so it cannot do this internally.
    //
    // When `--byok` was supplied with an `--anthropic-key` or
    // `--openai-key` we also seed the matching env var so the rest of
    // `resolve_provider` + `PiSessionConfig` plumbing reuses the same
    // credential without forcing the caller to also export the env
    // var. The on-disk credential persisted above is what subsequent
    // (no-`--byok`) invocations consume.
    let env = if cli.byok {
        // When `--byok` was supplied, take the BYOK profile from the
        // CLI flags verbatim and ignore any `*_API_KEY` env vars from
        // the other provider so `--byok --openai-key ...` cannot be
        // accidentally overridden by a leftover `ANTHROPIC_API_KEY`.
        ProviderEnv {
            anthropic_key: cli.anthropic_key.clone(),
            anthropic_base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
            openai_key: cli.openai_key.clone(),
            openai_base_url: cli
                .openai_base_url
                .clone()
                .or_else(|| std::env::var("OPENAI_BASE_URL").ok()),
        }
    } else {
        ProviderEnv {
            anthropic_key: std::env::var("ANTHROPIC_API_KEY").ok(),
            anthropic_base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
            openai_key: std::env::var("OPENAI_API_KEY").ok(),
            openai_base_url: std::env::var("OPENAI_BASE_URL").ok(),
        }
    };

    // Surface a snapshot of the resolved auth state so the transcript
    // shows the runner is reusing existing credentials (or operating
    // unauthenticated when no key/OAuth was supplied). Skipped when
    // `--oauth-paste` already emitted the post-handshake `authorized`
    // line, since duplicating it would clutter the transcript.
    let stored_snapshot = snapshot_anthropic_oauth(AnthropicOAuthConfig {
        client_id: None,
        client_secret: None,
        auth_path: cli.auth_path.clone(),
    });
    // Accept either OAuth or BYOK Authorized snapshots so a relaunch
    // without env vars after a successful BYOK persistence reuses the
    // stored credential instead of erroring on the missing env key.
    let has_stored_authorized = is_stored_authorized(&stored_snapshot);
    let stored_oauth_only = matches!(
        stored_snapshot,
        AuthEvent::Authorized {
            source: AuthEventSource::Oauth,
            ..
        }
    );
    if !cli.oauth_paste {
        if stored_oauth_only {
            emit_line(&TranscriptLine::OauthCredentialsLoaded { source: "oauth" });
        }
        emit_line(&auth_state_line(&stored_snapshot));
    }

    let resolved = match resolve_provider(cli.provider.clone(), &env) {
        Ok(r) => r,
        Err(err) if has_stored_authorized => {
            tracing::debug!(
                target: "pi_server_runner",
                "no BYOK env key ********* relying on stored Anthropic credential ({err})"
            );
            ResolvedProvider {
                provider: Some("anthropic".to_string()),
                api_key: None,
                base_url: env.anthropic_base_url.clone(),
            }
        }
        Err(err) => {
            // Setup-only invocations (e.g. `--oauth-paste` without
            // `--prompt`) legitimately have no key. Don't error out
            // here if no prompt was supplied either; we'll exit at the
            // resolve_prompt step below.
            if cli.prompt.is_none() && !cli.stdin {
                ResolvedProvider {
                    provider: None,
                    api_key: None,
                    base_url: None,
                }
            } else {
                eprintln!("pi-server-runner: {err}");
                return ExitCode::from(2);
            }
        }
    };
    let ResolvedProvider {
        provider,
        api_key,
        base_url,
    } = resolved;

    // Pi's default Anthropic model ids use the `*-latest` form which
    // BYOK proxies (e.g. cli-proxy.getpitchfork.com) reject with a 502
    // `unknown provider for model`. When the active provider is
    // Anthropic AND a custom proxy base URL is in play AND the caller
    // did not pick an explicit `--model`, default to a date-suffixed id
    // that proxies actually route. The non-proxy path keeps pi's
    // existing default behavior.
    let model = pick_default_model(cli.model.clone(), provider.as_deref(), base_url.as_deref());

    // Setup-only runs (`--oauth-paste` or `--byok` without a prompt)
    // exit cleanly after persisting credentials, so platform callers
    // can split "authorize" and "run prompt" into separate process
    // invocations. We require `--prompt`/`--stdin` only when one of
    // those setup flags is *not* present.
    let prompt_text = match resolve_prompt(&cli) {
        Ok(p) => p,
        Err(err) => {
            if cli.oauth_paste || cli.byok || cli.import_claude_credentials {
                tracing::info!(
                    target: "pi_server_runner",
                    "setup-only invocation complete (no prompt supplied); exiting"
                );
                return ExitCode::SUCCESS;
            }
            eprintln!("pi-server-runner: {err}");
            return ExitCode::from(2);
        }
    };

    // Surface the selected factory on stderr so validators (and
    // downstream tooling like tuistory) can confirm the right
    // factory was wired into the in-process runtime. The label is
    // intentionally stable; tests pin against it.
    let factory_label = cli.tool_factory.display_name();
    eprintln!("pi-server-runner: tool_factory={factory_label}");
    tracing::info!(
        target: "pi_server_runner",
        tool_factory = factory_label,
        "selected tool factory for in-process pi runtime"
    );

    let session_config = PiSessionConfig {
        provider,
        model,
        api_key,
        base_url,
        working_directory: std::env::current_dir().ok(),
        append_system_prompt: None,
        max_tool_iterations: None,
        enabled_tools: None,
        tool_factory: Some(cli.tool_factory.to_kind()),
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("pi-server-runner: tokio runtime build failed: {err}");
            return ExitCode::from(2);
        }
    };

    let timeout = Duration::from_secs(cli.timeout_secs);
    rt.block_on(async move { drive_local(session_config, prompt_text, timeout).await })
}

/// Translate the `--alleycat-pair` flag value into a JSON payload.
/// The literal string `env` (or an empty value) resolves the payload
/// from `$PI_ALLEYCAT_PAIR_PAYLOAD` so CI can keep the token out of
/// argv. Anything else is passed through verbatim.
fn resolve_alleycat_payload(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("env") {
        std::env::var("PI_ALLEYCAT_PAIR_PAYLOAD").unwrap_or_default()
    } else {
        trimmed.to_string()
    }
}

fn resolve_prompt(cli: &Cli) -> Result<String, String> {
    if let Some(text) = &cli.prompt {
        return Ok(text.clone());
    }
    if cli.stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("reading stdin: {e}"))?;
        if buf.trim().is_empty() {
            return Err("stdin was empty; supply --prompt or pipe text into stdin".to_string());
        }
        return Ok(buf);
    }
    Err("missing --prompt; pass --prompt '<text>' or --stdin".to_string())
}

async fn drive_local(
    session_config: PiSessionConfig,
    prompt: String,
    timeout: Duration,
) -> ExitCode {
    let handle = start_in_process(InProcessStartArgs {
        command_buffer: Some(8),
        event_buffer: Some(256),
        session: Some(session_config),
    });
    let mut events = handle.subscribe();

    if let Err(err) = handle.send(Command::Prompt(prompt)) {
        eprintln!("pi-server-runner: failed to enqueue prompt: {err}");
        return ExitCode::from(2);
    }

    let exit = match tokio::time::timeout(timeout, pump_events(&mut events)).await {
        Ok(code) => code,
        Err(_) => {
            eprintln!(
                "pi-server-runner: timed out after {}s without TurnComplete",
                timeout.as_secs()
            );
            ExitCode::from(1)
        }
    };

    // Drop the handle so the asupersync runtime shuts down cleanly
    // before the process exits.
    drop(handle);
    exit
}

async fn pump_events(
    events: &mut tokio::sync::broadcast::Receiver<PiEvent>,
) -> ExitCode {
    loop {
        match events.recv().await {
            Ok(event) => {
                let (line, terminal) = render(event);
                emit_line(&line);
                match terminal {
                    Terminal::Continue => {}
                    Terminal::Success => return ExitCode::SUCCESS,
                    Terminal::Failure => return ExitCode::from(1),
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                eprintln!(
                    "pi-server-runner: event stream lagged ({} events skipped)",
                    skipped
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                eprintln!("pi-server-runner: event stream closed before TurnComplete");
                return ExitCode::from(1);
            }
        }
    }
}

enum Terminal {
    Continue,
    Success,
    Failure,
}

fn render(event: PiEvent) -> (TranscriptLine, Terminal) {
    match event {
        PiEvent::PromptReceived { text } => (
            TranscriptLine::PromptReceived { text },
            Terminal::Continue,
        ),
        PiEvent::AssistantTextDelta { delta } => (
            TranscriptLine::AssistantTextDelta { delta },
            Terminal::Continue,
        ),
        PiEvent::AssistantText { text } => (
            TranscriptLine::AssistantText { text },
            Terminal::Continue,
        ),
        PiEvent::ToolExecStart {
            tool_call_id,
            tool_name,
            args_json,
        } => {
            let args = serde_json::from_str::<serde_json::Value>(&args_json)
                .unwrap_or(serde_json::Value::String(args_json));
            (
                TranscriptLine::ToolExec {
                    tool_call_id,
                    tool_name,
                    args,
                },
                Terminal::Continue,
            )
        }
        PiEvent::ToolExecEnd {
            tool_call_id,
            tool_name,
            result_text,
            is_error,
        } => (
            TranscriptLine::ToolExecResult {
                tool_call_id,
                tool_name,
                result_text,
                is_error,
            },
            Terminal::Continue,
        ),
        PiEvent::TurnComplete => (TranscriptLine::TurnComplete, Terminal::Success),
        PiEvent::TurnError { message } => (
            TranscriptLine::TurnError { message },
            Terminal::Failure,
        ),
        PiEvent::ShuttingDown => (TranscriptLine::ShuttingDown, Terminal::Continue),
    }
}

/// Snapshot of BYOK-relevant env vars, captured up-front so the
/// provider/base-url decision is a pure function of inputs (and
/// trivially testable without touching process env).
#[derive(Debug, Default, Clone)]
struct ProviderEnv {
    anthropic_key: Option<String>,
    anthropic_base_url: Option<String>,
    openai_key: Option<String>,
    openai_base_url: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct ResolvedProvider {
    provider: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
}

/// Choose provider, API key, and base URL from the CLI flag plus env.
///
/// Rules:
/// * `--provider anthropic` → Anthropic key + `ANTHROPIC_BASE_URL`.
/// * `--provider openai` → OpenAI key + `OPENAI_BASE_URL`.
/// * Any other explicit provider → prefer Anthropic key, fall back to
///   OpenAI key; no base URL is inferred (caller's env was custom).
/// * No `--provider`: pick Anthropic if its key is set, else OpenAI;
///   the matching `*_BASE_URL` is applied.
/// * No keys at all → error.
///
/// When both base-URL vars are set, the one matching the *active*
/// provider wins (provider-specific override).
fn resolve_provider(
    cli_provider: Option<String>,
    env: &ProviderEnv,
) -> Result<ResolvedProvider, String> {
    let (provider, api_key, base_url) = match (cli_provider, &env.anthropic_key, &env.openai_key) {
        (Some(p), _, _) if p == "anthropic" => (
            Some(p),
            env.anthropic_key.clone(),
            env.anthropic_base_url.clone(),
        ),
        (Some(p), _, _) if p == "openai" => (
            Some(p),
            env.openai_key.clone(),
            env.openai_base_url.clone(),
        ),
        (Some(p), _, _) => (
            Some(p),
            env.anthropic_key.clone().or_else(|| env.openai_key.clone()),
            None,
        ),
        (None, Some(_), _) => (
            Some("anthropic".to_string()),
            env.anthropic_key.clone(),
            env.anthropic_base_url.clone(),
        ),
        (None, None, Some(_)) => (
            Some("openai".to_string()),
            env.openai_key.clone(),
            env.openai_base_url.clone(),
        ),
        (None, None, None) => {
            return Err(
                "no BYOK credentials in environment. Export ANTHROPIC_API_KEY (+ optional ANTHROPIC_BASE_URL) or OPENAI_API_KEY (+ optional OPENAI_BASE_URL).".to_string(),
            );
        }
    };
    Ok(ResolvedProvider {
        provider,
        api_key,
        base_url,
    })
}

/// Pick a default `--model` for the resolved provider/base-URL pair.
///
/// When the caller passed `--model` explicitly we always honor it. Otherwise:
///
/// * If the active provider is `anthropic` AND a custom proxy base URL is
///   configured, default to a date-suffixed Anthropic model id that BYOK
///   proxies actually accept. Pi's stock default for Anthropic is a
///   `*-latest` alias which `cli-proxy.getpitchfork.com` (and similar)
///   reject with a 502 `unknown provider for model`.
/// * Otherwise leave the choice to pi's existing default selection.
fn pick_default_model(
    explicit: Option<String>,
    provider: Option<&str>,
    base_url: Option<&str>,
) -> Option<String> {
    if explicit.is_some() {
        return explicit;
    }
    let is_anthropic = provider
        .map(|p| p.eq_ignore_ascii_case("anthropic"))
        .unwrap_or(false);
    let has_proxy_base = base_url
        .map(|u| !u.trim().is_empty())
        .unwrap_or(false);
    if is_anthropic && has_proxy_base {
        return Some("claude-opus-4-5-20251101".to_string());
    }
    None
}

fn emit_line(line: &TranscriptLine) {
    match serde_json::to_string(line) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("pi-server-runner: failed to serialize transcript line: {err}"),
    }
}

/// Drive the Anthropic OAuth Authorization Code + PKCE handshake with
/// a pasted redirect code. Prints the authorize URL on stdout (as a
/// transcript line), attempts to open it via `open` when a TTY is
/// attached, then reads the redirect code from stdin and persists the
/// resulting tokens via the shared driver.
fn run_oauth_paste_flow(auth_path: Option<PathBuf>) -> Result<(), ExitCode> {
    let driver = AnthropicOAuthDriver::new(AnthropicOAuthConfig {
        client_id: None,
        client_secret: None,
        auth_path: auth_path.clone(),
    });

    let (begin_event, handshake) = match driver.begin() {
        Ok(pair) => pair,
        Err(failure) => {
            emit_line(&auth_state_line(&failure));
            eprintln!("pi-server-runner: failed to build Anthropic authorize URL");
            return Err(ExitCode::from(2));
        }
    };
    emit_line(&auth_state_line(&begin_event));
    let AuthorizeHandshake {
        authorize_url,
        verifier,
        ..
    } = handshake;
    emit_line(&TranscriptLine::OauthAuthorizeUrl {
        url: authorize_url.clone(),
    });

    // `open` is macOS-only and we don't want test invocations (which
    // pipe stdin) to spawn a browser, so only auto-open when stdin and
    // stdout are both TTYs and `open` actually exists on PATH.
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let _ = ShellCommand::new("open")
            .arg(&authorize_url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    emit_line(&TranscriptLine::OauthPasteWaiting);
    if std::io::stdin().is_terminal() {
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "Paste redirect code: ");
        let _ = stdout.flush();
    }

    let mut code_input = String::new();
    let stdin = std::io::stdin();
    let read = stdin
        .lock()
        .read_line(&mut code_input)
        .map_err(|err| {
            eprintln!("pi-server-runner: failed to read redirect code: {err}");
            ExitCode::from(2)
        })?;
    if read == 0 || code_input.trim().is_empty() {
        eprintln!("pi-server-runner: redirect code was empty");
        return Err(ExitCode::from(2));
    }
    let code_input = code_input.trim().to_string();

    let outcome = complete_anthropic_oauth_paste(
        AnthropicOAuthConfig {
            client_id: None,
            client_secret: None,
            auth_path,
        },
        code_input,
        verifier,
    );
    emit_line(&auth_state_line(&outcome));
    match outcome {
        AuthEvent::Authorized { .. } => Ok(()),
        AuthEvent::Failed { reason } => {
            eprintln!("pi-server-runner: oauth handshake failed: {reason}");
            Err(ExitCode::from(1))
        }
        other => {
            eprintln!(
                "pi-server-runner: unexpected post-handshake auth state: {other:?}"
            );
            Err(ExitCode::from(1))
        }
    }
}

/// Persist a BYOK credential into pi's `auth.json` via the shared
/// `pi_byok_set` helper. Accepts exactly one of `--anthropic-key`
/// or `--openai-key`; the OpenAI path additionally records a base
/// URL when supplied (so subsequent runs without `--byok` reuse the
/// proxy via `OPENAI_BASE_URL` in the env).
fn run_byok_persist_flow(
    auth_path: Option<PathBuf>,
    anthropic_key: Option<String>,
    openai_key: Option<String>,
    openai_base_url: Option<String>,
) -> Result<(), ExitCode> {
    let (provider, key) = match (anthropic_key, openai_key) {
        (Some(a), None) => ("anthropic".to_string(), a),
        (None, Some(o)) => ("openai".to_string(), o),
        (Some(_), Some(_)) => {
            eprintln!(
                "pi-server-runner: --byok accepts exactly one of --anthropic-key or --openai-key"
            );
            return Err(ExitCode::from(2));
        }
        (None, None) => {
            eprintln!(
                "pi-server-runner: --byok requires --anthropic-key <KEY> or --openai-key <KEY> (with optional --openai-base-url <URL>)"
            );
            return Err(ExitCode::from(2));
        }
    };

    if provider == "anthropic" && openai_base_url.is_some() {
        eprintln!(
            "pi-server-runner: --openai-base-url is only valid with --openai-key; ignoring"
        );
    }

    let event = pi_byok_set_blocking(auth_path, provider.clone(), key);
    emit_line(&auth_state_line(&event));
    match event {
        AuthEvent::Authorized { .. } => {
            emit_line(&TranscriptLine::ByokApplied { provider });
            Ok(())
        }
        AuthEvent::Failed { reason } => {
            eprintln!("pi-server-runner: byok persist failed: {reason}");
            Err(ExitCode::from(1))
        }
        other => {
            eprintln!("pi-server-runner: unexpected byok auth state: {other:?}");
            Err(ExitCode::from(1))
        }
    }
}

/// Import an Anthropic OAuth credential from a Claude
/// Code-shaped credentials JSON file. The credentials path is
/// resolved from `--credentials-path` first, then from
/// `CLAUDE_CREDENTIALS_JSON_PATH`. The raw token values never appear
/// in any emitted transcript line or stderr message — only the
/// redacted summary (provider, email, expires-in-ms) is logged.
fn run_claude_credentials_import_flow(
    auth_path: Option<PathBuf>,
    credentials_path: Option<PathBuf>,
) -> Result<(), ExitCode> {
    let path = credentials_path
        .or_else(|| std::env::var("CLAUDE_CREDENTIALS_JSON_PATH").ok().map(PathBuf::from));
    let path = match path {
        Some(p) => p,
        None => {
            eprintln!(
                "pi-server-runner: --import-claude-credentials requires --credentials-path <PATH> or CLAUDE_CREDENTIALS_JSON_PATH"
            );
            return Err(ExitCode::from(2));
        }
    };
    let json_text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!(
                "pi-server-runner: could not read claude credentials at {}: {err}",
                path.display()
            );
            return Err(ExitCode::from(2));
        }
    };
    let outcome = import_claude_credentials(
        &ClaudeImportConfig {
            auth_path: auth_path.clone(),
        },
        &json_text,
    );
    emit_line(&auth_state_line(&outcome.event));
    match (outcome.event, outcome.summary) {
        (AuthEvent::Authorized { .. }, Some(summary)) => {
            log_claude_summary(&summary);
            emit_line(&TranscriptLine::ClaudeCredentialsImported {
                provider: summary.provider,
                email: summary.email,
                expires_ms: summary.expires_ms,
                expires_in_ms: summary.expires_in_ms,
            });
            Ok(())
        }
        (AuthEvent::Failed { reason }, _) => {
            eprintln!("pi-server-runner: claude import failed: {reason}");
            Err(ExitCode::from(1))
        }
        (other, _) => {
            eprintln!(
                "pi-server-runner: unexpected post-import auth state: {other:?}"
            );
            Err(ExitCode::from(1))
        }
    }
}

fn log_claude_summary(summary: &ClaudeImportSummary) {
    // The summary intentionally carries only non-secret metadata.
    // Tokens never reach tracing here.
    tracing::info!(
        target: "pi_server_runner",
        provider = %summary.provider,
        email = %summary.email.clone().unwrap_or_else(|| "<none>".to_string()),
        expires_in_ms = summary.expires_in_ms,
        "imported anthropic oauth credential from claude credentials file"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(
        anth_key: Option<&str>,
        anth_base: Option<&str>,
        oa_key: Option<&str>,
        oa_base: Option<&str>,
    ) -> ProviderEnv {
        ProviderEnv {
            anthropic_key: anth_key.map(str::to_string),
            anthropic_base_url: anth_base.map(str::to_string),
            openai_key: oa_key.map(str::to_string),
            openai_base_url: oa_base.map(str::to_string),
        }
    }

    #[test]
    fn anthropic_provider_picks_up_anthropic_base_url() {
        let env = env_with(
            Some("sk-ant-test"),
            Some("https://anthropic.proxy.example/v1"),
            None,
            None,
        );
        let resolved =
            resolve_provider(Some("anthropic".to_string()), &env).expect("provider resolves");
        assert_eq!(resolved.provider.as_deref(), Some("anthropic"));
        assert_eq!(resolved.api_key.as_deref(), Some("sk-ant-test"));
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://anthropic.proxy.example/v1")
        );
    }

    #[test]
    fn openai_provider_picks_up_openai_base_url_and_ignores_anthropic_url() {
        let env = env_with(
            Some("sk-ant-test"),
            Some("https://anthropic.proxy.example/v1"),
            Some("sk-oa-test"),
            Some("https://oa.proxy.example/v1"),
        );
        let resolved =
            resolve_provider(Some("openai".to_string()), &env).expect("provider resolves");
        assert_eq!(resolved.provider.as_deref(), Some("openai"));
        assert_eq!(resolved.api_key.as_deref(), Some("sk-oa-test"));
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://oa.proxy.example/v1")
        );
    }

    #[test]
    fn anthropic_provider_with_both_base_urls_prefers_anthropic() {
        let env = env_with(
            Some("sk-ant-test"),
            Some("https://anthropic.proxy.example/v1"),
            Some("sk-oa-test"),
            Some("https://oa.proxy.example/v1"),
        );
        let resolved =
            resolve_provider(Some("anthropic".to_string()), &env).expect("provider resolves");
        assert_eq!(
            resolved.base_url.as_deref(),
            Some("https://anthropic.proxy.example/v1"),
            "ANTHROPIC_BASE_URL must win when --provider anthropic is active"
        );
    }

    #[test]
    fn default_provider_uses_anthropic_when_only_anth_key_present() {
        let env = env_with(Some("sk-ant"), Some("https://anth.example"), None, None);
        let resolved = resolve_provider(None, &env).expect("provider resolves");
        assert_eq!(resolved.provider.as_deref(), Some("anthropic"));
        assert_eq!(resolved.base_url.as_deref(), Some("https://anth.example"));
    }

    #[test]
    fn default_provider_uses_openai_when_only_oa_key_present() {
        let env = env_with(None, None, Some("sk-oa"), Some("https://oa.example"));
        let resolved = resolve_provider(None, &env).expect("provider resolves");
        assert_eq!(resolved.provider.as_deref(), Some("openai"));
        assert_eq!(resolved.base_url.as_deref(), Some("https://oa.example"));
    }

    #[test]
    fn pick_default_model_honors_explicit() {
        assert_eq!(
            pick_default_model(
                Some("custom-id".to_string()),
                Some("anthropic"),
                Some("https://proxy.example/v1")
            )
            .as_deref(),
            Some("custom-id"),
            "--model must always win over the heuristic"
        );
    }

    #[test]
    fn pick_default_model_switches_to_date_suffixed_on_anthropic_proxy() {
        let model = pick_default_model(None, Some("anthropic"), Some("https://proxy.example/v1"));
        assert_eq!(
            model.as_deref(),
            Some("claude-opus-4-5-20251101"),
            "proxy + anthropic must default to a date-suffixed model id"
        );
    }

    #[test]
    fn pick_default_model_leaves_pi_default_for_non_proxy_anthropic() {
        let model = pick_default_model(None, Some("anthropic"), None);
        assert!(
            model.is_none(),
            "non-proxy anthropic path must keep pi's stock default model"
        );
        let model_empty = pick_default_model(None, Some("anthropic"), Some("   "));
        assert!(
            model_empty.is_none(),
            "whitespace base_url must not trigger the proxy default"
        );
    }

    #[test]
    fn pick_default_model_leaves_pi_default_for_openai_proxy() {
        let model = pick_default_model(None, Some("openai"), Some("https://oa.proxy.example/v1"));
        assert!(
            model.is_none(),
            "non-anthropic providers must keep pi's stock default model"
        );
    }

    #[test]
    fn tool_factory_default_and_kind_round_trip() {
        // Default value parsed by clap when the user omits --tool-factory.
        let cli = Cli::try_parse_from(["pi-server-runner", "--local", "--prompt", "p"])
            .expect("parse without flag");
        assert_eq!(cli.tool_factory, ToolFactoryArg::PtyDev);
        assert_eq!(cli.tool_factory.display_name(), "pty-dev");
        // The kind we hand to PiSessionConfig must be PtyDev.
        match cli.tool_factory.to_kind() {
            ToolFactoryKind::PtyDev => {}
            other => panic!("unexpected tool factory kind: {other:?}"),
        }
    }

    #[test]
    fn tool_factory_explicit_pty_dev_parses() {
        let cli = Cli::try_parse_from([
            "pi-server-runner",
            "--local",
            "--tool-factory",
            "pty-dev",
            "--prompt",
            "p",
        ])
        .expect("parse with --tool-factory pty-dev");
        assert_eq!(cli.tool_factory, ToolFactoryArg::PtyDev);
    }

    #[test]
    fn tool_factory_rejects_unknown_value() {
        let err = Cli::try_parse_from([
            "pi-server-runner",
            "--local",
            "--tool-factory",
            "nope",
            "--prompt",
            "p",
        ])
        .expect_err("unknown factory must error");
        let rendered = err.to_string();
        assert!(
            rendered.contains("pty-dev"),
            "clap error should advertise the supported factory value, got: {rendered}"
        );
    }

    #[test]
    fn no_keys_returns_error() {
        let env = env_with(None, None, None, None);
        let err = resolve_provider(None, &env).expect_err("must error with no creds");
        assert!(err.contains("ANTHROPIC_API_KEY"));
        assert!(err.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn oauth_paste_flag_parses() {
        let cli = Cli::try_parse_from([
            "pi-server-runner",
            "--local",
            "--oauth-paste",
        ])
        .expect("parse with --oauth-paste");
        assert!(cli.oauth_paste);
        assert!(!cli.byok);
    }

    #[test]
    fn byok_anthropic_flag_parses() {
        let cli = Cli::try_parse_from([
            "pi-server-runner",
            "--local",
            "--byok",
            "--anthropic-key",
            "sk-ant-test",
            "--prompt",
            "hello",
        ])
        .expect("parse with --byok --anthropic-key");
        assert!(cli.byok);
        assert_eq!(cli.anthropic_key.as_deref(), Some("sk-ant-test"));
        assert!(cli.openai_key.is_none());
    }

    #[test]
    fn byok_openai_with_base_url_flag_parses() {
        let cli = Cli::try_parse_from([
            "pi-server-runner",
            "--local",
            "--byok",
            "--openai-key",
            "sk-oa-test",
            "--openai-base-url",
            "https://proxy.example/v1",
            "--prompt",
            "hello",
        ])
        .expect("parse with --byok --openai-* trio");
        assert!(cli.byok);
        assert_eq!(cli.openai_key.as_deref(), Some("sk-oa-test"));
        assert_eq!(
            cli.openai_base_url.as_deref(),
            Some("https://proxy.example/v1")
        );
    }

    #[test]
    fn auth_state_line_maps_each_event_variant() {
        match auth_state_line(&AuthEvent::Unauthenticated) {
            TranscriptLine::AuthState {
                state,
                source,
                reason,
            } => {
                assert_eq!(state, "unauthenticated");
                assert!(source.is_none());
                assert!(reason.is_none());
            }
            other => panic!("expected AuthState transcript line, got {other:?}"),
        }

        match auth_state_line(&AuthEvent::Authorizing) {
            TranscriptLine::AuthState { state, .. } => assert_eq!(state, "authorizing"),
            other => panic!("expected AuthState transcript line, got {other:?}"),
        }

        match auth_state_line(&AuthEvent::Authorized {
            source: AuthEventSource::Oauth,
            refresh_token: None,
        }) {
            TranscriptLine::AuthState { state, source, .. } => {
                assert_eq!(state, "authorized");
                assert_eq!(source, Some("oauth"));
            }
            other => panic!("expected AuthState transcript line, got {other:?}"),
        }

        match auth_state_line(&AuthEvent::Authorized {
            source: AuthEventSource::Byok,
            refresh_token: None,
        }) {
            TranscriptLine::AuthState { state, source, .. } => {
                assert_eq!(state, "authorized");
                assert_eq!(source, Some("byok"));
            }
            other => panic!("expected AuthState transcript line, got {other:?}"),
        }

        match auth_state_line(&AuthEvent::Failed {
            reason: "boom".to_string(),
        }) {
            TranscriptLine::AuthState { state, reason, .. } => {
                assert_eq!(state, "failed");
                assert_eq!(reason.as_deref(), Some("boom"));
            }
            other => panic!("expected AuthState transcript line, got {other:?}"),
        }
    }

    #[test]
    fn is_stored_authorized_accepts_both_oauth_and_byok() {
        // OAuth-source stored credential — the original supported case.
        assert!(is_stored_authorized(&AuthEvent::Authorized {
            source: AuthEventSource::Oauth,
            refresh_token: None,
        }));
        // BYOK-source stored credential — the relaunch-without-env path
        // this feature must now also cover.
        assert!(is_stored_authorized(&AuthEvent::Authorized {
            source: AuthEventSource::Byok,
            refresh_token: None,
        }));
        assert!(!is_stored_authorized(&AuthEvent::Unauthenticated));
        assert!(!is_stored_authorized(&AuthEvent::Authorizing));
        assert!(!is_stored_authorized(&AuthEvent::Failed {
            reason: "x".to_string(),
        }));
    }

    #[test]
    fn byok_persisted_credential_is_reused_on_relaunch_without_env() {
        // Simulates a BYOK relaunch path: after `--byok` writes an
        // anthropic key to auth.json, a subsequent invocation without
        // env vars must observe `Authorized { Byok }` and treat it as
        // a stored credential the resolve fallback can rely on.
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        run_byok_persist_flow(
            Some(auth_path.clone()),
            Some("sk-ant-relaunch-test".to_string()),
            None,
            None,
        )
        .expect("anthropic byok must succeed");

        let snap = snapshot_anthropic_oauth(AnthropicOAuthConfig {
            client_id: None,
            client_secret: None,
            auth_path: Some(auth_path.clone()),
        });
        assert!(
            matches!(
                snap,
                AuthEvent::Authorized {
                    source: AuthEventSource::Byok,
                    ..
                }
            ),
            "expected Authorized {{ Byok }}, got {snap:?}"
        );
        assert!(
            is_stored_authorized(&snap),
            "BYOK Authorized snapshot must satisfy is_stored_authorized so the resolve fallback reuses it"
        );
    }

    #[test]
    fn run_byok_persist_flow_rejects_missing_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let err =
            run_byok_persist_flow(Some(dir.path().join("auth.json")), None, None, None)
                .expect_err("missing key must error");
        // ExitCode doesn't implement PartialEq directly; format via Debug.
        assert!(
            format!("{err:?}").contains('2'),
            "missing-key path must exit 2, got {err:?}"
        );
    }

    #[test]
    fn run_byok_persist_flow_anthropic_persists_credential() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        run_byok_persist_flow(
            Some(auth_path.clone()),
            Some("sk-ant-runner-test".to_string()),
            None,
            None,
        )
        .expect("anthropic byok must succeed");

        // After persistence the snapshot helper reports an
        // `Authorized { source: Byok }` event — proof the credential
        // was written and is reachable through pi's AuthStorage on a
        // follow-up load.
        let snap = snapshot_anthropic_oauth(AnthropicOAuthConfig {
            client_id: None,
            client_secret: None,
            auth_path: Some(auth_path.clone()),
        });
        match snap {
            AuthEvent::Authorized {
                source: AuthEventSource::Byok,
                ..
            } => {}
            other => panic!("expected Authorized {{ Byok }}, got {other:?}"),
        }

        // And the file actually exists on disk.
        assert!(auth_path.exists(), "auth.json must be created");
    }

    #[test]
    fn run_byok_persist_flow_openai_records_provider_id() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        run_byok_persist_flow(
            Some(auth_path.clone()),
            None,
            Some("sk-oa-runner-test".to_string()),
            Some("https://proxy.example/v1".to_string()),
        )
        .expect("openai byok must succeed");

        // The OpenAI BYOK path does not register under the Anthropic
        // provider id, so the OAuth snapshot helper (which looks up
        // `anthropic`) must report `Unauthenticated`. This proves the
        // persisted entry landed under a non-anthropic key without
        // having to re-export pi's AuthStorage out of pi-mobile-client.
        let snap = snapshot_anthropic_oauth(AnthropicOAuthConfig {
            client_id: None,
            client_secret: None,
            auth_path: Some(auth_path.clone()),
        });
        assert_eq!(snap, AuthEvent::Unauthenticated);
        assert!(auth_path.exists(), "auth.json must be created");
        let contents = std::fs::read_to_string(&auth_path).expect("read auth.json");
        assert!(
            contents.contains("\"openai\""),
            "auth.json should record the openai provider entry, got: {contents}"
        );
    }

    #[test]
    fn import_claude_credentials_flag_parses() {
        let cli = Cli::try_parse_from([
            "pi-server-runner",
            "--local",
            "--import-claude-credentials",
            "--credentials-path",
            "/tmp/claude-creds.json",
        ])
        .expect("parse with --import-claude-credentials");
        assert!(cli.import_claude_credentials);
        assert_eq!(
            cli.credentials_path.as_deref().and_then(|p| p.to_str()),
            Some("/tmp/claude-creds.json")
        );
    }

    #[test]
    fn run_claude_credentials_import_flow_rejects_missing_path() {
        // Clear CLAUDE_CREDENTIALS_JSON_PATH so the flag can't pick
        // up a stale ambient value while this test runs in isolation.
        // SAFETY: scoped env removal mirrored at the end of the test.
        let prior = std::env::var("CLAUDE_CREDENTIALS_JSON_PATH").ok();
        // SAFETY: env mutation is intentional for this test scope.
        unsafe {
            std::env::remove_var("CLAUDE_CREDENTIALS_JSON_PATH");
        }
        let dir = tempfile::tempdir().expect("tmpdir");
        let result = run_claude_credentials_import_flow(
            Some(dir.path().join("auth.json")),
            None,
        );
        let err = result.expect_err("missing path must error");
        assert!(
            format!("{err:?}").contains('2'),
            "missing-path must exit 2, got {err:?}"
        );
        // SAFETY: restore prior env value.
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("CLAUDE_CREDENTIALS_JSON_PATH", v);
            }
        }
    }

    #[test]
    fn run_claude_credentials_import_flow_persists_oauth_entry() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let creds_path = dir.path().join("claude-creds.json");
        // Clearly-fake values — never use real Anthropic tokens here.
        let creds_json = r#"{
            "access_token": "sk-ant-oat01-RUNNER-FAKE-ACCESS",
            "refresh_token": "sk-ant-ort01-RUNNER-FAKE-REFRESH",
            "expired": "2026-05-26T05:16:09+08:00",
            "email": "tester@example.com",
            "type": "claude"
        }"#;
        std::fs::write(&creds_path, creds_json).expect("write creds");
        let auth_path = dir.path().join("auth.json");
        run_claude_credentials_import_flow(
            Some(auth_path.clone()),
            Some(creds_path),
        )
        .expect("import must succeed");
        // The snapshot helper observes the imported OAuth credential.
        let snap = snapshot_anthropic_oauth(AnthropicOAuthConfig {
            client_id: None,
            client_secret: None,
            auth_path: Some(auth_path),
        });
        match snap {
            AuthEvent::Authorized {
                source: AuthEventSource::Oauth,
                ..
            } => {}
            other => panic!("expected Authorized {{ Oauth }}, got {other:?}"),
        }
    }

    #[test]
    fn run_byok_persist_flow_rejects_both_keys() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let err = run_byok_persist_flow(
            Some(dir.path().join("auth.json")),
            Some("a".to_string()),
            Some("b".to_string()),
            None,
        )
        .expect_err("both keys must error");
        assert!(
            format!("{err:?}").contains('2'),
            "both-keys path must exit 2, got {err:?}"
        );
    }
}
