//! Bridge between pi's asupersync runtime (which lives on a dedicated OS
//! thread) and the tokio-shaped command/event channels exposed by
//! `pi-mobile-client` to the rest of the mobile stack.
//!
//! The runtime thread:
//!   * owns the asupersync `Runtime`,
//!   * holds a `CancelSentinel` whose `Drop` impl flips the caller-provided
//!     `Arc<AtomicBool>` so callers can observe cancellation, and
//!   * pumps inbound `Command`s from a tokio `mpsc` and outbound `PiEvent`s
//!     to a tokio `broadcast`.
//!
//! Nothing outside this module touches asupersync types directly.

use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::server::{Command, PiEvent, PiSessionConfig, ToolFactoryKind};
use crate::tools::pty_dev::PtyDevToolFactory;

/// Process-global counter incremented by the bridge's panic hook.
///
/// Used by the cancel-correctness test to assert the runtime worker
/// never panicked during shutdown. Public so tests in this crate can read
/// it; not re-exported from `lib.rs`.
pub(crate) static PANIC_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Install the bridge's panic hook exactly once per process.
///
/// The hook chains to the previously installed hook (so test output is still
/// emitted) and bumps `PANIC_COUNTER` so callers can detect panics that
/// originated inside the bridge worker thread.
pub(crate) fn install_panic_hook_once() {
    static HOOK_ONCE: Once = Once::new();
    HOOK_ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANIC_COUNTER.fetch_add(1, Ordering::SeqCst);
            prev(info);
        }));
    });
}

/// RAII guard whose `Drop` impl flips a caller-provided `AtomicBool`.
///
/// The bridge spawns a long-running asupersync task that owns one of these.
/// When the runtime is dropped at shutdown the task's future is cancelled,
/// the guard is dropped, and the sentinel flips to `true`. The
/// `shutdown_is_cancel_correct` test inspects the flag to confirm in-flight
/// tasks were actually cancelled rather than allowed to run to completion.
pub(crate) struct CancelSentinel {
    flag: Arc<AtomicBool>,
}

impl CancelSentinel {
    pub(crate) fn new(flag: Arc<AtomicBool>) -> Self {
        Self { flag }
    }
}

