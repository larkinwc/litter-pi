#!/usr/bin/env bash
#
# install-pi-on-remote.sh
#
# Bootstrap the `pi` coding agent binary on a remote SSH host so the iOS /
# Android "remote pi" path (codex-mobile-client SshBridge + pi-server-runner
# --remote-ssh) has something to spawn.
#
# Approach: clone the litter fork of pi_agent_rust on the *remote* host into
# ~/.cache/litter/pi_agent_rust, check out the same commit pinned in this
# repo's submodule, run `cargo build --release -p pi` with *default* features
# (sqlite-sessions + js-extensions + ast-grep — i.e. a normal pi install; we
# deliberately do NOT mirror the mobile-side feature trimming because the
# remote host runs the full agent), then install the binary into
# ~/.local/bin/pi (the first candidate path in
# codex-mobile-client::ssh::pi_binary::pi_binary_candidates).
#
# Why this approach (clone + build remotely) vs cross-compile + scp:
#   - Host already has rustup + cargo 1.93, no cross-toolchain wrangling.
#   - First build pulls the fork's exact pinned commit so we match locally.
#   - Subsequent runs are incremental thanks to cargo's target cache.
# Trade-off: first build is slow (~5–10 min on a small VPS).
#
# Required environment:
#   PI_REMOTE_SSH_HOST   target host (e.g. 192.168.1.156)
#   PI_REMOTE_SSH_USER   target user (e.g. linus)
# Optional environment:
#   PI_REMOTE_SSH_PORT   default 22
#   PI_REMOTE_SSH_KEY    explicit private key path (otherwise ssh's default)
#   PI_AGENT_REV         git ref to check out; defaults to the submodule pin
#   PI_AGENT_REPO        git URL; defaults to the litter fork
#   PI_AGENT_BRANCH      branch to track when no PI_AGENT_REV pin matches;
#                        defaults to mission/ios-bindgen-gate
#   PI_REMOTE_DEST       install path; defaults to ~/.local/bin/pi (matches
#                        the first explicit candidate in
#                        codex-mobile-client::ssh::pi_binary)
#
# Usage:
#   PI_REMOTE_SSH_HOST=192.168.1.156 PI_REMOTE_SSH_USER=linus \
#     tools/scripts/install-pi-on-remote.sh

set -euo pipefail

err() { printf 'install-pi-on-remote: %s\n' "$*" >&2; }
log() { printf 'install-pi-on-remote: %s\n' "$*"; }

: "${PI_REMOTE_SSH_HOST:?PI_REMOTE_SSH_HOST must be set}"
: "${PI_REMOTE_SSH_USER:?PI_REMOTE_SSH_USER must be set}"

PORT="${PI_REMOTE_SSH_PORT:-22}"
REPO_URL="${PI_AGENT_REPO:-https://github.com/larkinwc/pi_agent_rust.git}"
BRANCH="${PI_AGENT_BRANCH:-mission/ios-bindgen-gate}"
DEST="${PI_REMOTE_DEST:-\$HOME/.local/bin/pi}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Resolve the pinned submodule commit so the remote build matches what
# this repo currently expects.
if [[ -z "${PI_AGENT_REV:-}" ]]; then
    if pinned_rev="$(cd "${REPO_ROOT}" && git rev-parse 'HEAD:shared/third_party/pi_agent_rust' 2>/dev/null)"; then
        PI_AGENT_REV="${pinned_rev}"
    else
        err "could not resolve submodule pin for shared/third_party/pi_agent_rust"
        err "set PI_AGENT_REV explicitly to override"
        exit 2
    fi
fi

log "host=${PI_REMOTE_SSH_USER}@${PI_REMOTE_SSH_HOST}:${PORT}"
log "repo=${REPO_URL}"
log "branch=${BRANCH}"
log "rev=${PI_AGENT_REV}"
log "dest=${DEST}"

SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15 -p "${PORT}")
if [[ -n "${PI_REMOTE_SSH_KEY:-}" ]]; then
    SSH_OPTS+=(-i "${PI_REMOTE_SSH_KEY}")
fi

# Remote build script. Quoted heredoc so $vars are evaluated remotely.
# Locally-substituted values are inlined via printf below.
REMOTE_SCRIPT=$(cat <<'REMOTE_EOF'
set -euo pipefail

REPO_URL="__REPO_URL__"
BRANCH="__BRANCH__"
REV="__REV__"
DEST="__DEST__"

CACHE_DIR="$HOME/.cache/litter/pi_agent_rust"
mkdir -p "$(dirname "$CACHE_DIR")"

# Make rustup-installed cargo/rustc visible even under non-interactive ssh.
export PATH="$HOME/.cargo/bin:$PATH"

if ! command -v cargo >/dev/null 2>&1; then
    echo "remote: cargo not found in PATH" >&2
    exit 3
fi
if ! command -v git >/dev/null 2>&1; then
    echo "remote: git not found in PATH" >&2
    exit 3
fi

if [[ ! -d "$CACHE_DIR/.git" ]]; then
    echo "remote: cloning $REPO_URL into $CACHE_DIR"
    git clone --branch "$BRANCH" "$REPO_URL" "$CACHE_DIR"
fi

cd "$CACHE_DIR"
echo "remote: fetching latest refs"
git fetch --tags origin "$BRANCH"

# Try to check out the exact pinned commit first; fall back to the branch tip
# if the pin is not yet in the remote (e.g. local-only commit).
if git cat-file -e "${REV}^{commit}" 2>/dev/null; then
    echo "remote: checking out pinned commit $REV"
    git checkout --quiet --detach "$REV"
else
    echo "remote: pinned commit $REV not found, falling back to origin/$BRANCH"
    git checkout --quiet "$BRANCH"
    git reset --hard "origin/$BRANCH"
fi

echo "remote: building pi (cargo build --release -p pi_agent_rust --bin pi)"
# rust-toolchain.toml in the fork pins nightly. rustup auto-installs on first
# `cargo` invocation inside the checkout, which is exactly what we want.
cargo build --release -p pi_agent_rust --bin pi

BIN="$CACHE_DIR/target/release/pi"
if [[ ! -x "$BIN" ]]; then
    echo "remote: built artifact missing at $BIN" >&2
    exit 4
fi

mkdir -p "$(dirname "$DEST")"
install -m 0755 "$BIN" "$DEST"

echo "remote: installed -> $DEST"
"$DEST" --version
REMOTE_EOF
)

# Substitute local-resolved values into the remote script body.
REMOTE_SCRIPT="${REMOTE_SCRIPT//__REPO_URL__/${REPO_URL}}"
REMOTE_SCRIPT="${REMOTE_SCRIPT//__BRANCH__/${BRANCH}}"
REMOTE_SCRIPT="${REMOTE_SCRIPT//__REV__/${PI_AGENT_REV}}"
REMOTE_SCRIPT="${REMOTE_SCRIPT//__DEST__/${DEST}}"

log "running remote bootstrap"
# shellcheck disable=SC2087 # we *want* local expansion: REMOTE_SCRIPT has
# already been substituted with concrete values; the remote `bash -s` just
# reads the resulting script body from stdin.
ssh "${SSH_OPTS[@]}" "${PI_REMOTE_SSH_USER}@${PI_REMOTE_SSH_HOST}" bash -s <<EOF
${REMOTE_SCRIPT}
EOF

log "done"
