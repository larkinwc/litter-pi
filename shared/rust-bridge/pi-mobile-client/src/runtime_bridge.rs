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
use crate::tools::ish::IshToolFactory;
use crate::tools::pty_dev::PtyDevToolFactory;
use crate::turn_state::PiTurnState;

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
            // Transition Idle/Completed/Errored -> Streaming.
            let _ = events.send(PiEvent::TurnStateChanged {
                state: PiTurnState::Streaming,
            });

            let Some(config) = session_config else {
                // Echo mode: nothing further to do. Echo-mode prompts
                // are still considered Completed so observers see a
                // terminal state transition.
                let _ = events.send(PiEvent::TurnStateChanged {
                    state: PiTurnState::Completed,
                });
                return;
            };

            if pi_session.is_none() {
                match build_pi_session(config).await {
                    Ok(handle) => *pi_session = Some(handle),
                    Err(err) => {
                        let message = format!("pi session init failed: {err}");
                        let _ = events.send(PiEvent::TurnError {
                            message: message.clone(),
                        });
                        let _ = events.send(PiEvent::TurnStateChanged {
                            state: PiTurnState::errored_from_message(message),
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

    // BYOK base-URL override is applied directly onto pi's
    // `SessionOptions.base_url`. pi's `create_agent_session` patches the
    // resolved model entry before the provider implementation is built,
    // so callers no longer need to mutate `ANTHROPIC_BASE_URL` /
    // `OPENAI_BASE_URL` via `unsafe { env::set_var(...) }` for the
    // override to reach the provider.
    //
    // We still surface a runtime warning if a base URL was supplied
    // without an API key so the failure mode is observable in logs
    // instead of silently dropping the override.
    if config.base_url.is_some() && config.api_key.is_none() {
        tracing::warn!(
            target: "pi_mobile_client::bridge",
            "BYOK base_url set but no API key supplied; pi will use the env-resolved provider auth"
        );
    }

    let mut options = SessionOptions {
        provider: config.provider.clone(),
        model: config.model.clone(),
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        working_directory: config.working_directory.clone(),
        append_system_prompt: config.append_system_prompt.clone(),
        enabled_tools: config.enabled_tools.clone(),
        no_session: true,
        ..SessionOptions::default()
    };
    if let Some(iters) = config.max_tool_iterations {
        options.max_tool_iterations = iters;
    }
    if let Some(kind) = config.tool_factory.clone() {
        options.tool_factory = match kind {
            ToolFactoryKind::PtyDev => Some(Arc::new(PtyDevToolFactory::new())),
            ToolFactoryKind::Ish(exec) => Some(Arc::new(IshToolFactory::new(exec))),
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
            let _ = events.send(PiEvent::TurnStateChanged {
                state: PiTurnState::Completed,
            });
        }
        Err(err) => {
            let message = err.to_string();
            let _ = events.send(PiEvent::TurnError {
                message: message.clone(),
            });
            let _ = events.send(PiEvent::TurnStateChanged {
                state: PiTurnState::errored_from_message(message),
            });
        }
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::PiSessionConfig;

    fn run_async<F>(future: F) -> F::Output
    where
        F: std::future::Future,
    {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("asupersync runtime");
        runtime.block_on(future)
    }

    /// VAL-IOS-PI-013 regression: `PiSessionConfig.base_url` must reach
    /// the in-process pi provider. We build a session against the
    /// Anthropic provider with a proxy URL and a dummy API key, then
    /// confirm pi's `AgentSessionHandle::provider_base_url()` reports
    /// the proxy URL — proving the override was applied to the
    /// `ModelEntry` before the `AnthropicProvider` was constructed,
    /// rather than being silently dropped as it was previously.
    #[test]
    fn build_pi_session_applies_base_url_to_anthropic_provider() {
        let tmp = std::env::temp_dir();
        let config = PiSessionConfig {
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-5-20251101".to_string()),
            api_key: Some("sk-ant-dummy-for-test".to_string()),
            base_url: Some("https://proxy.example/v1".to_string()),
            working_directory: Some(tmp.clone()),
            append_system_prompt: None,
            max_tool_iterations: None,
            enabled_tools: None,
            tool_factory: None,
        };

        let handle = run_async(build_pi_session(&config)).expect("build pi session");
        assert_eq!(
            handle.provider_base_url(),
            "https://proxy.example/v1",
            "PiSessionConfig.base_url must reach the active provider; got `{}`",
            handle.provider_base_url()
        );
    }

    /// VAL-NFR-003 regression: drive a prompt through echo mode and
    /// confirm the typed `TurnStateChanged` events fire in the right
    /// order (Streaming → Completed). The bridge in echo mode does not
    /// require a real pi session, so this exercise stays hermetic.
    #[test]
    fn echo_mode_emits_streaming_then_completed_state_transitions() {
        use crate::server::{Command, PiEvent};
        use crate::turn_state::PiTurnState;
        use std::time::Duration;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("tokio runtime");

        let (commands_tx, commands_rx) = tokio::sync::mpsc::channel::<Command>(4);
        let (events_tx, mut events_rx) = tokio::sync::broadcast::channel::<PiEvent>(16);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let channels = RuntimeChannels {
            commands_rx,
            events_tx: events_tx.clone(),
            shutdown_rx,
            cancel_sentinel: cancel,
            session: None,
        };

        rt.block_on(async move {
            let drive_handle = tokio::spawn(drive(channels));
            commands_tx
                .send(Command::Prompt("ping".into()))
                .await
                .expect("send prompt");

            // Collect first three events with a bounded timeout.
            let mut received = Vec::new();
            for _ in 0..3 {
                let next = tokio::time::timeout(Duration::from_secs(2), events_rx.recv())
                    .await
                    .expect("event arrived")
                    .expect("broadcast open");
                received.push(next);
            }

            let _ = shutdown_tx.send(());
            drop(commands_tx);
            let _ = tokio::time::timeout(Duration::from_secs(2), drive_handle).await;

            assert!(
                matches!(received[0], PiEvent::PromptReceived { ref text } if text == "ping"),
                "first event must be PromptReceived, got {:?}",
                received[0]
            );
            assert!(
                matches!(
                    received[1],
                    PiEvent::TurnStateChanged {
                        state: PiTurnState::Streaming
                    }
                ),
                "second event must be TurnStateChanged(Streaming), got {:?}",
                received[1]
            );
            assert!(
                matches!(
                    received[2],
                    PiEvent::TurnStateChanged {
                        state: PiTurnState::Completed
                    }
                ),
                "third event must be TurnStateChanged(Completed), got {:?}",
                received[2]
            );
        });
    }

    /// Companion check: an OpenAI-compatible provider should also pick
    /// up the configured proxy URL via the same path. We do not assert
    /// the exact provider class — only that the URL the provider was
    /// built against matches what the caller asked for.
    #[test]
    fn build_pi_session_applies_base_url_to_openai_provider() {
        let tmp = std::env::temp_dir();
        let config = PiSessionConfig {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            api_key: Some("sk-oa-dummy-for-test".to_string()),
            base_url: Some("https://oa-proxy.example/v1".to_string()),
            working_directory: Some(tmp.clone()),
            append_system_prompt: None,
            max_tool_iterations: None,
            enabled_tools: None,
            tool_factory: None,
        };

        let handle = run_async(build_pi_session(&config)).expect("build pi session");
        assert_eq!(handle.provider_base_url(), "https://oa-proxy.example/v1");
    }
}
