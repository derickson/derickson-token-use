#!/usr/bin/env bash
# Standalone Filebeat path for token-use on Macs that CANNOT be enrolled in Fleet.
#
# Downloads a self-contained Filebeat INTO this project directory, drives it with
# a local ./filebeat-token-use.yml, and registers it as a launchd agent so it runs
# at boot without a login. It ships the collector's NDJSON to the SAME Elastic data
# stream (with the same NDJSON preprocessing) as the Fleet integration.
#
#   Binary   : ./filebeat/filebeat-<ver>-<arch>/        (gitignored, downloaded)
#   Config   : ./filebeat-token-use.yml                 (gitignored, has API key)
#   Service  : ~/Library/LaunchAgents/com.derickson.token-use-filebeat.plist
#   Registry : ~/Library/Application Support/token-use/filebeat-data
#   FB logs  : ~/Library/Logs/token-use-filebeat.{out,err}.log
#
# Re-run any time to update/restart the service in place.
# Use --uninstall to stop and remove the service (leaves the binary + config).
set -euo pipefail

FB_VERSION="9.4.2"     # pinned to match the Elastic Agent shipped to Fleet Macs
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LABEL="com.derickson.token-use-filebeat"
AGENT_DIR="$HOME/Library/LaunchAgents"
PLIST="$AGENT_DIR/$LABEL.plist"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This installer is macOS-only (use ./install.sh + a Fleet/systemd path on Linux)." >&2
  exit 1
fi

# ---- --uninstall -----------------------------------------------------------
if [[ "${1:-}" == "--uninstall" ]]; then
  echo "==> Stopping and removing $LABEL"
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  rm -f "$PLIST"
  echo "==> Done. Left in place: ./filebeat/ and ./filebeat-token-use.yml"
  exit 0
fi

# ---- arch / artifact -------------------------------------------------------
case "$(uname -m)" in
  arm64)  ARCH="darwin-aarch64" ;;
  x86_64) ARCH="darwin-x86_64" ;;
  *) echo "Unsupported CPU arch: $(uname -m)" >&2; exit 1 ;;
esac

FB_NAME="filebeat-${FB_VERSION}-${ARCH}"
FB_DIR="$REPO_DIR/filebeat/$FB_NAME"
FB_BIN="$FB_DIR/filebeat"
TARBALL="$REPO_DIR/filebeat/${FB_NAME}.tar.gz"
URL="https://artifacts.elastic.co/downloads/beats/filebeat/${FB_NAME}.tar.gz"

# ---- download + extract (idempotent) ---------------------------------------
if [[ -x "$FB_BIN" ]]; then
  echo "==> Filebeat $FB_VERSION already present -> $FB_DIR"
else
  mkdir -p "$REPO_DIR/filebeat"
  echo "==> Downloading $URL"
  curl -fSL --retry 3 -o "$TARBALL" "$URL"
  echo "==> Extracting -> $FB_DIR"
  tar -xzf "$TARBALL" -C "$REPO_DIR/filebeat"
  rm -f "$TARBALL"
  [[ -x "$FB_BIN" ]] || { echo "Extraction did not yield $FB_BIN" >&2; exit 1; }
fi

# ---- local config (created from example on first run) ----------------------
CONFIG="$REPO_DIR/filebeat-token-use.yml"
EXAMPLE="$REPO_DIR/deploy/filebeat-token-use.example.yml"
if [[ ! -f "$CONFIG" ]]; then
  cp "$EXAMPLE" "$CONFIG"
  chmod 0600 "$CONFIG"
  echo
  echo "==> Created $CONFIG from the example."
  echo "    The data stream / pipeline routing already match the Fleet integration."
  echo "    EDIT it and set TWO values (both are kept out of git):"
  echo "      1. the Elasticsearch host (output.elasticsearch.hosts):"
  echo "         hosts: [\"__ES_HOST__\"]  ->  [\"https://<deployment-id>.<region>.<csp>.elastic.cloud:443\"]"
  echo "      2. the API key (output.elasticsearch.api_key, id:key form):"
  echo "         api_key: \"__ID__:__API_KEY__\"   ->   \"<id>:<key>\""
  echo "    Get the host from \`elastic-agent inspect\` or the Cloud console; mint a"
  echo "    dedicated key in Kibana (see README), then re-run:"
  echo "      ./mac-filebeat-install.sh"
  exit 0
fi
# Filebeat refuses to load a config that is group/world writable.
chmod 0600 "$CONFIG"

if grep -q '__\(API_KEY\|ID\|ES_HOST\)__' "$CONFIG"; then
  echo "==> $CONFIG still has __ placeholders. Set the Elasticsearch host (__ES_HOST__)" >&2
  echo "    and API key (__ID__:__API_KEY__), then re-run." >&2
  exit 1
fi

# ---- validate config + output ----------------------------------------------
run_fb() { "$FB_BIN" -c "$CONFIG" --path.home "$FB_DIR" --path.config "$REPO_DIR" \
  --path.data "$HOME/Library/Application Support/token-use/filebeat-data" \
  --path.logs "$HOME/Library/Logs" "$@"; }

echo "==> Validating config"
run_fb test config
echo "==> Testing connection to Elasticsearch"
run_fb test output

# ---- register launchd agent ------------------------------------------------
mkdir -p "$AGENT_DIR" "$HOME/Library/Application Support/token-use/filebeat-data"
sed -e "s|__HOME__|$HOME|g" -e "s|__REPO__|$REPO_DIR|g" -e "s|__FB_DIR__|$FB_DIR|g" \
  "$REPO_DIR/deploy/$LABEL.plist" > "$PLIST"

echo "==> Loading launchd agent"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl enable "gui/$(id -u)/$LABEL"

echo "==> Done. Filebeat $FB_VERSION shipping token-use logs."
echo "    Status: launchctl print gui/$(id -u)/$LABEL"
echo "    Logs:   tail -f ~/Library/Logs/token-use-filebeat.err.log"