impl Drop for CancelSentinel {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

/// Channels held by the runtime worker. The tokio types are intentionally
/// the *only* shapes that cross the OS-thread boundary — pi/asupersync types
/// never escape.
pub(crate) struct RuntimeChannels {
    pub(crate) commands_rx: tokio::sync::mpsc::Receiver<Command>,
    pub(crate) events_tx: tokio::sync::broadcast::Sender<PiEvent>,
    pub(crate) shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    pub(crate) cancel_sentinel: Arc<AtomicBool>,
    pub(crate) session: Option<PiSessionConfig>,
}

/// Async driver for the bridge. Runs inside `Runtime::block_on` on the
/// dedicated OS thread.
///
/// Holds a `CancelSentinel` so the caller-provided flag is flipped when this
/// future is cancelled (i.e. the runtime is dropped at shutdown). Exits
/// cleanly when either:
///   * the shutdown oneshot fires (graceful path), or
///   * the command sender is dropped (handle dropped without explicit signal).
pub(crate) async fn drive(channels: RuntimeChannels) {
    let RuntimeChannels {
        mut commands_rx,
        events_tx,
        shutdown_rx,
        cancel_sentinel,
        session,
    } = channels;

    // Own the sentinel inside the awaited future. If the runtime is dropped
    // while this future is suspended, the guard's `Drop` flips the flag.
    let _sentinel = CancelSentinel::new(cancel_sentinel);

    // Optional pi session: built lazily on the first `Command::Prompt`
    // so failures surface as a `PiEvent::TurnError` rather than a hard
    // panic on startup. If no session config was supplied, the bridge
    // stays in echo mode (the legacy unit-test behavior).
    let mut pi_session: Option<pi::sdk::AgentSessionHandle> = None;

    // Wrap the shutdown receiver in a fused future so we can `select!`-style
    // race it against command reception without a third dependency.
    futures::pin_mut!(shutdown_rx);

    loop {
        // Hand-rolled biased select: shutdown first, then command.
        let recv = commands_rx.recv();
        futures::pin_mut!(recv);
        let next = futures::future::select(recv, std::pin::Pin::new(&mut shutdown_rx)).await;

        match next {
            futures::future::Either::Left((Some(cmd), _)) => {
                handle_command(cmd, &events_tx, session.as_ref(), &mut pi_session).await;
            }
            futures::future::Either::Left((None, _)) => {
                // Sender dropped without signalling shutdown — treat as
                // clean exit (the handle was dropped).
                tracing::debug!(target: "pi_mobile_client::bridge", "command channel closed; exiting");
                break;
            }
            futures::future::Either::Right(_) => {
                tracing::debug!(target: "pi_mobile_client::bridge", "shutdown signal received");
                break;
            }
        }
    }
}

/// Dispatch a single inbound `Command` to its handler.
///
/// In echo mode (`session_config = None`) `Prompt` round-trips as a
/// `PiEvent::PromptReceived` and the bridge does nothing else. With a
/// session config the bridge lazily builds a pi `AgentSession` on the
/// first prompt, runs it through pi's agent loop, and forwards a typed
/// `PiEvent` stream.
async fn handle_command(
    cmd: Command,
    events: &tokio::sync::broadcast::Sender<PiEvent>,
    session_config: Option<&PiSessionConfig>,
    pi_session: &mut Option<pi::sdk::AgentSessionHandle>,
) {
    match cmd {
        Command::Shutdown => {
            let _ = events.send(PiEvent::ShuttingDown);
        }
        Command::Prompt(text) => {
            // Always emit PromptReceived so existing tests/observers
            // can see the prompt enter the runtime.
            let _ = events.send(PiEvent::PromptReceived { text: text.clone() });

            let Some(config) = session_config else {
                // Echo mode: nothing further to do.
                return;
            };

            if pi_session.is_none() {
                match build_pi_session(config).await {
                    Ok(handle) => *pi_session = Some(handle),
                    Err(err) => {
                        let _ = events.send(PiEvent::TurnError {
                            message: format!("pi session init failed: {err}"),
                        });
                        return;
                    }
                }
            }

            let handle = pi_session.as_mut().expect("session built above");
            run_prompt(handle, text, events).await;
        }
    }
}

/// Build a pi `AgentSession` from the `PiSessionConfig` primitives.
///
/// Runs on the asupersync runtime worker thread. Failures are propagated
/// as a typed `pi::sdk::Error` so the caller can surface a `TurnError`.
async fn build_pi_session(
    config: &PiSessionConfig,
) -> pi::sdk::Result<pi::sdk::AgentSessionHandle> {
    use pi::sdk::SessionOptions;
    use std::sync::Arc;

    // BYOK `OPENAI_BASE_URL` handling is performed by the caller
    // (typically `pi-server-runner::main`) before any threads spawn,
    // because Rust 2024 marks `std::env::set_var` as `unsafe` and
    // `pi-mobile-client` is `#![forbid(unsafe_code)]`.
    let _ = &config.base_url; // currently informational; see note above.

    let mut options = SessionOptions {
        provider: config.provider.clone(),
        model: config.model.clone(),
        api_key: config.api_key.clone(),
        working_directory: config.working_directory.clone(),
        append_system_prompt: config.append_system_prompt.clone(),
        enabled_tools: config.enabled_tools.clone(),
        no_session: true,
        ..SessionOptions::default()
    };
    if let Some(iters) = config.max_tool_iterations {
        options.max_tool_iterations = iters;
    }
    if let Some(kind) = config.tool_factory {
        options.tool_factory = match kind {
            ToolFactoryKind::PtyDev => Some(Arc::new(PtyDevToolFactory::new())),
        };
    }

    pi::sdk::create_agent_session(options).await
}

/// Drive a single prompt through pi and forward agent events as
/// `PiEvent`s on the broadcast channel.
async fn run_prompt(
    handle: &mut pi::sdk::AgentSessionHandle,
    prompt: String,
    events: &tokio::sync::broadcast::Sender<PiEvent>,
) {
    use pi::model::AssistantMessageEvent;
    use pi::sdk::{AgentEvent, ContentBlock};

    let events_tx = events.clone();
    let result = handle
        .prompt(prompt, move |event| {
            match event {
                AgentEvent::MessageUpdate {
                    assistant_message_event:
                        AssistantMessageEvent::TextDelta { delta, .. },
                    ..
                } => {
                    let _ = events_tx.send(PiEvent::AssistantTextDelta { delta });
                }
                AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                } => {
                    let args_json = serde_json::to_string(&args).unwrap_or_default();
                    let _ = events_tx.send(PiEvent::ToolExecStart {
                        tool_call_id,
                        tool_name,
                        args_json,
                    });
                }
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    tool_name,
                    result,
                    is_error,
                } => {
                    let result_text = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let _ = events_tx.send(PiEvent::ToolExecEnd {
                        tool_call_id,
                        tool_name,
                        result_text,
                        is_error,
                    });
                }
                _ => {}
            }
        })
        .await;

    match result {
        Ok(message) => {
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                let _ = events.send(PiEvent::AssistantText { text });
            }
            let _ = events.send(PiEvent::TurnComplete);
        }
        Err(err) => {
            let _ = events.send(PiEvent::TurnError {
                message: err.to_string(),
            });
        }
    }
}
