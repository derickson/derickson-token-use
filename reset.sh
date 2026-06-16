#!/usr/bin/env bash
# Full do-over for the token-use collector.
#
# Stops the background service, deletes the generated NDJSON output AND the
# durable checkpoint (state.json), then restarts the service. With no
# checkpoint, the daemon re-backfills every existing transcript from scratch,
# so the freshly-written files are picked up again by the Elastic Fleet Agent
# / Filebeat filestream input.
#
#   Linux : systemd user unit  (token-use.service)
#   macOS : launchd agent       (com.derickson.token-use.plist)
#
# Paths mirror the service definitions in deploy/. Override with the same
# TOKEN_USE_OUT_DIR / TOKEN_USE_STATE_DIR env vars if you customized the unit.
set -euo pipefail

OS="$(uname -s)"

case "$OS" in
  Linux)
    OUT_DIR="${TOKEN_USE_OUT_DIR:-$HOME/.local/share/token-use/logs}"
    STATE_DIR="${TOKEN_USE_STATE_DIR:-$HOME/.local/state/token-use}"
    ;;
  Darwin)
    OUT_DIR="${TOKEN_USE_OUT_DIR:-$HOME/Library/Application Support/token-use/logs}"
    STATE_DIR="${TOKEN_USE_STATE_DIR:-$HOME/Library/Application Support/token-use/state}"
    ;;
  *)
    echo "Unsupported OS: $OS (Linux and macOS only)" >&2
    exit 1
    ;;
esac

echo "==> Stopping service"
case "$OS" in
  Linux)  systemctl --user stop token-use.service ;;
  Darwin) launchctl bootout "gui/$(id -u)/com.derickson.token-use" 2>/dev/null || true ;;
esac

echo "==> Deleting generated NDJSON  -> $OUT_DIR"
rm -f "$OUT_DIR"/*.ndjson

echo "==> Deleting checkpoint         -> $STATE_DIR/state.json"
rm -f "$STATE_DIR/state.json"

echo "==> Restarting service (full backfill)"
case "$OS" in
  Linux)  systemctl --user start token-use.service ;;
  Darwin) launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.derickson.token-use.plist" ;;
esac

echo "==> Done. Files regenerating in: $OUT_DIR"
case "$OS" in
  Linux)  echo "    Watch: journalctl --user -u token-use -f" ;;
  Darwin) echo "    Watch: tail -f ~/Library/Logs/token-use.err.log" ;;
esac
