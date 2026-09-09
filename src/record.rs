//! Output record schema — the contract with Elasticsearch.
//!
//! Two ECS-friendly record types share one NDJSON stream, distinguished by
//! `event.dataset`:
//!   * [`CallRecord`] — one per API call (`message.id`), with token breakdown and
//!     the derived per-call throughput metrics.
//!   * [`TurnRecord`] — one per agent "turn", with turn duration and effective
//!     throughput (which includes tool-execution wall-time).
//!
//! Canonical cross-service dimensions (`provider`, `model`, `service.name`,
//! `host.name`) use the same field names for every collector so a single
//! Kibana view can slice across Anthropic / OpenAI / Ollama.

use serde::Serialize;

/// Discriminated union written to the stream; serializes as the inner record.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum OutputRecord {
    Call(Box<CallRecord>),
    Turn(Box<TurnRecord>),
    Hermes(Box<HermesRecord>),
}

impl OutputRecord {
    /// ISO date (`YYYY-MM-DD`, UTC) used to route the record to its daily file.
    pub fn date(&self) -> &str {
        match self {
            OutputRecord::Call(c) => &c.date,
            OutputRecord::Turn(t) => &t.date,
            OutputRecord::Hermes(h) => &h.date,
        }
    }

    /// `service.name`, used to select the output writer.
    pub fn service_name(&self) -> &'static str {
        match self {
            OutputRecord::Call(c) => c.service.name,
            OutputRecord::Turn(t) => t.service.name,
            OutputRecord::Hermes(h) => h.service.name,
        }
    }

    /// Idempotency key for cross-restart dedup. Calls dedup on the provider's
    /// per-response id (`message.id` for Claude Code, `response_id` for Codex);
    /// turns have no stable key (offset checkpointing covers them). Hermes
    /// records return `None` *by design*: a session is re-emitted as it grows,
    /// so the in-process recent-id ring must not suppress legitimate updates —
    /// the per-DB cursor already gates re-emits and ES upserts by `_id`.
    pub fn dedup_key(&self) -> Option<&str> {
        match self {
            OutputRecord::Call(c) => match (&c.claude, &c.codex) {
                (Some(m), _) => Some(&m.message_id),
                (_, Some(m)) => Some(&m.response_id),
                _ => None,
            },
            OutputRecord::Turn(_) => None,
            OutputRecord::Hermes(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub dataset: &'static str,
    pub module: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Service {
    pub name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Host {
    pub name: String,
}

/// Per-call token-usage record (`event.dataset = "<service>.token_usage"`).
#[derive(Debug, Clone, Serialize)]
pub struct CallRecord {
    /// Routing date; not serialized into the document body.
    #[serde(skip)]
    pub date: String,

    #[serde(rename = "@timestamp")]
    pub timestamp: String,
    pub ingested_at: String,
    pub event: Event,
    pub service: Service,
    pub provider: &'static str,
    pub model: String,
    pub host: Host,
    /// Provider-specific metadata: exactly one of `claude`/`codex` is set, named
    /// per source so a new collector never reshapes an existing one's fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexMeta>,
    pub tokens: Tokens,
    /// Per-call inference throughput, when the source records enough timing to
    /// derive it. Absent for providers whose transcripts carry a single
    /// timestamp per response (Codex) — see [`Perf`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perf: Option<Perf>,
    pub tools: Tools,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUse>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeMeta {
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Duplicate of the top-level `model` kept for provider-specific fidelity.
    pub model: String,
    pub session_id: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub is_sidechain: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
}

/// Codex CLI per-call metadata, from a `token_usage_record` rollout line plus
/// the session/turn context that precedes it.
///
/// `project` holds the session `cwd`, deliberately the same field name and
/// meaning as [`ClaudeMeta::project`], so a Kibana view can group by project
/// across both services.
#[derive(Debug, Clone, Serialize)]
pub struct CodexMeta {
    /// OpenAI response id — the per-call idempotency key, parallel to Claude's
    /// `message_id`.
    pub response_id: String,
    /// Duplicate of the top-level `model` kept for provider-specific fidelity.
    pub model: String,
    pub session_id: String,
    pub thread_id: String,
    pub turn_id: String,
    /// Differs from `turn_id` when the call belongs to a nested (sub-agent) turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_turn_id: Option<String>,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Which Codex surface produced the session, e.g. `"Codex Desktop"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub originator: Option<String>,
    /// Reasoning effort in force for the turn (`"low"`/`"medium"`/`"high"`) — a
    /// first-order driver of `tokens.reasoning`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Token breakdown. Prompt side = `input + cache_read_input + cache_creation_input`
/// (surfaced as `total_input`); response side = `output`.
#[derive(Debug, Clone, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read_input: u64,
    pub cache_creation_input: u64,
    pub cache_creation_ephemeral_5m_input: u64,
    pub cache_creation_ephemeral_1h_input: u64,
    pub total_input: u64,
    pub total: u64,
    /// Reasoning/"thinking" output tokens, when the provider reports them
    /// separately (Hermes/OpenAI). Omitted for providers that don't (Claude Code).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
}

/// Derived per-call inference throughput.
///
/// This needs a *within-response* time window, which only exists when the source
/// timestamps a response's individual content blocks (Claude Code does). Codex
/// rollouts carry one timestamp per line, so the whole struct is omitted there
/// rather than reported as a fabricated zero — turn-level `duration_ms`, which
/// Codex does record, remains available on [`TurnRecord`].
#[derive(Debug, Clone, Serialize)]
pub struct Perf {
    /// `max(block_ts) - min(block_ts)` in ms across the call's content blocks.
    pub generation_ms: i64,
    /// `output / (generation_ms/1000)`; `None` when `generation_ms == 0`
    /// (e.g. single-block responses), to avoid divide-by-zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_sec: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tools {
    pub use_count: u32,
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerToolUse {
    pub web_search_requests: u64,
    pub web_fetch_requests: u64,
}

/// Per-turn record (`event.dataset = "<service>.turn"`).
#[derive(Debug, Clone, Serialize)]
pub struct TurnRecord {
    #[serde(skip)]
    pub date: String,

    #[serde(rename = "@timestamp")]
    pub timestamp: String,
    pub ingested_at: String,
    pub event: Event,
    pub service: Service,
    pub provider: &'static str,
    pub host: Host,
    /// Provider-specific metadata; exactly one is set, as on [`CallRecord`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude: Option<TurnMeta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexTurnMeta>,
    pub turn: Turn,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnMeta {
    pub session_id: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
}

/// Codex per-turn metadata, from a `task_complete` event and its `turn_context`.
#[derive(Debug, Clone, Serialize)]
pub struct CodexTurnMeta {
    pub session_id: String,
    pub turn_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_turn_id: Option<String>,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub originator: Option<String>,
    /// Latency to the turn's first output token, as reported by Codex. Codex
    /// specific: Claude Code's turn boundary carries no equivalent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_to_first_token_ms: Option<i64>,
}

/// Turn-level metrics. `tokens_per_sec` here is *effective* throughput — it
/// includes tool-execution wall-time, deliberately distinct from per-call
/// inference throughput.
#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    pub duration_ms: i64,
    pub message_count: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_sec: Option<f64>,
}

// --- Hermes -----------------------------------------------------------------

/// Per-session token-usage record for the Hermes agent
/// (`event.dataset = "hermes.token_usage"`).
///
/// Hermes keeps its token breakdown per *session* (its `messages` table carries
/// no usable per-message counts), so this is one record per session, re-emitted
/// with the latest aggregate as the session grows. `provider` is dynamic here —
/// it carries the session's `billing_provider` (e.g. `"openai-codex"`) — unlike
/// the static `provider` on Claude Code records.
#[derive(Debug, Clone, Serialize)]
pub struct HermesRecord {
    #[serde(skip)]
    pub date: String,

    #[serde(rename = "@timestamp")]
    pub timestamp: String,
    pub ingested_at: String,
    pub event: Event,
    pub service: Service,
    pub provider: String,
    pub model: String,
    pub host: Host,
    pub hermes: HermesMeta,
    pub tokens: Tokens,
}

#[derive(Debug, Clone, Serialize)]
pub struct HermesMeta {
    pub session_id: String,
    /// `"default"` for `~/.hermes/state.db`, else the profile directory name.
    pub persona: String,
    /// `true` once the session has an `ended_at`.
    pub ended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_call_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<u64>,
    /// `ended_at - started_at` in ms, when the session has ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

/// Compute tokens/sec from an output-token count and a duration in milliseconds,
/// returning `None` for non-positive durations.
pub fn tokens_per_sec(output_tokens: u64, duration_ms: i64) -> Option<f64> {
    if duration_ms <= 0 {
        return None;
    }
    Some(output_tokens as f64 / (duration_ms as f64 / 1000.0))
}
