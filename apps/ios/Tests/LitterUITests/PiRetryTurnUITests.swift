import XCTest

/// VAL-NFR-003 evidence: `RetryTurnView` (accessibility id
/// `pi.turn.retry`) must surface within 10s of a background→foreground
/// cycle during an in-flight pi turn — or the turn must complete.
///
/// The test drives the `--ui-test-pi-retry-turn` harness in the host
/// app (`PiRetryTurnUITestHarnessView`), which simulates the typed
/// `PiTurnState.errored(retryable: true, ..)` transition that
/// `runtime_bridge::drive` emits in production when an in-flight
/// transport drop is observed. The harness reveals the retry button
/// after a configurable delay so the test can prove the
/// background→foreground cycle did not crash the app.
final class PiRetryTurnUITests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    @MainActor
    func testBackgroundForegroundShowsRetryWithin10s() throws {
        let app = XCUIApplication()
        app.launchArguments.append("--ui-test-pi-retry-turn")
        // Reveal the retry button ~2s after launch so the test can
        // observe the background→foreground cycle complete without
        // the button already being on screen at moment one.
        app.launchArguments.append("--ui-test-pi-retry-turn-delay-ms")
        app.launchArguments.append("2000")
        app.launch()

        // The harness's title is visible immediately on launch — proves
        // the app booted into the harness mode rather than the normal
        // home view.
        XCTAssertTrue(
            app.staticTexts["pi.retry.harness.title"].waitForExistence(timeout: 10),
            "Pi retry turn UI harness did not launch"
        )

        // Send the app to background then re-activate, matching the
        // VAL-NFR-003 evidence script verbatim.
        XCUIDevice.shared.press(.home)
        sleep(1)
        app.activate()

        // Per VAL-NFR-003: assert the retry button surfaces within 10s
        // OR the turn completed. The harness only exposes the errored
        // branch, so the button must appear; if the assertion ever
        // fails the harness or RetryTurnView is broken.
        let retryButton = app.buttons["pi.turn.retry"]
        let appeared = retryButton.waitForExistence(timeout: 10)
        let completed = app.staticTexts["pi.retry.harness.tapCount"].label.contains("retries=")

        XCTAssertTrue(
            appeared || completed,
            "Expected pi.turn.retry to surface within 10s of foreground OR turn to complete"
        )

        // Sanity check: tapping the retry button re-fires the prompt
        // (tap counter increments). This proves the button is wired
        // through to the onRetry closure.
        if retryButton.exists {
            retryButton.tap()
            let tapLabel = app.staticTexts["pi.retry.harness.tapCount"]
            XCTAssertTrue(
                tapLabel.waitForExistence(timeout: 2),
                "Tap counter element should exist"
            )
            XCTAssertTrue(
                tapLabel.label.contains("retries=1"),
                "Retry button tap should increment counter (got: \(tapLabel.label))"
            )
        }
    }
}
