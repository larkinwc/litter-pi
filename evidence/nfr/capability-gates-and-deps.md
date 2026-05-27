# NFR Capability Gates + Dependency Evidence (VAL-NFR-005..VAL-NFR-007)

Worker session: `0ecf51ad-330f-4949-ae38-f2ea5556704b`
Feature: `nfr-capability-gates-and-deps`
Mission branch HEAD (parent of this commit): `206ea92` (`nfr: capture
VAL-NFR-001..004 evidence and tighten SAFETY comments`)
Build host: macOS 26.3 (Darwin 25.3.0), iPhone 17 Pro simulator.

## VAL-NFR-005: Capability `voice = false` hides all iOS voice controls

### Production wiring

The shared Rust manifest (`shared/rust-bridge/codex-mobile-client/src/alleycat.rs`,
`LitterManifestCapabilities::defaults_for(LitterManifestRuntimeKind::Pi)`)
pins `voice = false` for every pi entry (VAL-REM-009). The Swift
`PiCapabilityGates.showsVoice(for: agentRuntimeKind)` helper consumes
the canonical pi runtime kind and returns `false`, and the new
`piCapabilityGate(.voice, agentRuntimeKind:)` view modifier collapses
its content to an `EmptyView` whenever that gate returns `false`.

Three accessibility identifiers were added to the production iOS
voice surfaces so the contract's literal can be asserted:

* `voice.mic.button` — set on `InlineVoiceButton`
  (`apps/ios/Sources/Litter/Views/InlineVoiceButton.swift`) and on
  `HomeVoiceOrbButton`
  (`apps/ios/Sources/Litter/Views/HomeVoiceOrbButton.swift`).
* `voice.settings.row` — set on the harness's settings row (and
  reserved for future voice-settings rows in `SettingsView`).
* `voice.handoff.banner` — set on the `InlineHandoffView` banner used
  by `RealtimeVoiceScreen` while a voice→agent handoff is in flight.

### XCUITest evidence

A new harness `PiCapabilityFixtureUITestHarnessView` renders the three
identifiers behind the same `PiCapabilityGates.showsVoice` helper used
in production, parameterized by `--voice-false` / `--voice-true`. The
XCUITest `apps/ios/Tests/LitterUITests/PiCapabilityGatesUITests.swift`
asserts:

* `testVoiceCapabilityFalseHidesAllVoiceAccessibilityIdentifiers`:
  with `--voice-false` (pi runtime kind), each of
  `voice.mic.button`, `voice.settings.row`, `voice.handoff.banner`
  resolves to `.exists == false` across all reasonable element types
  (buttons / static texts / generic elements).
* `testVoiceCapabilityTrueRevealsAllVoiceAccessibilityIdentifiers`:
  with `--voice-true` (codex runtime kind), the same three
  identifiers resolve as existing elements — proving the gate is the
  only thing flipping the elements off.

Command + result:

```
$ xcodebuild test \
    -project apps/ios/Litter.xcodeproj \
    -scheme Litter \
    -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
    -only-testing:LitterUITests/PiCapabilityGatesUITests
...
Test Case '-[LitterUITests.PiCapabilityGatesUITests testVoiceCapabilityFalseHidesAllVoiceAccessibilityIdentifiers]' passed (...)
Test Case '-[LitterUITests.PiCapabilityGatesUITests testVoiceCapabilityTrueRevealsAllVoiceAccessibilityIdentifiers]' passed (19.368 seconds).
Test Suite 'PiCapabilityGatesUITests' passed at 2026-05-27 02:04:10.956.
	 Executed 2 tests, with 0 failures (0 unexpected) in 63.705 (63.712) seconds
** TEST SUCCEEDED **
```

**Verdict:** VAL-NFR-005 satisfied.

## VAL-NFR-006: Capability `plans = false` hides Codex plan UI server-side

### Implementation

The pi outbound stream is structurally incapable of carrying a plan
event: the `PiEvent` enum in
`shared/rust-bridge/pi-mobile-client/src/server.rs` declares no
`Plan*` variant, the `PiSessionConfig` carries no `plans` toggle,
and `runtime_bridge::drive` only forwards the known non-plan event
shapes (`PromptReceived`, `AssistantText*`, `ToolExec*`, `TurnState*`,
`TurnComplete`, `TurnError`, `ShuttingDown`).

A new integration test
`shared/rust-bridge/pi-server-runner/tests/capability_plans.rs` pins
all three invariants:

* `pi_event_enum_has_no_plan_variant_in_source`: parses the `PiEvent`
  enum body in `pi-mobile-client/src/server.rs` (stripping comments)
  and asserts no variant name contains `Plan`. Fails closed if a
  future contributor adds a `PlanUpdate` / `TurnPlanUpdated` /
  `PlanCard` variant.
* `pi_session_config_has_no_plans_toggle`: ditto for
  `PiSessionConfig` — there must be no `plans:` field.
* `scripted_pi_run_emits_no_plan_events`: drives an in-process pi
  session in echo-mode (deterministic, no network), pumps the
  broadcast stream, and asserts none of the observed
  `PiEvent`s' `Debug` representation contains the substring `plan`
  (case-insensitive), plus the pattern-match `match` ensures every
  observed event is one of the known non-plan variants.

### Evidence

```
$ cd shared/rust-bridge && cargo test -p pi-server-runner --test capability_plans -- --nocapture
...
    Finished `test` profile [unoptimized + debuginfo] target(s) in 56.41s
     Running tests/capability_plans.rs (target/debug/deps/capability_plans-0e4e5786457dbd26)

running 3 tests
test pi_event_enum_has_no_plan_variant_in_source ... ok
test pi_session_config_has_no_plans_toggle ... ok
test scripted_pi_run_emits_no_plan_events ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
```

The test target is wired through an explicit `[[test]]` entry in
`shared/rust-bridge/pi-server-runner/Cargo.toml` so the contract's
literal `cargo test -p pi-server-runner --test capability_plans`
invocation resolves.

**Verdict:** VAL-NFR-006 satisfied.

## VAL-NFR-007: Pi runtime introduces no new C/C++/ObjC dependencies on iOS

### `apps/ios/project.yml` SPM package diff

Pre-pi baseline = parent of `a35c5ba`, the commit that introduced
`pi-mobile-client`. Comparing the `packages:` block of `project.yml`
at that revision against HEAD:

```
$ git show a35c5ba~1:apps/ios/project.yml \
    | awk '/^packages:/{f=1} f && /^[a-z][a-z]+:/ && !/^packages:/{f=0} f' > /tmp/pkgs_pre.txt
$ awk '/^packages:/{f=1} f && /^[a-z][a-z]+:/ && !/^packages:/{f=0} f' apps/ios/project.yml > /tmp/pkgs_post.txt
$ diff /tmp/pkgs_pre.txt /tmp/pkgs_post.txt
$ echo $?
0
```

The diff is empty. The full `packages:` block at both revisions:

```yaml
packages:
  Hairball:
    url: https://github.com/dnakov/hairball.git
    branch: main
  WebRTC:
    url: https://github.com/stasel/WebRTC.git
    exactVersion: "147.0.0"
  Nuke:
    url: https://github.com/kean/Nuke.git
    from: "12.8.0"
```

Hairball / WebRTC / Nuke all pre-date the pi integration; none were
added by it. Net SPM additions outside Rust UniFFI artifacts: **0**.

### `apps/ios/Frameworks/` snapshot

```
$ ls apps/ios/Frameworks
codex_mobile_client.xcframework
```

The only resident xcframework is `codex_mobile_client.xcframework`,
which is the single shared Rust UniFFI artifact — explicitly excluded
by the contract ("zero net additions outside Rust UniFFI artifacts").
No new `.framework` / `.a` files landed under `apps/ios/Frameworks/`.

### What the pi runtime *does* add

The pi runtime statically links into `libcodex_mobile_client.a` (and
the corresponding `codex_mobile_client.xcframework`) via the
`pi-mobile-client` Rust crate. All pi code, plus the
`pi_agent_rust` submodule it consumes, is pure-Rust modulo the
`rquickjs-sys` JS bridge whose iOS bindgen gate was added in
`shared/third_party/pi_agent_rust` (VAL-SCAF-002). That bindgen path
emits a Rust staticlib, not a separate iOS-visible C/C++/ObjC
dependency.

**Verdict:** VAL-NFR-007 satisfied — zero net SPM additions, zero
net `Frameworks/` additions outside Rust UniFFI artifacts.

## Summary

| Assertion | Verifier | Result |
|---|---|---|
| VAL-NFR-005 voice=false hides iOS controls | `xcodebuild test -only-testing:LitterUITests/PiCapabilityGatesUITests` | ✅ 2/2 passed |
| VAL-NFR-006 plans=false suppresses PlanUpdate | `cargo test -p pi-server-runner --test capability_plans` | ✅ 3/3 passed |
| VAL-NFR-007 no new iOS C/C++/ObjC deps | `diff` of `apps/ios/project.yml` `packages:` + `ls Frameworks/` | ✅ 0-byte diff |
