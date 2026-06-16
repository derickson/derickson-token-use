# token-use

A small Rust daemon that watches local AI-tool transcripts and emits a
high-fidelity, **per-API-call** token-usage log in NDJSON — ready for an
Elasticsearch **filestream / Filebeat** integration to pick up.

It exists to close the gap Elastic's cloud integrations leave: token spend from
API calls made on **your own laptops and servers**. The first integration is
**Anthropic Claude Code** (reading `~/.claude`); the collector layer is pluggable
so OpenAI and Ollama can be added later.

Every record identifies the **provider**, **model**, **service**, **host**, and
**project**, with a full prompt-vs-response token breakdown and derived
**throughput** metrics.

## What it captures

Two ECS-friendly record types share one NDJSON stream, distinguished by
`event.dataset`:

| Dataset | One per | Highlights |
|---|---|---|
| `claude_code.token_usage` | API call (`message.id`) | prompt/response tokens, cache read/creation breakdown, `perf.tokens_per_sec` + `generation_ms`, `tools.use_count`/`names`, `stop_reason` |
| `claude_code.turn` | agent turn (`turn_duration`) | `turn.duration_ms`, summed `output_tokens`, effective `tokens_per_sec` |

### Derived-metric methodology
- **Per-call** `generation_ms` = span between a response's first and last
  streamed content-block timestamps; `tokens_per_sec` = `output / generation_ms`.
  Single-block responses report `tokens_per_sec: null` (no divide-by-zero). This
  is **inference throughput**.
- **Turn** `tokens_per_sec` divides the turn's summed output tokens by the turn
  wall-clock — so it **includes tool-execution time** ("effective throughput"),
  deliberately a different number, kept in a separate dataset.

## How it works
- Watches `~/.claude/projects` with a debounced filesystem watcher (`notify`).
- Tails each transcript incrementally by **byte offset** (never re-reads), with
  partial-line and truncation handling.
- A single API response spans several transcript lines that repeat the same
  `usage`; the collector **accumulates by `message.id` and finalizes on the next
  boundary**, so each call is counted **exactly once** (verified: emitted call
  count == distinct `message.id` count).
- On first run it **backfills** all existing transcripts, then runs live. A
  ~5-minute tick rescans for missed events / new files and checkpoints state.

## Install (Linux & macOS)

### Prerequisites: the Rust toolchain
Building the collector needs `cargo`/`rustc` (Rust 2021, stable). The one-liner
below works on both Linux and macOS; if `cargo` is already on your `PATH` you can
skip it.

```bash
# Linux & macOS — official installer (installs rustup + the stable toolchain)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"      # add cargo to PATH for the current shell
```

Or use your package manager instead:
- **macOS** — `brew install rustup-init && rustup-init` (or `brew install rust`).
- **Linux (Debian/Ubuntu)** — `sudo apt install rustup && rustup default stable`
  (older releases: `sudo apt install cargo`); **Fedora** — `sudo dnf install cargo`;
  **Arch** — `sudo pacman -S rust`.

Verify with `cargo --version`. On macOS the build also needs the Command Line
Tools (`xcode-select --install`) for the system linker.

### Build + register the service
```bash
./install.sh
```
This builds `--release`, installs the binary to `~/.local/bin/token-use`, and
registers a background service:
- **Linux** — a systemd **user** unit (`systemctl --user … token-use`), with
  lingering enabled so it runs without an active login.
- **macOS** — a launchd agent (`RunAtLoad` + `KeepAlive`).

Output NDJSON lands in:
- Linux: `~/.local/share/token-use/logs/`
- macOS: `~/Library/Application Support/token-use/logs/`

### Run manually
```bash
TOKEN_USE_OUT_DIR=./logs cargo run --release
```

### Check it's running
```bash
./status.sh
```
`status.sh` auto-detects macOS vs Linux and reports the health of the whole
pipeline: the collector daemon (process + launchd/systemd registration), the
freshness of the output NDJSON, and which shipper — a Fleet-managed Elastic
Agent and/or the standalone Filebeat below — is picking the logs up. It changes
nothing and exits non-zero if the collector or all shippers are down (handy for
`&&` chaining or monitoring).

## Configuration (environment)
| Var | Default | Purpose |
|---|---|---|
| `TOKEN_USE_OUT_DIR` | `./logs` | NDJSON output directory |
| `TOKEN_USE_STATE_DIR` | XDG state / App Support | checkpoint (`state.json`) |
| `TOKEN_USE_HOME` | `$HOME` | locate `~/.claude` (handy for testing) |
| `TOKEN_USE_DEBOUNCE_MS` | `1500` | FS-event debounce window |
| `TOKEN_USE_TICK_SECS` | `300` | safety-net rescan / checkpoint interval |
| `RUST_LOG` | `info` | operational log level (stderr) |

