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
See [`deploy/filebeat-token-use.yml`](deploy/filebeat-token-use.yml) for a
filestream input. Recommended: an ingest pipeline that sets `_id` to
`claude.message_id` for call records, making re-ingestion idempotent.

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
deploy/                systemd unit, launchd plist, filebeat example
install.sh             build + install the service (Linux/macOS)
```
