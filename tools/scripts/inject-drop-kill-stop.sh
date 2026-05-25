#!/usr/bin/env bash
# Out-of-band helper for VAL-REM-006.
#
# The remote pi-acp runner does not (and must not) self-pause its own
# russh process. This script orchestrates the disruption from the
# outside: it watches for a `pi-server-runner --inject-drop kill-stop`
# process, then signals SIGSTOP + SIGCONT to force a transient pause
# that simulates a network hiccup. The runner's reconnect tolerance is
# verified by the validator pinning the JSONL transcript for absence
# of a `disconnected` line and presence of a `turn_complete`.
#
# Usage:
#   tools/scripts/inject-drop-kill-stop.sh <pid> [pause_seconds]
#
# Defaults: pause_seconds=5.

set -euo pipefail

pid="${1:?pid required: pi-server-runner pid to pause}"
pause="${2:-5}"

if ! kill -0 "$pid" 2>/dev/null; then
    echo "inject-drop-kill-stop: pid $pid is not running" >&2
    exit 2
fi

echo "inject-drop-kill-stop: SIGSTOP pid=$pid (resume in ${pause}s)"
kill -STOP "$pid"
sleep "$pause"
kill -CONT "$pid"
echo "inject-drop-kill-stop: SIGCONT pid=$pid"
