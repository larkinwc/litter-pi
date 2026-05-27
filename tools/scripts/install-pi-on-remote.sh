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
# Credential bootstrap (all optional). When ANY of these are set, the script
# also seeds `~/.pi/agent/{auth.json,models.json,settings.json}` on the remote
# so that the freshly installed `pi` can authenticate without any manual
# post-install editing. Writes are atomic (temp file + rename) and respect
# pi's `auth.json.lock` so an in-flight pi session is not clobbered.
#
#   ANTHROPIC_API_KEY    seeds `auth.json[anthropic] = {type: "api_key", key}`
#                        using pi's `AuthCredential::ApiKey` schema (field name
#                        is `key`, NOT `api_key` — the latter falls back to
#                        ~/.claude/.credentials.json and 401s on proxies).
#   ANTHROPIC_BASE_URL   seeds `models.json` provider entry for anthropic with
#                        api=anthropic-messages; pi does not honour the env var
#                        for base_url at runtime so it must live in models.json.
#   ANTHROPIC_MODEL      model id to expose under the anthropic provider in
#                        models.json (default: claude-opus-4-7).
#   OPENAI_API_KEY       same shape under `openai`.
#   OPENAI_BASE_URL      seeds openai provider entry (api=openai-completions).
#   OPENAI_MODEL         model id under openai (default: gpt-4o).
#   PI_DEFAULT_PROVIDER  written into settings.json (defaults to anthropic if
#                        ANTHROPIC_API_KEY set, else openai if OPENAI_API_KEY
#                        set, else unset).
#   PI_DEFAULT_MODEL     written into settings.json (defaults to the matching
#                        ANTHROPIC_MODEL / OPENAI_MODEL).
#
# Usage:
#   PI_REMOTE_SSH_HOST=192.168.1.156 PI_REMOTE_SSH_USER=linus \
#     tools/scripts/install-pi-on-remote.sh
#
#   # with credential bootstrap:
#   PI_REMOTE_SSH_HOST=192.168.1.156 PI_REMOTE_SSH_USER=linus \
#     ANTHROPIC_API_KEY=$ANTHROPIC_API_KEY \
#     ANTHROPIC_BASE_URL=$ANTHROPIC_BASE_URL \
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

# ---------------------------------------------------------------------------
# Credential bootstrap (optional; only runs when env vars are supplied).
#
# We build the JSON payloads locally and ship them over a single ssh stdin
# transfer that runs an atomic temp-file + rename install on the remote,
# guarded by flock on ~/.pi/agent/auth.json.lock so in-flight pi sessions are
# not clobbered. Secrets never appear in argv.
# ---------------------------------------------------------------------------

ANTHROPIC_MODEL_DEFAULT="${ANTHROPIC_MODEL:-claude-opus-4-7}"
OPENAI_MODEL_DEFAULT="${OPENAI_MODEL:-gpt-4o}"

# Derive defaults for settings.json
default_provider="${PI_DEFAULT_PROVIDER:-}"
default_model="${PI_DEFAULT_MODEL:-}"
if [[ -z "${default_provider}" ]]; then
    if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
        default_provider="anthropic"
        default_model="${default_model:-${ANTHROPIC_MODEL_DEFAULT}}"
    elif [[ -n "${OPENAI_API_KEY:-}" ]]; then
        default_provider="openai"
        default_model="${default_model:-${OPENAI_MODEL_DEFAULT}}"
    fi
fi

# Build JSON files locally via python3 (already a hard dep on the dev host).
build_credential_payloads() {
    if ! command -v python3 >/dev/null 2>&1; then
        err "python3 required locally to build credential JSON payloads"
        exit 5
    fi

    python3 - <<'PYEOF'
import json, os, sys

anthropic_key = os.environ.get("ANTHROPIC_API_KEY", "")
anthropic_url = os.environ.get("ANTHROPIC_BASE_URL", "")
anthropic_model = os.environ.get("ANTHROPIC_MODEL_RESOLVED", "")
openai_key = os.environ.get("OPENAI_API_KEY", "")
openai_url = os.environ.get("OPENAI_BASE_URL", "")
openai_model = os.environ.get("OPENAI_MODEL_RESOLVED", "")
default_provider = os.environ.get("DEFAULT_PROVIDER_RESOLVED", "")
default_model = os.environ.get("DEFAULT_MODEL_RESOLVED", "")

auth = {}
if anthropic_key:
    auth["anthropic"] = {"type": "api_key", "key": anthropic_key}
if openai_key:
    auth["openai"] = {"type": "api_key", "key": openai_key}

providers = {}
if anthropic_url or anthropic_key:
    entry = {
        "api": "anthropic-messages",
        "authHeader": False,
    }
    if anthropic_url:
        entry["baseUrl"] = anthropic_url
    entry["models"] = [{
        "id": anthropic_model,
        "name": anthropic_model,
        "input": ["text"],
        "reasoning": True,
        "contextWindow": 200000,
        "maxTokens": 8192,
    }]
    providers["anthropic"] = entry
if openai_url or openai_key:
    entry = {"api": "openai-completions"}
    if openai_url:
        entry["baseUrl"] = openai_url
    entry["models"] = [{
        "id": openai_model,
        "name": openai_model,
        "input": ["text"],
        "contextWindow": 128000,
        "maxTokens": 8192,
    }]
    providers["openai"] = entry

models = {"providers": providers} if providers else None

settings = {}
if default_provider:
    settings["default_provider"] = default_provider
if default_model:
    settings["default_model"] = default_model

out = {
    "auth": auth or None,
    "models": models,
    "settings": settings or None,
}
# Emit as a single JSON document so the remote can pick apart.
json.dump(out, sys.stdout)
PYEOF
}

