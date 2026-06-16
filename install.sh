#!/usr/bin/env bash
# Build token-use and install it as a per-user background service.
#   Linux : systemd user unit  (~/.config/systemd/user/token-use.service)
#   macOS : launchd agent       (~/Library/LaunchAgents/com.derickson.token-use.plist)
#
# Re-run any time to update the binary; the service is restarted in place.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DST="$HOME/.local/bin/token-use"
OS="$(uname -s)"

echo "==> Building release binary"
( cd "$REPO_DIR" && cargo build --release )

echo "==> Installing binary -> $BIN_DST"
mkdir -p "$HOME/.local/bin"
install -m 0755 "$REPO_DIR/target/release/token-use" "$BIN_DST"

case "$OS" in
  Linux)
    UNIT_DIR="$HOME/.config/systemd/user"
    mkdir -p "$UNIT_DIR"
    install -m 0644 "$REPO_DIR/deploy/token-use.service" "$UNIT_DIR/token-use.service"
    echo "==> Enabling systemd user service"
    systemctl --user daemon-reload
    systemctl --user enable --now token-use.service
    # Keep the service running even when no user session is logged in.
    loginctl enable-linger "$USER" 2>/dev/null || \
      echo "    (could not enable-linger; service runs while you are logged in)"
    echo "==> Done. Logs: journalctl --user -u token-use -f"
    echo "    Output NDJSON: $HOME/.local/share/token-use/logs/"
    ;;
  Darwin)
    AGENT_DIR="$HOME/Library/LaunchAgents"
    PLIST="$AGENT_DIR/com.derickson.token-use.plist"
    mkdir -p "$AGENT_DIR"
    sed "s|__HOME__|$HOME|g" "$REPO_DIR/deploy/com.derickson.token-use.plist" > "$PLIST"
    echo "==> Loading launchd agent"
    launchctl bootout "gui/$(id -u)/com.derickson.token-use" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$PLIST"
    launchctl enable "gui/$(id -u)/com.derickson.token-use"
    echo "==> Done. Logs: ~/Library/Logs/token-use.{out,err}.log"
    echo "    Output NDJSON: ~/Library/Application Support/token-use/logs/"
    ;;
  *)
    echo "Unsupported OS: $OS (Linux and macOS only)" >&2
    exit 1
    ;;
esac
