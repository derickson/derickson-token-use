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
}

impl OutputRecord {
    /// ISO date (`YYYY-MM-DD`, UTC) used to route the record to its daily file.
    pub fn date(&self) -> &str {
        match self {
            OutputRecord::Call(c) => &c.date,
            OutputRecord::Turn(t) => &t.date,
        }
    }

    /// `service.name`, used to select the output writer.
    pub fn service_name(&self) -> &'static str {
        match self {
            OutputRecord::Call(c) => c.service.name,
            OutputRecord::Turn(t) => t.service.name,
        }
    }

    /// Idempotency key for cross-restart dedup. Calls dedup on `message.id`;
    /// turns have no stable key (offset checkpointing covers them).
    pub fn dedup_key(&self) -> Option<&str> {
        match self {
            OutputRecord::Call(c) => Some(&c.claude.message_id),
            OutputRecord::Turn(_) => None,
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
    pub claude: ClaudeMeta,
    pub tokens: Tokens,
    pub perf: Perf,
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
}

/// Derived per-call inference throughput.
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
    pub claude: TurnMeta,
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

/// Compute tokens/sec from an output-token count and a duration in milliseconds,
/// returning `None` for non-positive durations.
pub fn tokens_per_sec(output_tokens: u64, duration_ms: i64) -> Option<f64> {
    if duration_ms <= 0 {
        return None;
    }
    Some(output_tokens as f64 / (duration_ms as f64 / 1000.0))
}
