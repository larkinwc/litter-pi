import XCTest

/// VAL-NFR-005 evidence: when the pi manifest reports `voice = false`,
/// the iOS UI must hide every voice affordance addressable via the
/// accessibility identifiers `voice.mic.button`, `voice.settings.row`,
/// and `voice.handoff.banner`.
///
/// The harness `PiCapabilityFixtureUITestHarnessView` renders the
/// three accessibility-identified elements behind
/// `PiCapabilityGates.showsVoice(for:)` — the same gate the
/// production composers/settings/handoff views consume. Driving the
/// fixture with `--voice-false` flips the gate off; `--voice-true`
/// flips it on. The first case is the contract assertion; the second
/// case is a control proving the gate is the only thing flipping the
/// elements off (so a passing `voice-false` line cannot just mean the
/// harness never rendered anything).
final class PiCapabilityGatesUITests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    @MainActor
    func testVoiceCapabilityFalseHidesAllVoiceAccessibilityIdentifiers() throws {
        let app = XCUIApplication()
        app.launchArguments.append("--ui-test-pi-capability-fixture")
        app.launchArguments.append("--voice-false")
        app.launch()

        XCTAssertTrue(
            app.staticTexts["pi.capability.harness.title"].waitForExistence(timeout: 10),
            "Pi capability fixture harness did not launch"
        )
        XCTAssertEqual(
            app.staticTexts["pi.capability.harness.kind"].label,
            "kind=pi",
            "Harness must run against the pi runtime kind for the voice=false branch"
        )

        // Each accessibility identifier listed in VAL-NFR-005 must
        // resolve to a non-existent element when the capability gate
        // reports voice = false.
        for identifier in ["voice.mic.button", "voice.settings.row", "voice.handoff.banner"] {
            XCTAssertFalse(
                app.buttons[identifier].exists,
                "\(identifier) must NOT be present when voice = false (found as button)"
            )
            XCTAssertFalse(
                app.otherElements[identifier].exists,
                "\(identifier) must NOT be present when voice = false (found as element)"
            )
            XCTAssertFalse(
                app.staticTexts[identifier].exists,
                "\(identifier) must NOT be present when voice = false (found as text)"
            )
        }
    }

    @MainActor
    func testVoiceCapabilityTrueRevealsAllVoiceAccessibilityIdentifiers() throws {
        let app = XCUIApplication()
        app.launchArguments.append("--ui-test-pi-capability-fixture")
        app.launchArguments.append("--voice-true")
        app.launch()

        XCTAssertTrue(
            app.staticTexts["pi.capability.harness.title"].waitForExistence(timeout: 10),
            "Pi capability fixture harness did not launch"
        )
        XCTAssertEqual(
            app.staticTexts["pi.capability.harness.kind"].label,
            "kind=codex",
            "Harness must run against a non-pi runtime kind for the control case"
        )

        // The mic button is exposed as a UIButton in the harness; the
        // settings row and handoff banner are non-button containers.
        // Asserting that the elements resolve under *some* element
        // type proves the gate flipped them on rather than hiding
        // them outright.
        XCTAssertTrue(
            app.buttons["voice.mic.button"].waitForExistence(timeout: 5),
            "voice.mic.button must be present when voice = true"
        )
        XCTAssertTrue(
            elementExists(in: app, identifier: "voice.settings.row"),
            "voice.settings.row must be present when voice = true"
        )
        XCTAssertTrue(
            elementExists(in: app, identifier: "voice.handoff.banner"),
            "voice.handoff.banner must be present when voice = true"
        )
    }

    private func elementExists(in app: XCUIApplication, identifier: String) -> Bool {
        if app.otherElements[identifier].waitForExistence(timeout: 5) { return true }
        if app.staticTexts[identifier].exists { return true }
        if app.buttons[identifier].exists { return true }
        return false
    }
}
