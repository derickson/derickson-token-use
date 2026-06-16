#!/usr/bin/env bash
# Re-ship ALL token-use NDJSON through the standalone Filebeat (macOS).
#
# Stops the Filebeat launchd agent, deletes its registry (the read-position
# state), and restarts it. With no registry, Filebeat re-reads every existing
# *.ndjson from the start and re-sends each line — so after an ingest-pipeline or
# data-stream fix, the previously-shipped records are reprocessed and land in the
# correct place.
#
# This is the SHIPPER counterpart to ./reset.sh (which re-backfills the COLLECTOR
# by clearing its checkpoint). Use this one when the NDJSON on disk is already
# correct and you just need Elasticsearch to ingest it again.
#
# Note: token-use docs go to data streams, which auto-assign _id (no idempotent
# message_id key), so a re-ship can create duplicate documents for records that
# were ALREADY ingested successfully. Run this when the prior attempts failed
# (e.g. landed in the failure store) or when you explicitly want a full re-ingest.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS-only (the standalone Filebeat path is macOS)." >&2
  exit 1
fi

LABEL="com.derickson.token-use-filebeat"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
REGISTRY="$HOME/Library/Application Support/token-use/filebeat-data/registry"

if [[ ! -f "$PLIST" ]]; then
  echo "Filebeat service not installed ($PLIST missing)." >&2
  echo "Run ./mac-filebeat-install.sh first." >&2
  exit 1
fi

echo "==> Stopping Filebeat service"
launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true

echo "==> Deleting Filebeat registry -> $REGISTRY"
rm -rf "$REGISTRY"

echo "==> Restarting Filebeat (re-reads every *.ndjson from the start)"
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl enable "gui/$(id -u)/$LABEL"

echo "==> Done. Watch progress:"
echo "    tail -f ~/Library/Logs/token-use-filebeat.err.log"
