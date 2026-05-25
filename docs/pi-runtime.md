# Pi Coding Agent Runtime

This document covers the operational surfaces that the litter mobile apps use
to drive the `pi` coding agent (from
[`larkinwc/pi_agent_rust`](https://github.com/larkinwc/pi_agent_rust)). Two
runtimes are supported: an **in-process** runtime on the mobile device and a
**remote SSH** runtime where `pi` runs on a remote host and litter drives it
over SSH.

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

Prerequisites on the remote host:

- `git` and `rustup` (or a Rust toolchain) on `$PATH`. The fork pins
  nightly via `rust-toolchain.toml`; rustup installs it automatically on
  the first `cargo` invocation inside the checkout.
- Roughly 5–10 GB of free disk for the cargo target dir; first build is
  CPU-bound (≈7 min on a small VPS with cargo 1.93).
- SSH access via key (the script disables interactive password prompts via
  `BatchMode=yes`).

Subsequent runs of the script are incremental — it reuses the existing
clone and the cargo target cache, only recompiling crates that changed
between pins.

### Why clone + build remotely instead of cross-compiling locally?

The remote host already has cargo + rustup, so there is no cross-toolchain
to install on the developer machine, and the first build pulls the exact
pinned commit so the remote `pi` matches what this repo expects. The
trade-off is that the first install on a fresh host is slow; pick this lane
for parity with the submodule pin, and only switch to local
cross-compilation if iteration speed becomes the dominant cost.

## In-process (local) runtime

Documented separately by the in-process worker stream. The binary resolver
above is only used for the remote SSH lane.