if [[ -n "${ANTHROPIC_API_KEY:-}" || -n "${ANTHROPIC_BASE_URL:-}" \
        || -n "${OPENAI_API_KEY:-}" || -n "${OPENAI_BASE_URL:-}" \
        || -n "${PI_DEFAULT_PROVIDER:-}" || -n "${PI_DEFAULT_MODEL:-}" ]]; then
    log "credential bootstrap requested; building ~/.pi/agent/*.json payloads"
    CRED_BUNDLE=$(
        ANTHROPIC_MODEL_RESOLVED="${ANTHROPIC_MODEL_DEFAULT}" \
        OPENAI_MODEL_RESOLVED="${OPENAI_MODEL_DEFAULT}" \
        DEFAULT_PROVIDER_RESOLVED="${default_provider}" \
        DEFAULT_MODEL_RESOLVED="${default_model}" \
        ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
        ANTHROPIC_BASE_URL="${ANTHROPIC_BASE_URL:-}" \
        OPENAI_API_KEY="${OPENAI_API_KEY:-}" \
        OPENAI_BASE_URL="${OPENAI_BASE_URL:-}" \
        build_credential_payloads
    )
    SEED_CREDS=1
else
    SEED_CREDS=0
fi

log "running remote bootstrap"
# shellcheck disable=SC2087 # we *want* local expansion: REMOTE_SCRIPT has
# already been substituted with concrete values; the remote `bash -s` just
# reads the resulting script body from stdin.
ssh "${SSH_OPTS[@]}" "${PI_REMOTE_SSH_USER}@${PI_REMOTE_SSH_HOST}" bash -s <<EOF
${REMOTE_SCRIPT}
EOF

if [[ "${SEED_CREDS}" == "1" ]]; then
    log "seeding ~/.pi/agent/{auth,models,settings}.json on remote"
    # Base64-encode the bundle so it survives heredoc transit without quoting
    # surprises (newlines, quotes, backslashes in JSON). It lands in a local
    # variable in the remote bash process and never on the remote disk except
    # as the final file payloads themselves. argv stays clean.
    if command -v base64 >/dev/null 2>&1; then
        CRED_BUNDLE_B64=$(printf '%s' "${CRED_BUNDLE}" | base64 | tr -d '\n')
    else
        err "base64 required locally to seed credentials"
        exit 5
    fi

    REMOTE_CRED_SCRIPT=$(cat <<'CRED_EOF'
set -euo pipefail
AGENT_DIR="$HOME/.pi/agent"
mkdir -p "$AGENT_DIR"

BUNDLE_B64="__BUNDLE_B64__"
if [[ -z "$BUNDLE_B64" ]]; then
    echo "remote: empty credential bundle" >&2
    exit 6
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "remote: python3 required to decode credential bundle" >&2
    exit 6
fi

# Use python3 to deconstruct the base64-encoded JSON bundle and emit each
# component to a per-file temp path. Doing the decode in one python invocation
# avoids re-passing the bundle as args.
TMP_AUTH="$(mktemp "$AGENT_DIR/.auth.XXXXXX")"
TMP_MODELS="$(mktemp "$AGENT_DIR/.models.XXXXXX")"
TMP_SETTINGS="$(mktemp "$AGENT_DIR/.settings.XXXXXX")"
trap 'rm -f "$TMP_AUTH" "$TMP_MODELS" "$TMP_SETTINGS"' EXIT

WROTE=$(BUNDLE_B64="$BUNDLE_B64" \
        TMP_AUTH="$TMP_AUTH" \
        TMP_MODELS="$TMP_MODELS" \
        TMP_SETTINGS="$TMP_SETTINGS" \
        python3 - <<'PYEOF'
import base64, json, os
b = json.loads(base64.b64decode(os.environ["BUNDLE_B64"]).decode("utf-8"))
wrote = []
def dump(key, path):
    v = b.get(key)
    if v is None:
        return
    with open(path, "w") as f:
        json.dump(v, f, indent=2)
        f.write("\n")
    wrote.append(key)
dump("auth", os.environ["TMP_AUTH"])
dump("models", os.environ["TMP_MODELS"])
dump("settings", os.environ["TMP_SETTINGS"])
print(",".join(wrote))
PYEOF
)

# Hold flock on auth.json.lock for the duration of the install so pi sessions
# don't observe a half-written auth.json. Pi uses the same lockfile via fs2.
LOCK="$AGENT_DIR/auth.json.lock"
touch "$LOCK"
exec 9<"$LOCK"
if command -v flock >/dev/null 2>&1; then
    flock -x 9
fi

install_one() {
    local key="$1" tmp="$2" dest="$3"
    if [[ ",$WROTE," == *",${key},"* ]]; then
        chmod 600 "$tmp"
        mv "$tmp" "$dest"
        echo "remote: wrote $dest"
    fi
}

install_one auth     "$TMP_AUTH"     "$AGENT_DIR/auth.json"
install_one models   "$TMP_MODELS"   "$AGENT_DIR/models.json"
install_one settings "$TMP_SETTINGS" "$AGENT_DIR/settings.json"

exec 9>&-
echo "remote: credential bootstrap complete (wrote=${WROTE:-none})"
CRED_EOF
)
    REMOTE_CRED_SCRIPT="${REMOTE_CRED_SCRIPT//__BUNDLE_B64__/${CRED_BUNDLE_B64}}"

    ssh "${SSH_OPTS[@]}" "${PI_REMOTE_SSH_USER}@${PI_REMOTE_SSH_HOST}" bash -s <<EOF
${REMOTE_CRED_SCRIPT}
EOF
fi

log "done"
