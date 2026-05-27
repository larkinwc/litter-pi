import SwiftUI

#if DEBUG
/// Test harness that surfaces `RetryTurnView` directly when the app is
/// launched with `--ui-test-pi-retry-turn`. The harness simulates the
/// terminal state `PiTurnState.errored(retryable: true, ..)` so the
/// XCUITest `PiRetryTurnUITests.testBackgroundForegroundShowsRetryWithin10s`
/// can locate the accessibility identifier `pi.turn.retry` without
/// requiring live BYOK credentials or a real pi backend.
///
/// VAL-NFR-003 evidence: the assertion checks the button exists OR the
/// turn completed. This harness exercises the "errored, retryable"
/// branch; the production app drives the same view from the typed
/// `PiEvent.turnStateChanged(state:)` stream surfaced by Rust.
struct PiRetryTurnUITestHarnessView: View {
    static var isEnabled: Bool {
        ProcessInfo.processInfo.arguments.contains("--ui-test-pi-retry-turn")
    }

    /// When set via `--ui-test-pi-retry-turn-delay-ms <N>`, the harness
    /// waits `N` milliseconds after launch before showing the retry
    /// button. The XCUITest backgrounds the app, sleeps 1s, foregrounds
    /// it, and then asserts the button shows up within 10s — the delay
    /// ensures the button appears *after* the foreground transition,
    /// proving the background→foreground cycle did not crash the app.
    private static var revealDelayMs: Int {
        let args = ProcessInfo.processInfo.arguments
        guard let idx = args.firstIndex(of: "--ui-test-pi-retry-turn-delay-ms"),
              idx + 1 < args.count,
              let v = Int(args[idx + 1])
        else {
            return 500
        }
        return max(0, v)
    }

    @State private var showRetry = false
    @State private var lastTapCount = 0

    var body: some View {
        VStack(spacing: 16) {
            Text("PiRetryTurnUITestHarness")
                .font(.system(.title3, design: .monospaced))
                .accessibilityIdentifier("pi.retry.harness.title")

            if showRetry {
                RetryTurnView(
                    message: "Simulated transport drop (test harness)",
                    onRetry: { lastTapCount += 1 }
                )
            } else {
                Text("Waiting for simulated errored state...")
                    .font(.system(.footnote, design: .monospaced))
                    .accessibilityIdentifier("pi.retry.harness.waiting")
            }

            Text("retries=\(lastTapCount)")
                .font(.system(.caption2, design: .monospaced))
                .accessibilityIdentifier("pi.retry.harness.tapCount")
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(Color.black)
        .task {
            let delay = UInt64(Self.revealDelayMs) * 1_000_000
            try? await Task.sleep(nanoseconds: delay)
            showRetry = true
        }
    }
}
#endif
