#!/usr/bin/env bash
# Health check for the token-use pipeline. Reports two things:
#
#   1. The Rust collector daemon (token-use) that generates the NDJSON logs.
#   2. The log shipper picking those logs up: either a Fleet-managed Elastic
#      Agent, or this project's standalone Filebeat (mac-filebeat-install.sh).
#
# Dynamically detects macOS vs Linux and checks the matching service manager
# (launchd vs systemd) and default paths. Read-only: starts/changes nothing.
# Exit status: 0 if the collector AND at least one shipper are running, else 1.
#
#   Linux : systemd user unit (token-use.service) + Fleet elastic-agent
#   macOS : launchd agents (com.derickson.token-use[-filebeat]) + Fleet agent
#
# Paths mirror reset.sh / the deploy/ service definitions; override with the
# same TOKEN_USE_OUT_DIR env var if you customized the unit.
set -uo pipefail

OS="$(uname -s)"

# ---- pretty output ---------------------------------------------------------
if [[ -t 1 ]]; then
  R=$'\033[31m'; G=$'\033[32m'; Y=$'\033[33m'; B=$'\033[1m'; N=$'\033[0m'
else
  R=""; G=""; Y=""; B=""; N=""
fi
ok()   { echo "  ${G}●${N} $*"; }
bad()  { echo "  ${R}○${N} $*"; }
warn() { echo "  ${Y}●${N} $*"; }
info() { echo "    $*"; }
hdr()  { echo; echo "${B}$*${N}"; }

COLLECTOR_UP=1   # 0 = up   (shell-truthy via the final exit math)
SHIPPER_UP=1

# stat(1) is incompatible across platforms; wrap mtime + human age.
file_mtime() {
  case "$OS" in
    Darwin) stat -f %m "$1" 2>/dev/null ;;
    *)      stat -c %Y "$1" 2>/dev/null ;;
  esac
}
human_age() {  # seconds -> "3m12s ago"
  local s=$1
  if   (( s < 90 ));    then echo "${s}s ago"
  elif (( s < 5400 ));  then echo "$(( s / 60 ))m ago"
  elif (( s < 172800 ));then echo "$(( s / 3600 ))h ago"
  else echo "$(( s / 86400 ))d ago"; fi
}

# ---- OS-specific paths -----------------------------------------------------
case "$OS" in
  Linux)
    OUT_DIR="${TOKEN_USE_OUT_DIR:-$HOME/.local/share/token-use/logs}"
    ;;
  Darwin)
    OUT_DIR="${TOKEN_USE_OUT_DIR:-$HOME/Library/Application Support/token-use/logs}"
    ;;
  *)
    echo "Unsupported OS: $OS (Linux and macOS only)" >&2
    exit 1
    ;;
esac

echo "${B}token-use status${N}  —  $OS  ($(hostname -s 2>/dev/null || hostname))"

# ===========================================================================
# 1. Collector daemon (the Rust process generating the logs)
# ===========================================================================
hdr "1. Collector daemon (token-use)"

# Is the process actually alive? (-x: exact name, so the repo path doesn't match)
COLLECTOR_PID="$(pgrep -x token-use 2>/dev/null | head -n1)"
if [[ -n "$COLLECTOR_PID" ]]; then
  ok "process running (pid $COLLECTOR_PID)"
  COLLECTOR_UP=0
else
  bad "process NOT running"
fi

# Service-manager registration (separate from "is the pid alive").
case "$OS" in
  Linux)
    if command -v systemctl >/dev/null 2>&1; then
      state="$(systemctl --user is-active token-use.service 2>/dev/null)"
      case "$state" in
        active)  ok "systemd user unit: active" ;;
        "")      warn "systemd user unit: not found (not installed via install.sh?)" ;;
        *)       bad "systemd user unit: $state"
                 info "logs: journalctl --user -u token-use -e" ;;
      esac
    fi
    ;;
  Darwin)
    if launchctl print "gui/$(id -u)/com.derickson.token-use" >/dev/null 2>&1; then
      ok "launchd agent: loaded (com.derickson.token-use)"
    else
      warn "launchd agent: not loaded (not installed via install.sh?)"
    fi
    ;;
esac

# Output: does the NDJSON exist and is it fresh?
hdr "   Output NDJSON  ($OUT_DIR)"
if [[ -d "$OUT_DIR" ]]; then
  newest="$(ls -t "$OUT_DIR"/*.ndjson 2>/dev/null | head -n1)"
  if [[ -n "$newest" ]]; then
    now="$(date +%s)"
    mt="$(file_mtime "$newest")"
    lines="$(wc -l < "$newest" 2>/dev/null | tr -d ' ')"
    if [[ -n "$mt" ]]; then
      age=$(( now - mt ))
      msg="$(basename "$newest"): ${lines:-0} lines, updated $(human_age "$age")"
      # > ~2h stale is worth flagging, but it's normal when idle (no API calls).
      if (( age < 7200 )); then ok "$msg"; else warn "$msg (idle?)"; fi
    else
      ok "$(basename "$newest"): ${lines:-0} lines"
    fi
  else
    warn "no *.ndjson files yet (collector may not have backfilled / no usage)"
  fi
else
  bad "output dir does not exist"
fi

# ===========================================================================
# 2. Log shipper (Fleet Elastic Agent and/or standalone Filebeat)
# ===========================================================================
hdr "2. Log shipper"

# --- Fleet-managed Elastic Agent -------------------------------------------
echo "  ${B}Fleet Elastic Agent${N}"
AGENT_PID="$(pgrep -x elastic-agent 2>/dev/null | head -n1)"
if [[ -n "$AGENT_PID" ]]; then
  ok "elastic-agent running (pid $AGENT_PID)"
  SHIPPER_UP=0
  if command -v elastic-agent >/dev/null 2>&1; then
    info "detail: elastic-agent status"
  fi
else
  case "$OS" in
    Linux)  bad "elastic-agent not running" ;;
    Darwin) bad "elastic-agent not running (Fleet-enrolled Macs only)" ;;
  esac
fi

# --- Standalone Filebeat (this project) ------------------------------------
echo "  ${B}Standalone Filebeat${N}"
FB_PID="$(pgrep -x filebeat 2>/dev/null | head -n1)"
if [[ -n "$FB_PID" ]]; then
  ok "filebeat running (pid $FB_PID)"
  SHIPPER_UP=0
else
  bad "filebeat not running"
fi

case "$OS" in
  Darwin)
    if launchctl print "gui/$(id -u)/com.derickson.token-use-filebeat" >/dev/null 2>&1; then
      ok "launchd agent: loaded (com.derickson.token-use-filebeat)"
    else
      warn "launchd agent: not loaded (run ./mac-filebeat-install.sh to set up)"
    fi
    info "logs: tail -f ~/Library/Logs/token-use-filebeat.err.log"
    ;;
  Linux)
    if command -v systemctl >/dev/null 2>&1 && \
       systemctl is-active --quiet filebeat.service 2>/dev/null; then
      ok "systemd unit: active (filebeat.service)"
    fi
    ;;
esac

# ===========================================================================
# Verdict
# ===========================================================================
hdr "Summary"
if (( COLLECTOR_UP == 0 )); then ok "collector generating logs";  else bad "collector DOWN"; fi
if (( SHIPPER_UP == 0 ));   then ok "a shipper is picking them up"; else bad "no shipper running"; fi

(( COLLECTOR_UP == 0 && SHIPPER_UP == 0 ))
