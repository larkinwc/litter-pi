//! Typed UniFFI surface for the in-process pi runtime.
//!
//! Previously pi runtime events were forwarded into the unified
//! `ServerEvent::LegacyNotification` channel as stringly-typed JSON
//! payloads (`"pi/promptReceived"`, `"pi/turnComplete"`, ...). That
//! violated the drift guardrail "do not parse upstream wire-format
//! strings in Swift/Kotlin" — Swift would have to inspect the method
//! name and decode `serde_json::Value` to render anything useful.
//!
//! This module introduces a typed `PiEvent` enum that mirrors the
//! `pi_mobile_client::PiEvent` variants the iOS UI needs plus a
//! `PiEventListener` callback trait so platforms can observe the
//! stream as typed values. The `AppClient` exposes
//! `subscribe_pi_events` / `send_pi_prompt` so direct prompt
//! input + typed event observation no longer have to round-trip
//! through `ServerEvent`.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

/// Typed pi runtime event surface for Swift/Kotlin consumers.
///
/// Mirrors the `pi_mobile_client::PiEvent` variants the iOS UI needs.
/// Kept as a UniFFI-safe enum (only owned primitive/String fields) so
/// no upstream pi or serde types leak across the FFI boundary.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PiEvent {
    /// Emitted as soon as the runtime accepts a prompt. Carries the
    /// raw user text so the UI can echo it locally without keeping
    /// its own copy of the in-flight prompt.
    PromptReceived { text: String },
    /// Streaming partial assistant text. The UI accumulates these to
    /// render incremental output.
    AssistantTextDelta { delta: String },
    /// Final assistant text for the turn (the full content, sent once
    /// the turn is finalized).
    AssistantText { text: String },
    /// A tool execution started. `args_json` is the upstream tool
    /// arguments serialized as JSON — Swift treats it as an opaque
    /// blob (no parsing) and only displays it for debug surfaces.
    ToolExecStarted {
        tool_call_id: String,
        tool_name: String,
        args_json: String,
    },
    /// A tool execution finished.
    ToolExecOutput {
        tool_call_id: String,
        tool_name: String,
        result_text: String,
        is_error: bool,
    },
    /// The agent turn completed cleanly.
    TurnComplete,
    /// The agent turn failed; carries a human-readable error message.
    Error { message: String },
    /// Emitted in response to an explicit `Command::Shutdown` (or when
    /// the runtime is being torn down by drop). Useful for the UI to
    /// gate further `send_pi_prompt` calls.
    ShuttingDown,
}

impl PiEvent {
    /// Translate a `pi_mobile_client::PiEvent` into the typed UniFFI
    /// surface. Kept in a dedicated helper so the connection worker
    /// in `session::connection` only deals in typed `PiEvent`s.
    pub(crate) fn from_pi(event: pi_mobile_client::PiEvent) -> Self {
        use pi_mobile_client::PiEvent as P;
        match event {
            P::PromptReceived { text } => PiEvent::PromptReceived { text },
            P::AssistantTextDelta { delta } => PiEvent::AssistantTextDelta { delta },
            P::AssistantText { text } => PiEvent::AssistantText { text },
            P::ToolExecStart {
                tool_call_id,
                tool_name,
                args_json,
            } => PiEvent::ToolExecStarted {
                tool_call_id,
                tool_name,
                args_json,
            },
            P::ToolExecEnd {
                tool_call_id,
                tool_name,
                result_text,
                is_error,
            } => PiEvent::ToolExecOutput {
                tool_call_id,
                tool_name,
                result_text,
                is_error,
            },
            P::TurnComplete => PiEvent::TurnComplete,
            P::TurnError { message } => PiEvent::Error { message },
            P::ShuttingDown => PiEvent::ShuttingDown,
        }
    }
}

/// UniFFI callback trait that Swift/Kotlin implements to observe the
/// typed pi event stream.
#[uniffi::export(callback_interface)]
pub trait PiEventListener: Send + Sync {
    fn on_event(&self, event: PiEvent);
}

/// Errors from the pi-runtime UniFFI surface.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PiRuntimeError {
    #[error("No pi runtime is registered for server_id={server_id}")]
    NoPiRuntime { server_id: String },
    #[error("Pi runtime command channel is full")]
    ChannelFull,
    #[error("Pi runtime is shutting down")]
    Closed,
}

