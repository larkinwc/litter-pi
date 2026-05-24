# LitterTests

XCTest cases for the iOS app's Rust-backed surfaces. Most tests run
unconditionally against the production simulator staticlib (`make
rust-ios-sim-fast`). One harness — `PiByokE2ETests` — needs additional
plumbing because it exercises the **test-only** UniFFI surface gated on
the `test-injection` cargo feature.

## Pi BYOK end-to-end harness (VAL-IOS-PI-011 / VAL-IOS-PI-012)

`PiByokE2ETests.swift` covers two validation contract assertions:

- `testBYOKAnthropicTurnRoundTrip` — VAL-IOS-PI-011. Runs a BYOK
  Anthropic turn through a stub `PiIshExec` so the iSH kernel is not
  booted, asserts an `AssistantText` + `TurnComplete` arrives, and
  asserts the stub recorded at least one tool exec.
- `testBYOKOpenAICompatibleTurnRoundTrip` — VAL-IOS-PI-012. Same shape,
  but against a BYOK OpenAI-compatible profile with a custom base URL,
  and additionally asserts that `AppClient.piActiveBaseUrl(serverId)`
  returns the configured base URL.

The harness depends on three UniFFI symbols that **only exist when the
`test-injection` cargo feature is enabled** on `codex-mobile-client`:

- `AppClient.connectLocalPiByokWithIshExec(...)`
- `AppClient.piActiveBaseUrl(serverId:)`
- The `PiIshExec` callback interface + `PiIshExecOutput` record

Production iOS Debug/device and package builds compile with the feature
**off** so these symbols never reach a shipping binary. The
`make rust-ios-sim-fast` lane verifies this:

```
nm $(PWD)/apps/ios/GeneratedRust/ios-sim/libcodex_mobile_client.a \
  | grep -E 'connect_local_pi_byok_with_ish_exec|pi_active_base_url' \
  | head
```

prints nothing in the production lane.

### Opt-in

To run `PiByokE2ETests` locally:

1. Populate the repo-root `.env` with the credentials documented in
   `library/environment.md`:
   - `ANTHROPIC_API_KEY` (and optionally `ANTHROPIC_BASE_URL`)
   - `OPENAI_API_KEY` + `OPENAI_BASE_URL` (and optionally
     `OPENAI_MODEL`)
   The harness `XCTSkip`s cleanly when any required credential is
   absent.
2. Rebuild the simulator Rust staticlib + regenerated bindings with
   the `test-injection` feature on:
   ```
   make rust-ios-sim-test-injection
   ```
3. Drive the XCTest target with `ENABLE_PI_TEST_INJECTION=YES`. The
   build setting threads two things:
   - The pre-build script on the `LitterTests` target re-runs
     `make rust-ios-sim-test-injection` so any stale production
     simulator staticlib is replaced.
   - `SWIFT_ACTIVE_COMPILATION_CONDITIONS` gains `PI_TEST_INJECTION`,
     which is what `PiByokE2ETests.swift` is compiled behind. Without
     the condition the file is a no-op and the harness is skipped at
     compile time.

   Example invocation:
   ```
   set -a
   . ./.env
   set +a
   make rust-ios-sim-test-injection
   xcodebuild \
     -project apps/ios/Litter.xcodeproj \
     -scheme Litter \
     -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
     -only-testing:LitterTests/PiByokE2ETests \
     ENABLE_PI_TEST_INJECTION=YES \
     test
   ```

4. To return to the production simulator lane, run
   `make rust-ios-sim-fast` and unset `ENABLE_PI_TEST_INJECTION` (or
   set it to `NO`).

### Why a stub `PiIshExec`?

The upstream iSH kernel currently aborts on iOS 26.x simulator
runtimes (see `library/litter-ish-compatibility.md`). Booting it
inside an XCTest process would crash before the test could observe
the turn outcome. The stub `PiIshExec` intercepts tool exec at the
pi runtime boundary, returns canned `ls /root` output, and lets the
harness focus on what the assertions actually care about: the
end-to-end BYOK plumbing (provider selection, base URL routing,
event stream observability). `IshLazyBootTests` runs alongside this
harness and confirms `ishIsKernelBooted()` stays `false` for the
duration of the suite.
