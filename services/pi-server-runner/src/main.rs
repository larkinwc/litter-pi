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

use std::io::Read as _;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use pi_mobile_client::{
    Command, InProcessStartArgs, PiEvent, PiSessionConfig, ToolFactoryKind, start_in_process,
};
use serde::Serialize;

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

    if !cli.local {
        eprintln!(
            "pi-server-runner: --local is the only supported mode in this milestone; rerun with --local."
        );
        return ExitCode::from(2);
    }

    // BYOK env plumbing happens here, on the still-single-threaded
    // main thread. Reading env vars is safe; `pi-mobile-client` is
    // `#![forbid(unsafe_code)]` so it cannot do this internally.
    let env = ProviderEnv {
        anthropic_key: std::env::var("ANTHROPIC_API_KEY").ok(),
        anthropic_base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
        openai_key: std::env::var("OPENAI_API_KEY").ok(),
        openai_base_url: std::env::var("OPENAI_BASE_URL").ok(),
    };

    let resolved = match resolve_provider(cli.provider.clone(), &env) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("pi-server-runner: {err}");
            return ExitCode::from(2);
        }
    };
    let ResolvedProvider {
        provider,
        api_key,
        base_url,
    } = resolved;

    let prompt_text = match resolve_prompt(&cli) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("pi-server-runner: {err}");
            return ExitCode::from(2);
        }
    };

    let session_config = PiSessionConfig {
        provider,
        model: cli.model.clone(),
        api_key,
        base_url,
        working_directory: std::env::current_dir().ok(),
        append_system_prompt: None,
        max_tool_iterations: None,
        enabled_tools: None,
        tool_factory: Some(ToolFactoryKind::PtyDev),
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

fn emit_line(line: &TranscriptLine) {
    match serde_json::to_string(line) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("pi-server-runner: failed to serialize transcript line: {err}"),
    }
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
    fn no_keys_returns_error() {
        let env = env_with(None, None, None, None);
        let err = resolve_provider(None, &env).expect_err("must error with no creds");
        assert!(err.contains("ANTHROPIC_API_KEY"));
        assert!(err.contains("OPENAI_API_KEY"));
    }
}