/// Subscription handle returned to Swift/Kotlin from
/// `AppClient.subscribe_pi_events`. Dropping the handle aborts the
/// background pump task that drains the broadcast receiver and
/// forwards events to the listener, so the listener stops being
/// invoked immediately.
#[derive(uniffi::Object)]
pub struct PiEventSubscription {
    // Held as an `Option` so `Drop` can `take()` and abort the task.
    handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[uniffi::export]
impl PiEventSubscription {
    /// Cancel the subscription. Idempotent. The owning Swift/Kotlin
    /// caller can either call this explicitly or just drop the handle.
    pub fn cancel(&self) {
        if let Ok(mut guard) = self.handle.lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }
}

impl Drop for PiEventSubscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Spawn a background task that drains `events_rx` and forwards every
/// event to `listener`. Returns a `PiEventSubscription` that owns the
/// task and aborts it on drop.
pub(crate) fn spawn_pi_event_pump(
    runtime: &Arc<tokio::runtime::Runtime>,
    mut events_rx: broadcast::Receiver<PiEvent>,
    listener: Box<dyn PiEventListener>,
) -> Arc<PiEventSubscription> {
    let listener: Arc<dyn PiEventListener> = Arc::from(listener);
    let join = runtime.spawn(async move {
        loop {
            match events_rx.recv().await {
                Ok(event) => listener.on_event(event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!("pi event listener lagged, skipped {skipped} events");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!("pi event broadcast closed; pump exiting");
                    break;
                }
            }
        }
    });
    Arc::new(PiEventSubscription {
        handle: std::sync::Mutex::new(Some(join)),
    })
}

/// Per-session pi runtime control surface stored on `ServerSession`.
///
/// Holds the captured `Command` sender from
/// `pi_mobile_client::PiInProcessHandle::commands_tx` (cloned into the
/// session worker) plus the broadcast sender that fans typed
/// `PiEvent`s out to any number of UniFFI subscribers.
pub(crate) struct PiSessionChannels {
    pub(crate) commands_tx: mpsc::Sender<pi_mobile_client::Command>,
    pub(crate) events_tx: broadcast::Sender<PiEvent>,
}

impl PiSessionChannels {
    pub(crate) fn new(
        commands_tx: mpsc::Sender<pi_mobile_client::Command>,
        events_tx: broadcast::Sender<PiEvent>,
    ) -> Self {
        Self {
            commands_tx,
            events_tx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Capturing `PiEventListener` used by the pump test. Records the
    /// typed events the pump forwards so the test can assert
    /// translation + ordering without touching a real pi session.
    struct CapturingListener {
        events: Arc<Mutex<Vec<PiEvent>>>,
    }

    impl PiEventListener for CapturingListener {
        fn on_event(&self, event: PiEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    /// Drive a typed PiEvent broadcast directly into the pump and
    /// confirm: (1) `PiEvent::from_pi` translation, (2) the pump
    /// forwards every typed event to the listener, and (3) on a
    /// `TurnComplete` the listener observes the terminal event before
    /// the pump task is aborted.
    #[test]
    fn pump_forwards_typed_events_through_listener() {
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("multi-thread tokio runtime"),
        );
        let (tx, _) = broadcast::channel::<PiEvent>(16);
        let events = Arc::new(Mutex::new(Vec::<PiEvent>::new()));
        let listener = Box::new(CapturingListener {
            events: Arc::clone(&events),
        });
        let rx = tx.subscribe();
        let _sub = spawn_pi_event_pump(&rt, rx, listener);

        // Translate a synthetic upstream pi event stream into typed
        // PiEvents and broadcast each one.
        let stream = vec![
            pi_mobile_client::PiEvent::PromptReceived {
                text: "say hi".to_string(),
            },
            pi_mobile_client::PiEvent::AssistantText {
                text: "hi".to_string(),
            },
            pi_mobile_client::PiEvent::TurnComplete,
        ];
        for raw in stream {
            tx.send(PiEvent::from_pi(raw)).expect("listener present");
        }

        // The pump runs on the shared runtime; give it a moment to
        // drain the broadcast.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let captured = events.lock().unwrap().clone();
        assert_eq!(
            captured.len(),
            3,
            "listener should observe 3 typed events, got {captured:?}"
        );
        assert!(
            matches!(captured[0], PiEvent::PromptReceived { ref text } if text == "say hi"),
            "first event must be PromptReceived"
        );
        assert!(
            matches!(captured[1], PiEvent::AssistantText { ref text } if text == "hi"),
            "second event must be AssistantText"
        );
        assert!(
            matches!(captured[2], PiEvent::TurnComplete),
            "third event must be TurnComplete"
        );
    }
}
