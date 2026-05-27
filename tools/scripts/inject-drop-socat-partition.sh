#!/usr/bin/env bash
# Out-of-band helper for VAL-REM-007.
#
# When the remote pi-acp runner is pointed at a socat tunnel
# (`socat TCP-LISTEN:<local_port>,reuseaddr,fork TCP:<host>:22`),
# this script tears the tunnel down for a brief window to force a
# transport-layer partition, then restarts it. The validator pins
# the JSONL transcript for `turn_complete` (i.e. the runner
# reconnected) after the partition window.
#
# Usage:
#   tools/scripts/inject-drop-socat-partition.sh <socat_pid> [partition_seconds]
#
# Defaults: partition_seconds=8.

set -euo pipefail

pid="${1:?pid required: socat tunnel pid to tear down}"
partition="${2:-8}"

if ! kill -0 "$pid" 2>/dev/null; then
    echo "inject-drop-socat-partition: pid $pid is not running" >&2
    exit 2
fi

echo "inject-drop-socat-partition: SIGTERM pid=$pid (partition ${partition}s)"
kill -TERM "$pid" || true
# Give socat a moment to actually exit. The caller's wrapper script is
# expected to relaunch socat after the partition window.
sleep "$partition"
echo "inject-drop-socat-partition: partition window over; caller must relaunch socat"
