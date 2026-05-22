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

use crate::server::{Command, PiEvent};

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
    } = channels;

    // Own the sentinel inside the awaited future. If the runtime is dropped
    // while this future is suspended, the guard's `Drop` flips the flag.
    let _sentinel = CancelSentinel::new(cancel_sentinel);

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
                handle_command(cmd, &events_tx).await;
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
/// Today the bridge only knows about `Shutdown` (handled by the caller's
/// `Drop`) and `Prompt` (placeholder until the agent-loop feature lands).
/// Unknown commands are logged and dropped.
async fn handle_command(cmd: Command, events: &tokio::sync::broadcast::Sender<PiEvent>) {
    match cmd {
        Command::Shutdown => {
            // Best-effort notification; receivers may have hung up.
            let _ = events.send(PiEvent::ShuttingDown);
        }
        Command::Prompt(text) => {
            // The agent loop is wired in a later feature; for now we just
            // echo back a placeholder event so the channel plumbing is
            // observable from tests.
            let _ = events.send(PiEvent::PromptReceived { text });
        }
    }
}
