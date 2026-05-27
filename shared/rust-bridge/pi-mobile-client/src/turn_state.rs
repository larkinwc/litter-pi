//! Typed runtime state for an in-flight pi turn.
//!
//! This module exposes [`PiTurnState`], a small finite-state machine
//! shape the rest of the mobile stack (and iOS/Android UI through the
//! `codex-mobile-client` UniFFI surface) uses to decide whether to show
//! a retry affordance when a turn fails.
//!
//! Per the validation contract VAL-NFR-003, the iOS / Android UI must
//! render a `RetryTurnView` / `RetryTurnRow` with accessibility id
//! `pi.turn.retry` when the runtime transitions to
//! `PiTurnState::Errored { retryable: true, .. }`. Non-retryable error
//! classes (401/403, invalid API key) MUST NOT surface that button:
//! retrying with the same credentials would just hit the same wall.
//!
//! The classification of an error message lives here so it stays
//! consistent across the in-process runtime (`runtime_bridge::drive`)
//! and the SSH remote driver in `pi-server-runner`.

/// Public state of an in-flight pi turn.
///
/// Kept intentionally narrow:
///   * `Idle` — no turn is in flight.
///   * `Streaming` — a prompt has been accepted and the agent loop is
///     producing tokens / tool calls.
///   * `Completed` — the most recent turn ended cleanly.
///   * `Errored { retryable, message }` — the most recent turn failed;
///     `retryable == true` indicates the UI may surface a retry button
///     (e.g. transport drop, idle disconnect, write-half EAGAIN).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiTurnState {
    Idle,
    Streaming,
    Completed,
    Errored { retryable: bool, message: String },
}

impl PiTurnState {
    /// Build an `Errored` state from a raw error message, deciding the
    /// `retryable` flag by inspecting the message for transport-drop
    /// indicators.
    ///
    /// Retryable signals (case-insensitive substring match):
    ///   * `would block`            — `std::io::ErrorKind::WouldBlock`
    ///   * `wouldblock`             — same, debug-formatted
    ///   * `connection reset`       — TCP RST mid-stream
    ///   * `broken pipe`            — write half closed mid-stream
    ///   * `connection aborted`     — local socket teardown mid-stream
    ///   * `eof`                    — half-open transport drop
    ///   * `transport drop`/`transport dropped`
    ///   * `disconnected`           — pi/codex ACP transport disconnect
    ///   * `timed out`/`timeout`    — network idle timeout (mid-turn)
    ///
    /// Non-retryable signals override the above — if the message
    /// matches a non-retryable class (401, 403, invalid_api_key,
    /// authentication failed) we report `retryable: false` even if a
    /// transport keyword is also present, because the auth failure is
    /// the dominant cause.
    pub fn errored_from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        let retryable = classify_retryable(&message);
        PiTurnState::Errored { retryable, message }
    }
}

/// Decide whether `message` describes a retryable transport-class
/// failure or a definitive non-retryable failure (auth / permission).
///
/// Returns `true` for transport drops, `false` for auth-class errors.
/// Defaults to `false` for unknown failure shapes so callers do not
/// invite an infinite retry loop on something they cannot classify.
pub fn classify_retryable(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();

    // Non-retryable wins if the message looks like an auth/permission
    // class failure. Retrying with the same credentials would just hit
    // the same wall, so we must NOT show the retry button.
    if is_non_retryable(&lower) {
        return false;
    }
    if is_transport_drop(&lower) {
        return true;
    }
    false
}

fn is_non_retryable(lower: &str) -> bool {
    // HTTP status indicators from upstream provider errors. We match
    // both the bare code and common surrounding phrasing so a message
    // like "HTTP 401 Unauthorized" or "status: 403" is caught.
    const NON_RETRYABLE_TOKENS: &[&str] = &[
        " 401",
        "(401",
        "[401",
        "401 ",
        " 403",
        "(403",
        "[403",
        "403 ",
        "unauthorized",
        "forbidden",
        "invalid_api_key",
        "invalid api key",
        "authentication failed",
        "authentication_error",
        "permission denied",
    ];
    NON_RETRYABLE_TOKENS.iter().any(|tok| lower.contains(tok))
}