## Elasticsearch ingestion
On Fleet-managed machines, an Elastic Agent custom-logs integration tails the
NDJSON output and ships it. The records carry `event.dataset`
(`claude_code.token_usage` / `claude_code.turn`) and `event.module: token-use`, and
belong in per-dataset data streams `logs-<event.dataset>-<namespace>`.
See [`deploy/filebeat-token-use.example.yml`](deploy/filebeat-token-use.example.yml)
for the standalone shipper, which routes there directly (see
[Data stream routing](#data-stream-routing)).

### Un-Fleet-managed Macs: standalone Filebeat
For Macs that **cannot be enrolled in Fleet**, `mac-filebeat-install.sh` ships the
same logs to the same Elastic cluster with the **same NDJSON preprocessing**, using
a self-contained Filebeat that lives in this project directory and is driven by a
local yml.

```bash
./mac-filebeat-install.sh        # 1st run: downloads Filebeat, creates local config, stops
#   edit ./filebeat-token-use.yml  (set the API key; host/routing pre-filled)
./mac-filebeat-install.sh        # 2nd run: validates, registers launchd service
./mac-filebeat-install.sh --uninstall   # stop + remove the service
./mac-reprocess.sh               # reset the registry + re-ship every *.ndjson
```

Use `mac-reprocess.sh` after an ingest-pipeline / data-stream fix to re-ingest
records that were already shipped (it clears the Filebeat registry so every line
is re-read). It is the shipper-side counterpart to `reset.sh` (which re-backfills
the collector). Because data-stream docs get an auto-assigned `_id`, a re-ship can
duplicate records that were *already* ingested successfully — run it when the
prior attempts failed (e.g. failure store) or you want a deliberate full re-ingest.

What it does:
- Downloads a pinned Filebeat (**9.4.2**, matching the Fleet agent and the 9.x
  stack) into `./filebeat/` (gitignored).
- Creates `./filebeat-token-use.yml` (gitignored, `chmod 0600`) from
  [`deploy/filebeat-token-use.example.yml`](deploy/filebeat-token-use.example.yml).
- Registers a **launchd agent** (`com.derickson.token-use-filebeat`, `RunAtLoad` +
  `KeepAlive`) so Filebeat runs at boot without a login.

The example is pre-filled from a Fleet-managed host's `elastic-agent inspect`:
- same NDJSON parser (`target: ""`, `overwrite_keys`, `add_error_key`),
- same Elastic Cloud `output.elasticsearch.hosts`,
- direct routing to `logs-<event.dataset>-dericksontokenuse`
  (see [Data stream routing](#data-stream-routing)).

The **only** thing you supply is a **dedicated** API key (do *not* reuse the Fleet
agent's key). In Kibana → *Stack Management → API keys*, create one whose role
descriptor allows writing the token-use data streams, e.g.:

```json
{ "token-use-filebeat": {
  "cluster": ["monitor"],
  "indices": [ {
    "names": ["logs-claude_code.*-dericksontokenuse"],
    "privileges": ["auto_configure", "create_doc"] } ] } }
```

The `cluster: ["monitor"]` privilege is required: Filebeat calls `GET /` for a
version check at startup (and `filebeat test output` does the same). Without it
you get `403 ... action [cluster:monitor/main] is unauthorized`. The `indices`
block alone is enough to *write* data but not to pass that check.

Paste it into `filebeat-token-use.yml` in `id:key` form, then re-run
`./mac-filebeat-install.sh`.

### Data stream routing
The shipper writes each record straight to its final data stream by
`event.dataset` — `output.elasticsearch.index: "logs-%{[event.dataset]}-dericksontokenuse"`
— so `claude_code.turn` → `logs-claude_code.turn-dericksontokenuse`, etc. These are
auto-created by the stock `logs-*-*` template (permissive mappings), so
`event.module: token-use` is accepted.

This **bypasses** the `filestream.generic` data stream on purpose: that's the
Custom Logs package stream, and its mapping pins `event.module` to the constant
`filestream`, so any record written there with `event.module: token-use` is
rejected (`document_parsing_exception`) and dead-letters into the
`.fs-…` failure store. A Fleet *custom-logs* integration that writes to
`filestream.generic` hits the same wall and needs either a `reroute` ingest
pipeline (`{ "reroute": { "dataset": "{{{event.dataset}}}" } }`) or a dedicated
dataset — the standalone shipper just routes correctly to begin with.

> **No idempotent `_id`.** Data streams reject a client- or pipeline-set `_id`, so
> re-ingestion is **not** deduped by `message.id`. Each line is shipped once
> (collector checkpoint + Filebeat registry); duplicates only occur if the registry
> is reset (e.g. `mac-reprocess.sh`) and the same lines are re-read.

## Development
```bash
cargo test     # unit + accumulation/dedup/metrics + tailer + state + output
```

## Layout
```
src/
  record.rs            output schema (the contract)
  tailer.rs            generic incremental file tailer
  state.rs            durable checkpoint (offsets + recent-id guard)
  output.rs            dated NDJSON writer (daily rotation)
  collector/
    mod.rs             Collector trait (the per-service seam)
    claude_code.rs     Anthropic Claude Code collector
  daemon.rs            backfill + watch loop + tick
  config.rs main.rs error.rs
deploy/                systemd unit, launchd plists, standalone filebeat example
install.sh             build + install the collector service (Linux/macOS)
status.sh              health check: collector + shipper (Linux/macOS)
reset.sh               stop, wipe output + checkpoint, restart (full backfill)
mac-filebeat-install.sh  standalone Filebeat shipper for un-Fleet-able Macs
mac-reprocess.sh         reset the Filebeat registry + re-ship all NDJSON
```
