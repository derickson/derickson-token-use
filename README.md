# token-use

A small Rust daemon that watches local AI-tool transcripts and emits a
high-fidelity, **per-API-call** token-usage log in NDJSON — ready for an
Elasticsearch **filestream / Filebeat** integration to pick up.

It exists to close the gap Elastic's cloud integrations leave: token spend from
API calls made on **your own laptops and servers**. It ships with two
integrations — **Anthropic Claude Code** (tailing `~/.claude` transcripts) and the
**Hermes AI agent** (polling its `~/.hermes` SQLite state) — and the collector
layer is pluggable so OpenAI, Ollama, and others can be added later.

Every record identifies the **provider**, **model**, **service**, **host**, and
**project**, with a full prompt-vs-response token breakdown and derived
**throughput** metrics.

## What it captures

ECS-friendly record types share the NDJSON output (one file series per service),
distinguished by `event.dataset`. Canonical dimensions (`provider`, `model`,
`service.name`, `host.name`, `tokens.*`) use the same field names across services
so one Kibana view can slice every source.

| Dataset | One per | Highlights |
|---|---|---|
| `claude_code.token_usage` | API call (`message.id`) | prompt/response tokens, cache read/creation breakdown, `perf.tokens_per_sec` + `generation_ms`, `tools.use_count`/`names`, `stop_reason` |
| `claude_code.turn` | agent turn (`turn_duration`) | `turn.duration_ms`, summed `output_tokens`, effective `tokens_per_sec` |
| `hermes.token_usage` | Hermes session | per-session `input`/`output`/`cache_read`/`cache_creation(=cache write)`/`reasoning` tokens, `provider` (`billing_provider`, e.g. `openai-codex`), `model` (e.g. `gpt-5.5`), `hermes.persona`, `source`, cost + `duration_ms` |

### Hermes specifics
Hermes stores state in **SQLite** (`~/.hermes/state.db` plus one
`~/.hermes/profiles/<name>/state.db` per **persona**), so it can't be byte-tailed
like a text transcript — it's a **poll source** (see *How it works*). Its token
breakdown only exists as a per-session aggregate, so the grain is **one record per
session**, re-emitted with the latest totals as the session grows (key on
`hermes.session_id` for an idempotent upsert). `hermes.persona` is `default` for
the root DB or the profile directory name otherwise.

### Derived-metric methodology
- **Per-call** `generation_ms` = span between a response's first and last
  streamed content-block timestamps; `tokens_per_sec` = `output / generation_ms`.
  Single-block responses report `tokens_per_sec: null` (no divide-by-zero). This
  is **inference throughput**.
- **Turn** `tokens_per_sec` divides the turn's summed output tokens by the turn
  wall-clock — so it **includes tool-execution time** ("effective throughput"),
  deliberately a different number, kept in a separate dataset.

## How it works
There are two collector seams: a line-tailed **`Collector`** (Claude Code) and a
**`PollCollector`** for sources that don't fit byte-tailing (Hermes/SQLite).

**Claude Code (tail):**
- Watches `~/.claude/projects` with a debounced filesystem watcher (`notify`).
- Tails each transcript incrementally by **byte offset** (never re-reads), with
  partial-line and truncation handling.
- A single API response spans several transcript lines that repeat the same
  `usage`; the collector **accumulates by `message.id` and finalizes on the next
  boundary**, so each call is counted **exactly once** (verified: emitted call
  count == distinct `message.id` count).

**Hermes (poll):**
- Each persona DB is opened **read-only**; the append-only `messages.id` is the
  incremental **cursor** (stored in the same checkpoint as the file offsets).
- A poll re-derives the snapshot only for sessions that gained a message since the
  cursor, then advances the cursor to the new high-water mark — so a quiet poll
  emits nothing (verified: emitted record count == distinct active sessions; a
  re-run with no new activity adds zero lines).

On first run it **backfills** all existing transcripts and Hermes sessions, then
runs live. A ~5-minute tick rescans transcripts for missed events / new files,
**polls the Hermes DBs**, and checkpoints state.

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
| `TOKEN_USE_HERMES_DIR` | `$HOME/.hermes` | Hermes install dir (the SQLite poll source) |
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
    mod.rs             Collector + PollCollector traits (the per-service seams)
    claude_code.rs     Anthropic Claude Code collector (line tailer)
    hermes.rs          Hermes collector (SQLite poll source)
  daemon.rs            backfill + watch loop + tick (+ poll sources)
  config.rs main.rs error.rs
deploy/                systemd unit, launchd plist, filebeat example
install.sh             build + install the service (Linux/macOS)
```
