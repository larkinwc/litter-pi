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

    /// Boots the *production* app surface (no `--ui-test-pi-capability-fixture`
    /// flag) and uses the DEBUG-only
    /// `--ui-test-pi-active-runtime-kind <kind>` override added in
    /// `LitterApp.swift::HomeNavigationView.activeServerAgentRuntimeKind`
    /// to synthesize a pi active-server runtime. This proves the
    /// production-side wiring of `.piCapabilityGate(.voice, ...)` on
    /// `HomeVoiceOrbButton` (the `homeVoiceLauncher` overlay), not
    /// just the harness gate.
    ///
    /// Production `InlineVoiceButton` has no caller today
    /// (`HomeVoiceOrbButton` is the lone home-screen mic surface),
    /// and `InlineHandoffView` is only rendered from
    /// `RealtimeVoiceScreen` which requires a live realtime session
    /// to navigate into. Both surfaces apply `.piCapabilityGate(.voice, ...)`
    /// internally, so this test asserts the union of identifiers:
    /// when the active runtime is pi, NONE of the three IDs may
    /// resolve anywhere in the live app accessibility tree.
    @MainActor
    func testProductionAppHidesVoiceAccessibilityIdentifiersWhenActiveRuntimeIsPi() throws {
        let app = XCUIApplication()
        app.launchArguments.append("--ui-test-pi-active-runtime-kind")
        app.launchArguments.append("pi")
        app.launch()

        // The production app may take a moment to settle past splash
        // before the home navigation overlay would render the voice
        // launcher. Polling each identifier for absence covers both
        // the "never rendered" and "rendered then removed" paths.
        // Give the production app a few seconds to finish splash + home
        // dashboard render. We poll for *absence* using direct
        // `.exists` (not `waitForExistence`) so the test does not pay
        // the timeout cost for every identifier.
        let deadline = Date().addingTimeInterval(10)
        while Date() < deadline {
            if !directlyExists(in: app, identifier: "voice.mic.button")
                && !directlyExists(in: app, identifier: "voice.settings.row")
                && !directlyExists(in: app, identifier: "voice.handoff.banner") {
                break
            }
            Thread.sleep(forTimeInterval: 0.25)
        }

        for identifier in ["voice.mic.button", "voice.settings.row", "voice.handoff.banner"] {
            XCTAssertFalse(
                directlyExists(in: app, identifier: identifier),
                "Production app must NOT render \(identifier) when active runtime is pi"
            )
        }
    }

    private func directlyExists(in app: XCUIApplication, identifier: String) -> Bool {
        if app.otherElements[identifier].exists { return true }
        if app.staticTexts[identifier].exists { return true }
        if app.buttons[identifier].exists { return true }
        return false
    }
}
