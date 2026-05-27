import SwiftUI

/// `RetryTurnView` is the inline UI surfaced when the active pi turn
/// transitions to `PiTurnState.errored(retryable: true, ..)`.
///
/// Per VAL-NFR-003, the contained button MUST carry the accessibility
/// identifier `pi.turn.retry` so the simulator harness (and any
/// background→foreground assertion) can locate and tap it. The retry
/// action re-fires the last user prompt through the same
/// `RustPiRuntimeBridge.sendPrompt` path the composer uses.
struct RetryTurnView: View {
    /// Human-readable description surfaced from
    /// `PiTurnState.errored(message:)`. Rendered above the button so
    /// the user has context for the retry.
    let message: String
    /// Closure invoked when the user taps the retry button. The
    /// caller is responsible for re-firing the last user prompt
    /// against the active pi server.
    let onRetry: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Turn errored")
                .font(.system(.headline, design: .monospaced))
                .foregroundColor(LitterTheme.danger)
            if !message.isEmpty {
                Text(message)
                    .font(.system(.footnote, design: .monospaced))
                    .foregroundColor(LitterTheme.textSecondary)
                    .accessibilityIdentifier("pi.turn.retry.message")
            }
            Button(action: onRetry) {
                Text("Retry")
                    .font(.system(.body, design: .monospaced))
                    .padding(.horizontal, 16)
                    .padding(.vertical, 8)
                    .background(LitterTheme.accent.opacity(0.18))
                    .foregroundColor(LitterTheme.accent)
                    .cornerRadius(6)
            }
            .accessibilityIdentifier("pi.turn.retry")
            .accessibilityLabel("Retry pi turn")
        }
        .padding(12)
        .background(Color.black)
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .stroke(LitterTheme.danger.opacity(0.6), lineWidth: 1)
        )
        .accessibilityIdentifier("pi.turn.retry.container")
    }
}

/// Render `RetryTurnView` only when `state` is a retryable error.
/// Non-retryable errors (401/403) intentionally render no retry
/// affordance per VAL-NFR-003.
struct PiRetryTurnGate: View {
    let state: PiTurnState
    let onRetry: () -> Void

    var body: some View {
        switch state {
        case .errored(let retryable, let message) where retryable:
            RetryTurnView(message: message, onRetry: onRetry)
        default:
            EmptyView()
        }
    }
}
