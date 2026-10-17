#!/usr/bin/env bash
# Run one owned install with live output, a log, no stdin and a bounded lifetime.
set -euo pipefail
umask 077
log=$1 limit=$2
shift 2
case "$limit" in ''|*[!0-9]*|0) echo 'kit: install timeout must be a positive number of seconds' >&2; exit 2 ;; esac
work=$(mktemp -d "${log}.runner.XXXXXX")
mkfifo "$work/output"
# A FIFO keeps the logger out of the install's process group on Bash 3.2 too.
set -m
tee -a "$log" < "$work/output" &
logger=$!
"$@" </dev/null > "$work/output" 2>&1 &
worker=$!
(
  set +m
  # Do not let a very fast worker make cleanup race process-group creation.
  # This handshake runs only after Bash has established the watchdog group;
  # every timer child subsequently inherits that group with job control off.
  : > "$work/watchdog-ready"
  sleep "$limit"
  echo timeout > "$work/timeout"
  echo "kit: installation exceeded ${limit}s; see $log" | tee -a "$log" >&2
  kill -TERM -- "-$worker" 2>/dev/null || true
  # No detached grace timer: cancellation must not leave a child holding the
  # caller's output pipe open after the runner exits.
  kill -KILL -- "-$worker" 2>/dev/null || true
) &
watchdog=$!
cleanup() {
  kill -KILL -- "-$watchdog" "$watchdog" 2>/dev/null || true
  kill -TERM -- "-$worker" 2>/dev/null || true
  kill -KILL -- "-$worker" 2>/dev/null || true
  wait "$watchdog" 2>/dev/null || true
  # A group kill can race a fork already in progress: the timer may appear
  # after the first signal's membership snapshot. Once its parent is reaped it
  # cannot spawn again, so a final group kill closes that window.
  kill -KILL -- "-$watchdog" 2>/dev/null || true
  wait "$worker" 2>/dev/null || true
  kill -KILL -- "-$worker" 2>/dev/null || true
  wait "$logger" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
while [ ! -e "$work/watchdog-ready" ]; do
  kill -0 "$watchdog" 2>/dev/null || exit 1
done
set +m
rc=0
wait "$worker" || rc=$?
[ ! -s "$work/timeout" ] || rc=124
exit "$rc"
