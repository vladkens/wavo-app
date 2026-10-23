#!/bin/sh
# Starts a built Wavo.app in place of the running one, optionally installing it first.
#   scripts/run.sh <Wavo.app> [<dest, e.g. /Applications/Wavo.app>]
#
# The running Wavo (any copy) is asked to quit over its control socket, so it finishes what it
# is doing; one still running after 10 s gets a signal. With a dest the bundle replaces the one
# there. The app opens with its log in $WAVO_LOG_FILE (/tmp/wavo.log by default).
set -eu

app=$1
dest=${2:-}
log=${WAVO_LOG_FILE:-/tmp/wavo.log}
running='Wavo.app/Contents/MacOS/wavo-app'

if pgrep -f "$running" >/dev/null; then
  "$app/Contents/MacOS/wavo-app" ctl quit >/dev/null 2>&1 || true
  for _ in $(seq 50); do
    pgrep -f "$running" >/dev/null || break
    sleep 0.2
  done

  # Still there: killed, and gone before the new one starts (LaunchServices would only activate it).
  pkill -KILL -f "$running" 2>/dev/null || true
  while pgrep -f "$running" >/dev/null; do sleep 0.1; done
fi

if [ -n "$dest" ]; then
  rm -rf "$dest"
  ditto "$app" "$dest"
  app=$dest
fi

open --stderr "$log" --env WAVO_LOG=warn,wavo_app=debug "$app"
echo "started $app, log in $log"
