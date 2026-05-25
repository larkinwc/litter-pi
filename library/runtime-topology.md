# pi-mobile-client runtime topology

`pi-mobile-client::server::start_in_process` constructs an `asupersync`
runtime on a dedicated OS thread named `pi-asupersync` and attaches a
platform-appropriate I/O reactor before pi's HTTP client (used by every
provider, including `AnthropicProvider` over the BYOK proxy) issues any
sockets. Without an attached reactor the agent loop hangs forever on the
first request.

## Platform reactor matrix

`asupersync::runtime::reactor::create_reactor()` selects the backend:

| Target                                          | Reactor backend  | Module             |
|------------------------------------------------|------------------|--------------------|
| `target_os = "linux"`                          | `EpollReactor`   | `reactor/epoll.rs` |
| `target_os = "android"` (Litter patch)         | `EpollReactor`   | `reactor/epoll.rs` |
| `target_os = "macos"` and other BSDs           | `KqueueReactor`  | `reactor/kqueue.rs`|
| `target_os = "ios"` (Litter patch)             | `KqueueReactor`  | `reactor/kqueue.rs`|
| `target_os = "windows"`                        | `IocpReactor`    | `reactor/windows.rs`|
| `target_arch = "wasm32"`                       | `BrowserReactor` | `reactor/browser.rs`|
| anything else                                  | returns `io::ErrorKind::Unsupported` | — |

iOS shares the BSD-family kqueue ABI with macOS, and Android shares the
Linux epoll ABI, so each platform reuses the existing backend
implementation unchanged. The only edits in our vendored asupersync are
`#[cfg(...)]` predicate widenings.

## Vendored asupersync rationale

Upstream `asupersync = 0.3.2` does not include iOS or Android in its
reactor cfg gates and does not expose a public extension API for
plugging in an alternative reactor (`Events::push` is `pub(crate)` and
there is no tokio adapter). The minimum diff to make `create_reactor()`
work on `target_os = "ios"` and `target_os = "android"` is a handful of
`#[cfg(...)]` predicate edits in
`src/runtime/reactor/mod.rs`, plus a one-line widening of the
`deny(dead_code)` lint gate in `src/lib.rs` (some macOS-only helpers in
`runtime/resource_monitor.rs` are not callable from the iOS slice).

User-approved approach: **vendor asupersync 0.3.2 as a git submodule
under `shared/third_party/asupersync/` and patch in place**. The patch
is applied on the `litter/ios-android-reactor-cfg` branch in the
submodule and is kept as small as possible (cfg-attr edits only, no API
surface changes).

Both `Cargo.toml` (this workspace) and `shared/rust-bridge/Cargo.toml`
(the bridge workspace) carry a matching `[patch.crates-io] asupersync =
{ path = "..." }` entry so `pi-server-runner` and `pi-mobile-client`
resolve the same patched copy.

## Upstream PR plan

We intend to file an upstream PR against
`Dicklesworthstone/asupersync` proposing:

1. Adding `target_os = "ios"` next to `target_os = "macos"` on every
   kqueue-flavoured cfg gate in `src/runtime/reactor/mod.rs`.
2. Adding `target_os = "android"` next to `target_os = "linux"` on
   every epoll-flavoured cfg gate.
3. Optionally exposing a `pub` extension point on `Events::push` plus a
   tokio adapter so downstream embedders can wire a custom reactor
   without forking the crate (this would let us remove the
   `[patch.crates-io]` entry entirely).

Until that lands and is released, the vendored copy stays in tree.

## Cross-client OAuth reuse (claude-credentials import)

To re-use an existing Claude Code Anthropic OAuth credential without a
fresh interactive sign-in, run
`pi-server-runner --import-claude-credentials --credentials-path <PATH>`
(or set `CLAUDE_CREDENTIALS_JSON_PATH`). The runner parses the Claude
Code JSON (`access_token`, `refresh_token`, ISO 8601 `expired`),
stamps pi's anthropic `client_id` + `token_url`, and writes the
credential under the `anthropic` provider key in pi's `auth.json` via
the shared `AuthStorage` file-locking path. Existing non-anthropic
entries are preserved. Tokens never appear in logs — only a redacted
summary (provider, email, expires-in-ms) is emitted.