fn is_transport_drop(lower: &str) -> bool {
    const RETRYABLE_TOKENS: &[&str] = &[
        "would block",
        "wouldblock",
        "connection reset",
        "broken pipe",
        "connection aborted",
        "transport drop",
        "transport dropped",
        "disconnected",
        "timed out",
        "timeout",
        "eof",
    ];
    RETRYABLE_TOKENS.iter().any(|tok| lower.contains(tok))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Transition-matrix coverage. Each case asserts the classifier
    /// returns the right `retryable` flag for the canonical wording
    /// produced by either the in-process runtime or the SSH remote
    /// driver.
    #[test]
    fn classify_transport_drops_as_retryable() {
        for msg in [
            "io error: would block",
            "WouldBlock",
            "connection reset by peer",
            "Broken pipe",
            "connection aborted",
            "transport drop",
            "remote pi acp disconnected: stream closed",
            "session/prompt transport: timed out",
            "session/prompt transport: timeout waiting for response",
            "stream returned eof mid-turn",
        ] {
            assert!(
                classify_retryable(msg),
                "expected {msg:?} to classify as retryable transport drop"
            );
        }
    }

    #[test]
    fn classify_auth_failures_as_non_retryable() {
        for msg in [
            "pi session init failed: HTTP 401 Unauthorized",
            "session/prompt server: unauthorized (401)",
            "session/prompt server: forbidden (403)",
            "authentication_error: invalid_api_key",
            "Authentication failed",
            "permission denied",
        ] {
            assert!(
                !classify_retryable(msg),
                "expected {msg:?} to classify as non-retryable auth/permission error"
            );
        }
    }

    /// If a message smells like *both* a transport drop and an auth
    /// failure, the auth signal wins. Retrying a 401 with the same
    /// credentials would just hit the same wall.
    #[test]
    fn auth_signal_wins_over_transport_signal() {
        let msg = "connection reset after HTTP 401 from upstream";
        assert!(
            !classify_retryable(msg),
            "auth signal must dominate transport signal; got retryable=true for {msg:?}"
        );
    }

    /// Unknown failure shapes default to non-retryable. We do not want
    /// the UI to invite an infinite retry loop on an error we cannot
    /// classify.
    #[test]
    fn unknown_errors_default_to_non_retryable() {
        assert!(!classify_retryable("some opaque internal error"));
        assert!(!classify_retryable(""));
    }

    #[test]
    fn errored_from_message_packages_retryable_and_message() {
        let s = PiTurnState::errored_from_message("connection reset by peer");
        match s {
            PiTurnState::Errored { retryable, message } => {
                assert!(retryable);
                assert_eq!(message, "connection reset by peer");
            }
            other => panic!("expected Errored variant, got {other:?}"),
        }

        let s = PiTurnState::errored_from_message("HTTP 401 Unauthorized");
        match s {
            PiTurnState::Errored { retryable, message } => {
                assert!(!retryable);
                assert_eq!(message, "HTTP 401 Unauthorized");
            }
            other => panic!("expected Errored variant, got {other:?}"),
        }
    }

    /// State equality smoke-test so consumers can compare typed
    /// states directly (used by the codex-mobile-client UniFFI
    /// translator unit tests).
    #[test]
    fn state_equality_covers_all_variants() {
        assert_eq!(PiTurnState::Idle, PiTurnState::Idle);
        assert_eq!(PiTurnState::Streaming, PiTurnState::Streaming);
        assert_eq!(PiTurnState::Completed, PiTurnState::Completed);
        assert_eq!(
            PiTurnState::Errored {
                retryable: true,
                message: "x".into()
            },
            PiTurnState::Errored {
                retryable: true,
                message: "x".into()
            }
        );
        assert_ne!(
            PiTurnState::Errored {
                retryable: true,
                message: "x".into()
            },
            PiTurnState::Errored {
                retryable: false,
                message: "x".into()
            }
        );
    }
}
