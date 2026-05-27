import SwiftUI

#if DEBUG
/// Test harness that exercises the pi-capability gating contract end
/// to end (VAL-NFR-005). The host app is launched with
/// `--ui-test-pi-capability-fixture` plus an explicit voice mode flag:
///
///   * `--voice-false` simulates the pi manifest reporting
///     `voice = false`. The three voice-related affordances
///     (`voice.mic.button`, `voice.settings.row`,
///     `voice.handoff.banner`) are gated through
///     [`PiCapabilityGates.showsVoice`] using the canonical pi
///     runtime kind and therefore must not appear in the accessibility
///     tree. The XCUITest asserts `.exists == false` for each
///     identifier.
///
///   * `--voice-true` flips the fixture to a runtime kind whose
///     `showsVoice(...)` returns `true` so the same harness renders
///     all three elements. The same XCUITest covers this branch to
///     prove the gate is the only thing flipping the elements off.
///
/// Production uses the same `PiCapabilityGates` helper, so this
/// harness is a direct contract test for that gate.
struct PiCapabilityFixtureUITestHarnessView: View {
    static var isEnabled: Bool {
        ProcessInfo.processInfo.arguments.contains("--ui-test-pi-capability-fixture")
    }

    /// Resolve the runtime-kind to pass into the gate from the
    /// launch arguments. Defaults to pi (`voice = false`).
    private static var agentRuntimeKind: String {
        let args = ProcessInfo.processInfo.arguments
        if args.contains("--voice-true") {
            return "codex"
        }
        return "pi"
    }

    private var kind: String { Self.agentRuntimeKind }

    var body: some View {
        VStack(spacing: 16) {
            Text("PiCapabilityFixtureUITestHarness")
                .font(.system(.title3, design: .monospaced))
                .accessibilityIdentifier("pi.capability.harness.title")

            // voice.mic.button — gated identically to the production
            // `InlineVoiceButton` / `HomeVoiceOrbButton` accessibility
            // identifier.
            Button(action: {}) {
                Image(systemName: "waveform.and.mic")
                    .font(.system(size: 18, weight: .semibold))
                    .frame(width: 44, height: 44)
            }
            .accessibilityIdentifier("voice.mic.button")
            .piCapabilityGate(.voice, agentRuntimeKind: kind)

            // voice.settings.row — a row that links to the realtime
            // voice settings. Gated through the same helper used by
            // `SettingsView` rows that surface voice features.
            HStack(spacing: 10) {
                Image(systemName: "waveform")
                Text("Realtime voice")
            }
            .padding(8)
            .accessibilityIdentifier("voice.settings.row")
            .piCapabilityGate(.voice, agentRuntimeKind: kind)

            // voice.handoff.banner — banner the realtime voice screen
            // surfaces while a handoff is in flight.
            Text("Handing off…")
                .font(.system(.caption, design: .monospaced))
                .padding(8)
                .accessibilityIdentifier("voice.handoff.banner")
                .piCapabilityGate(.voice, agentRuntimeKind: kind)

            Text("kind=\(kind)")
                .font(.system(.caption2, design: .monospaced))
                .accessibilityIdentifier("pi.capability.harness.kind")
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(Color.black)
    }
}
#endif
