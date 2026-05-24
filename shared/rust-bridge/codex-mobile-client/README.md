# codex-mobile-client

The single public UniFFI surface for the Litter iOS and Android apps.
Owns canonical store/reducer state, hydration, discovery, SSH, and the
in-process pi runtime bridge. Mobile platform code consumes only the
UniFFI-generated Swift/Kotlin bindings produced from this crate; see
the top-level `CLAUDE.md` and `AGENTS.md` for the feature-placement
rules that keep new behavior on this side of the FFI boundary.

## Cargo features

| Feature          | Default | Purpose                                                                                                                                                                                                                                                                                                                                |
| ---------------- | ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `rpc-trace`      | off     | Verbose tracing for upstream RPC traffic. Only useful for protocol debugging.                                                                                                                                                                                                                                                          |
| `test-injection` | off     | Exposes test-only hooks on `AppClient` for asserting the pi BYOK plumbing without booting the iSH kernel. Adds `connect_local_pi_byok_with_ish_exec(...)` (accepts a caller-supplied `PiIshExec` callback) and `pi_active_base_url(server_id)` (reads back the resolved `PiSessionConfig.base_url`). Used by XCTest / integration tests for VAL-IOS-PI-011 and VAL-IOS-PI-012. Production iOS Debug/device and package builds compile with the feature **off** so the symbols never reach a shipping binary. |

### Enabling `test-injection`

* **Rust integration tests**:
  `cargo test -p codex-mobile-client --features test-injection`.
* **iOS XCTest**: build the simulator staticlib with
  `make rust-ios-sim-test-injection` before invoking
  `xcodebuild test`. The Makefile target forwards
  `--test-injection` to `apps/ios/scripts/build-rust.sh` and forces a
  bindings regeneration so the generated Swift surface includes the
  test-only methods. Switch back to a production lane (e.g.
  `make rust-ios-sim-fast`) before shipping a Debug build.
