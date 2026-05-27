# Pi Coding Agent Runtime

This document covers the operational surfaces that the litter mobile apps use
to drive the `pi` coding agent (from
[`larkinwc/pi_agent_rust`](https://github.com/larkinwc/pi_agent_rust)). Two
runtimes are supported: an **in-process** runtime that runs `pi` inside the
mobile app via the shared Rust layer, and a **remote SSH** runtime where
`pi` runs on a remote host and litter drives it over SSH. Pairing through
the Alleycat relay is layered on top of the same shared client.

## Local iOS

The iOS app boots the shared `codex-mobile-client` Rust library through
UniFFI; that crate in turn embeds `pi-mobile-client`, which links the
`pi_agent_rust` core directly into the process. There is no separate
helper binary on device — `pi` runs in-process inside `Litter.app`.

Local-iOS bring-up checklist:

1. Build the package lane (`make ios`) or, for iteration, `make ios-sim-fast`
   / `make ios-device-fast`. The Rust pi-mobile-client crate is part of the
   normal `codex_mobile_client` static lib / xcframework — no extra
   targets are required in `apps/ios/project.yml`.
2. Open Settings → Servers and pick **Local (Pi)** as the runtime. The
   Swift bridge reports the chosen `AlleycatAgentRuntimeKind.Pi`, and the
   Rust layer constructs the in-process `ProotToolFactory` automatically.
3. Authenticate either via the **Anthropic OAuth** flow (see *OAuth flow*
   below) or paste an API key in **BYOK setup**.
4. Capability flags surface through the typed UniFFI manifest; the iOS UI
   hides the mic button, voice settings row, and voice handoff banner
   whenever `voice = false` is reported (VAL-NFR-005).

Useful host-side debugging without booting the simulator:

```bash
cargo run -p pi-server-runner -- --local
cargo run -p pi-server-runner -- --byok --anthropic-key "$ANTHROPIC_API_KEY"
cargo run -p pi-server-runner -- --oauth-paste
```

These modes exercise the exact same `pi-mobile-client` code path the iOS
app uses, so a passing runner is a strong signal that the on-device path
will work.

## Local Android

Android consumes the same `codex-mobile-client` UniFFI surface (Kotlin
bindings generated into `shared/rust-bridge/generated/kotlin/`). The
in-process pi runtime ships behind the same `AlleycatAgentRuntimeKind.Pi`
enum value; the Compose UI for the pi-specific affordances lives in
`apps/android/app/src/main/java/com/litter/android/ui/pi/`
(`AnthropicOAuthScreen.kt`, `PiCapabilityGates.kt`).

Local-Android bring-up checklist:

1. Cross-compile the Rust JNI bundle (`make rust-android`) and assemble
   the APK (`make android-emulator-fast` for the host-appropriate ABI).
2. Install + launch:
   `adb -e install -r apps/android/app/build/outputs/apk/debug/app-debug.apk`
   then
   `adb -e shell am start -n com.sigkitten.litter.android/com.litter.android.MainActivity`.
3. Pick **Local (Pi)** in the runtime selector. The Kotlin bridge calls
   into the same Rust factory as iOS.
4. Capability gating is enforced by `PiCapabilityGates` which observes the
   manifest UniFFI record and hides voice / plans entries when those are
   `false`.

Android UI manual QA for the pi runtime is currently **deferred** — see
`apps/android/docs/qa-matrix.md` ("Pi runtime"). Host-side validation runs
through `cargo test -p pi-mobile-client` and the same `pi-server-runner`
modes listed under *Local iOS*.

## Remote SSH

The remote runtime invokes a `pi` binary that is already installed on the
target host. The mobile client's binary resolver
(`codex-mobile-client::ssh::pi_binary::pi_binary_candidates`) probes, in
order:

1. `~/.local/bin/pi`
2. `/opt/homebrew/bin/pi`
3. `/usr/local/bin/pi`
4. `/usr/bin/pi`
5. `pi` via the SSH login shell's `$PATH`

### Prerequisite: install `pi` on the target host

Before the iOS / Android remote path can connect to a host, a `pi` binary
must exist on that host at one of the candidate paths above. Use the helper
script:

```bash
PI_REMOTE_SSH_HOST=<host> PI_REMOTE_SSH_USER=<user> \
  tools/scripts/install-pi-on-remote.sh
```

What it does:

- SSHes in to `${PI_REMOTE_SSH_USER}@${PI_REMOTE_SSH_HOST}`.
- Clones the litter fork
  (`https://github.com/larkinwc/pi_agent_rust.git`, branch
  `mission/ios-bindgen-gate`) into `~/.cache/litter/pi_agent_rust/` and
  checks out the same commit currently pinned in
  `shared/third_party/pi_agent_rust/` (override with `PI_AGENT_REV=<sha>` if
  the pin is not yet pushed to the fork).
- Runs `cargo build --release -p pi_agent_rust --bin pi` with the fork's
  default features (`sqlite-sessions`, `js-extensions`, `ast-grep`). We
  deliberately do **not** mirror the mobile-side feature trimming here —
  the remote host runs a normal `pi` install with full functionality.
- Installs the resulting binary into `~/.local/bin/pi` (the first explicit
  candidate path probed by the mobile client).
- Verifies the install by running `~/.local/bin/pi --version`.

Recognized environment variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `PI_REMOTE_SSH_HOST` | _required_ | Target host |
| `PI_REMOTE_SSH_USER` | _required_ | Target user |
| `PI_REMOTE_SSH_PORT` | `22` | SSH port |
| `PI_REMOTE_SSH_KEY` | (ssh default) | Explicit private key path |
| `PI_AGENT_REPO` | litter fork URL | Override the source repo |
| `PI_AGENT_BRANCH` | `mission/ios-bindgen-gate` | Override the branch |
| `PI_AGENT_REV` | submodule pin from `HEAD` | Override the commit |
| `PI_REMOTE_DEST` | `$HOME/.local/bin/pi` | Install destination |

### Why clone + build remotely instead of cross-compiling locally?

The remote host already has cargo + rustup, so there is no cross-toolchain
to install on the developer machine, and the first build pulls the exact
pinned commit so the remote `pi` matches what this repo expects. The
trade-off is that the first install on a fresh host is slow; pick this lane
for parity with the submodule pin, and only switch to local
cross-compilation if iteration speed becomes the dominant cost.

End-to-end host-side validation of the SSH driving path:

```bash
cargo run -p pi-server-runner -- --remote-ssh \
  --ssh-host "$PI_REMOTE_SSH_HOST" --ssh-user "$PI_REMOTE_SSH_USER"
```

## Alleycat

Alleycat is the relay-paired transport that lets the mobile apps reach a
`pi` (or Codex) agent on a remote workstation without an inbound SSH
connection. The shared Rust client owns pairing, token rotation, and
agent selection; the platform UI is only a QR scanner plus a runtime
picker.

- **iOS:** `AlleycatAddServerSheet` opens from the discovery toolbar's QR
  button, parses the payload via `AlleycatBridge.parsePairPayload`,
  enumerates available agents with `serverBridge.listAlleycatAgents`,
  and persists tokens through `AlleycatCredentialStore`.
- **Android:** the same QR flow uses CameraX + ML Kit; the persisted
  record lives in `SavedServerStore.rememberAlleycat`. Legacy Alleycat
  records (pre-multi-agent) require a fresh QR scan to attach the agent
  kind.
- **Runner:** `cargo run -p pi-server-runner -- --alleycat-pair`
  exercises the pairing handshake, agent enumeration, and a turn over the
  paired transport end-to-end without booting iOS. Use
  `--inject-drop kill-stop` or `--inject-drop socat-partition` to drive
  fault-injection coverage of reconnect + resume semantics.

The Alleycat transport is agent-agnostic: the same paired host can expose
both a Codex and a pi runtime, and the mobile client lets the user pick
between them at connect time.

## OAuth flow

Anthropic OAuth is the preferred authentication path for the in-process
pi runtime, because it produces a refreshable credential without
shipping a long-lived API key to the device.

High-level flow (shared Rust, exposed to both iOS and Android through
typed UniFFI records — no wire-format parsing in Swift/Kotlin):

1. The platform UI calls `pi_anthropic_oauth_begin()` on the shared
   client. The Rust layer generates a PKCE verifier/challenge pair and
   returns the authorization URL plus an opaque flow handle.
2. The user authenticates in the system browser; Anthropic redirects to
   the registered callback. On iOS this is captured via
   `ASWebAuthenticationSession`; on Android via a Chrome Custom Tab plus
   the registered `litter://oauth/anthropic` intent filter.
3. The platform passes the redirect URL back into
   `pi_anthropic_oauth_complete(handle, redirect_url)`. Rust performs
   the token exchange, persists the OAuth credential into pi's
   `auth.json` (`AuthCredential::OAuth { access, refresh, expires_at }`),
   and emits an `AppStoreUpdateRecord` so both platforms refresh their
   account UI.
4. Token refresh happens entirely in the shared Rust client. The
   platforms never see or store the refresh token directly.

For host-side verification without booting a platform UI:

```bash
cargo run -p pi-server-runner -- --oauth-paste
# pastes the redirect URL into the runner; runs a turn against
# the resulting OAuth-authenticated pi process.
```

Validation contract assertions covering this path: VAL-AUTH-006
(refresh), VAL-AUTH-007 (persistence).

## BYOK setup

BYOK ("bring your own key") lets users skip OAuth and configure their
own provider credentials. Both Anthropic and OpenAI providers are
supported; selection and rotation are owned by the shared Rust
`AppClient`.

Mobile UX:

- **iOS / Android:** Settings → Account → "Bring your own key". The
  picker offers Anthropic or OpenAI; the entered key flows through a
  typed UniFFI record into the shared `pi-mobile-client::byok` module,
  which writes pi's `auth.json` atomically (temp file + rename + 0600).
- The same screen lets the user pick a default provider/model, written
  to `settings.json` so pi does not fall back to the first available
  provider.

Host-side bring-up + smoke:

```bash
cargo run -p pi-server-runner -- --byok \
  --anthropic-key "$ANTHROPIC_API_KEY" \
  --anthropic-base-url "$ANTHROPIC_BASE_URL"
```

Server-side remote bootstrap (used when `install-pi-on-remote.sh` is run
with credential env vars set) seeds the same files; see the script
recognized variables and the writes' atomicity / `flock` notes below.

### Remote credential bootstrap (optional)

If any of the credential env vars below are set, the install script also
seeds `~/.pi/agent/{auth.json,models.json,settings.json}` on the remote
so the freshly installed `pi` authenticates without any manual
post-install editing. The writes are atomic (temp file + rename) and
hold an exclusive `flock` on `~/.pi/agent/auth.json.lock` so an
in-flight pi session is not clobbered.

| Variable | Effect |
| --- | --- |
| `ANTHROPIC_API_KEY` | Writes `auth.json[anthropic] = {type: "api_key", key: <value>}`. The field name is `key`, **not** `api_key` — that's pi's `AuthCredential::ApiKey` schema (see `shared/third_party/pi_agent_rust/src/auth.rs`). A wrong field name silently falls back to `~/.claude/.credentials.json` and the proxy responds 401. |
| `ANTHROPIC_BASE_URL` | Seeds `models.json` provider entry for `anthropic` (`api=anthropic-messages`). pi does not honour the `ANTHROPIC_BASE_URL` env var at runtime, so the override must live in `models.json`. |
| `ANTHROPIC_MODEL` | Model id under the anthropic provider in `models.json` (default `claude-opus-4-7`). |
| `OPENAI_API_KEY` | Same shape under `auth.json[openai]`. |
| `OPENAI_BASE_URL` | Seeds `models.json` provider entry for `openai` (`api=openai-completions`). |
| `OPENAI_MODEL` | Model id under the openai provider in `models.json` (default `gpt-4o`). |
| `PI_DEFAULT_PROVIDER` | Written to `settings.json`. Auto-defaults to `anthropic` if `ANTHROPIC_API_KEY` is set, else `openai` if `OPENAI_API_KEY` is set. |
| `PI_DEFAULT_MODEL` | Written to `settings.json`. Auto-defaults to the matching `*_MODEL` for the chosen provider. |

Secrets stay out of argv: the script base64-encodes the JSON bundle into
the remote bash heredoc, decodes it with `python3`, writes per-file
temps inside `~/.pi/agent/`, `chmod 600`s them, then renames into place.

## Capabilities

The shared Rust manifest exposes the pi runtime's capabilities as a
typed UniFFI record (no stringly-typed status fields cross into
Swift/Kotlin). Both platforms read the same record and gate UI off it.
The table below compares the pi runtime against the existing Codex
runtime for the current release:

| Capability   | Pi    | Codex | Notes                                                                                           |
| ---          | ---   | ---   | ---                                                                                             |
| `voice`      | false | true  | Pi has no realtime voice transport; see *Voice is out of scope* below.                          |
| `plans`      | false | true  | Pi does not expose Codex's plan/subagent UI yet; the plans drawer is hidden when `false`.       |
| `tool_exec`  | true  | true  | Pi supports tool execution via the in-process `ProotToolFactory` (local) and the remote shell. |
| `mcp`        | true  | true  | MCP tool calls flow through pi's tool dispatch and render in the existing tool card UI.         |
| `byok`       | true  | true  | Both runtimes support BYOK; pi additionally supports Anthropic OAuth (see *OAuth flow*).        |
| `oauth`      | true  | true  | Pi: Anthropic OAuth. Codex: existing OpenAI/ChatGPT OAuth.                                      |
| `streaming`  | true  | true  | Assistant + tool deltas stream through the shared RPC.                                          |

Platforms must not derive capability state from runtime kind alone —
always read the manifest record. The validators specifically check that
when `voice = false` is reported, iOS hides `voice.mic.button`,
`voice.settings.row`, and `voice.handoff.banner` (VAL-NFR-005).

### Production-enforced gating

Capability gating is now enforced at the **production** mobile call
sites, not just inside the XCUITest fixture harness:

* iOS production surfaces apply `.piCapabilityGate(.voice, agentRuntimeKind:)`
  directly on `InlineVoiceButton`, `HomeVoiceOrbButton`, the
  `homeVoiceLauncher` overlay rendered by `HomeNavigationView`, and
  the `InlineHandoffView` banner used by `RealtimeVoiceScreen`. The
  runtime kind passed into the gate is resolved from the active
  thread (via `AppModel.snapshot.threadSnapshot(for:).agentRuntimeKind`)
  with a fallback to the home dashboard's currently selected
  server's first available agent runtime — the same observation the
  harness exercises end-to-end. The XCUITest
  `apps/ios/Tests/LitterUITests/PiCapabilityGatesUITests.swift`
  contains a third case
  (`testProductionAppHidesVoiceAccessibilityIdentifiersWhenActiveRuntimeIsPi`)
  that boots the production app surface with
  `--ui-test-pi-active-runtime-kind pi` (DEBUG-only override) and
  asserts none of the three voice accessibility identifiers
  resolve.
* Android Compose production surfaces use the same string-based
  decision helper (`PiCapabilityGates.showsVoice(agentRuntimeKind:)`
  in `ui/PiCapabilityGates.kt`). The composer call site in
  `ui/conversation/ComposerBar.kt` consults the helper before
  rendering `InlineVoiceButton`, mirroring the iOS gate. Host-side
  parity is pinned by
  `apps/android/app/src/test/java/com/litter/android/ui/PiCapabilityGatesDecisionTest.kt`,
  following the same pattern as
  `RetryTurnRowDecisionTest.kt`. The full Compose
  instrumentation test remains deferred per the Android UI deferral
  documented in `apps/android/docs/qa-matrix.md`.

## Voice is out of scope

Realtime voice is intentionally out of scope for the pi runtime in this
release. The shared Rust client reports `voice = false` in the pi
manifest; both mobile UIs hide every voice affordance whenever that
flag is `false` (see *Capabilities*).

Follow-up tracker: see issue
[`pi-runtime/voice-followup`](https://github.com/larkinwc/litter/issues?q=label%3Api-runtime+label%3Avoice)
for the multi-step plan to land pi voice once an upstream realtime
transport is available. Until that issue is closed, do not wire
platform-side voice controls to the pi runtime, and do not introduce a
parallel native voice transport just for pi — when voice lands it will
flow through the same libwebrtc path the Codex runtime already uses.
