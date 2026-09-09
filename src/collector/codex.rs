//! OpenAI Codex CLI collector.
//!
//! Reads `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<session-uuid>.jsonl`
//! rollout logs, which are append-only and byte-tailable like a Claude Code
//! transcript — so this is a line-consuming [`Collector`], not a
//! [`PollCollector`](crate::collector::PollCollector).
//!
//! Unlike Claude Code, Codex writes a **complete** `token_usage_record` line per
//! API response, so no usage accumulation is needed: that line *is* the call
//! boundary. What we accumulate instead is the context around it, because it
//! carries only ids and counts:
//!
//!   * `session_meta` (first line) — `cwd`, `originator`, `cli_version`,
//!     git branch, and a fallback model name.
//!   * `turn_context` — the authoritative **per-turn** model and reasoning
//!     effort. Codex can switch models mid-session, so the model is resolved per
//!     turn and only falls back to `session_meta`'s.
//!   * `response_item` — the tool calls of a response, written *before* the
//!     `token_usage_record` that closes it, hence buffered and attached to the
//!     next call emitted.
//!   * `event_msg` / `task_started` + `task_complete` — turn boundaries.
//!     `task_complete` carries an explicit `duration_ms`, the direct analogue of
//!     Claude Code's `turn_duration` line, and yields a [`TurnRecord`].
//!
//! Turns are tracked in a map keyed by `turn_id` so a nested (sub-agent) turn's
//! calls cannot reset the parent turn's accumulator.
//!
//! Two deliberate differences from the Claude Code collector:
//!
//!   * **No `perf`.** A rollout line has a single timestamp, so a response's
//!     *internal* generation window is not recoverable. `perf` is omitted rather
//!     than fabricated as a zero that would drag down cross-provider averages;
//!     turn-level `duration_ms` is real and still emitted. Consequently a call's
//!     `@timestamp` is the response *completion* time (Claude Code's is the
//!     first block's timestamp).
//!   * **Only `token_usage_record` is read.** Codex releases before ~0.15 logged
//!     per-call usage in `event_msg`/`token_count` events instead, with no
//!     `response_id` to dedup on. Current releases write *both*, so consuming
//!     both would double-count; such legacy rollouts are simply not captured.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;
use walkdir::WalkDir;

use crate::error::ParseError;
use crate::record::*;

const PROVIDER: &str = "openai";
const SERVICE: &str = "codex";

/// Session-wide context, from `session_meta` (one per rollout file).
#[derive(Default)]
struct SessionState {
    session_id: String,
    project: String,
    git_branch: Option<String>,
    originator: Option<String>,
    cli_version: Option<String>,
    /// `base_instructions.provenance.model` — the fallback when a call's turn
    /// has no `turn_context`.
    fallback_model: Option<String>,
    /// Open turns by `turn_id`; nested sub-agent turns coexist with their parent.
    turns: HashMap<String, OpenTurn>,
    /// Most recently started turn, used to attribute items that carry no turn id.
    current_turn: Option<String>,
    /// Tool calls seen since the last `token_usage_record`, awaiting the call
    /// they belong to.
    pending: PendingTools,
}

/// A turn between its `task_started`/`turn_context` and its `task_complete`.
#[derive(Default)]
struct OpenTurn {
    root_turn_id: Option<String>,
    /// Per-turn model and effort from `turn_context`, authoritative over
    /// `session_meta` because Codex can switch models mid-session.
    model: Option<String>,
    reasoning_effort: Option<String>,
    project: Option<String>,
    /// Transcript items recorded in the turn (messages, reasoning, tool calls
    /// and their outputs) — the closest analogue to Claude Code's `messageCount`.
    item_count: u64,
    /// Latest `turn_token_usage.output_tokens`, which Codex maintains as a
    /// running total for the turn; preferred over summing our own per-call view,
    /// so a daemon that starts mid-turn still reports the true turn total.
    output_tokens: u64,
    /// Whether any `token_usage_record` was seen for this turn. A turn without
    /// one carries no token data at all (a legacy rollout, or a daemon that
    /// attached after the turn's last call), so it is dropped rather than
    /// emitted with a fabricated zero.
    saw_usage: bool,
}

#[derive(Default)]
struct PendingTools {
    /// Call ids already counted, so a repeated item cannot double-count.
    ids: BTreeSet<String>,
    names: BTreeSet<String>,
    count: u32,
    web_search: u64,
}

pub struct CodexCollector {
    watch_root: PathBuf,
    host: String,
    /// One rollout file = one session.
    sessions: HashMap<PathBuf, SessionState>,
}

impl CodexCollector {
    pub fn new(codex_dir: &Path, host: String) -> Self {
        CodexCollector {
            watch_root: codex_dir.join("sessions"),
            host,
            sessions: HashMap::new(),
        }
    }

    fn now_iso() -> String {
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// Session state for `path`, **primed from the file's own `session_meta`**
    /// line the first time it is touched.
    ///
    /// Priming matters on restart: the tailer resumes at a checkpointed byte
    /// offset, so `session_meta` (always line 1) would otherwise never be
    /// replayed and every remaining call in that session would lose its `cwd`,
    /// branch, and fallback model.
    fn session(&mut self, path: &Path) -> &mut SessionState {
        if !self.sessions.contains_key(path) {
            let mut s = SessionState::default();
            if let Some(meta) = read_session_meta(path) {
                apply_session_meta(&mut s, &meta);
            }
            self.sessions.insert(path.to_path_buf(), s);
        }
        self.sessions.get_mut(path).expect("just inserted")
    }

    /// Absorb the `session_meta` line's session-wide context.
    fn handle_session_meta(&mut self, path: &Path, p: &Value) {
        apply_session_meta(self.session(path), p);
    }

    /// Absorb a `turn_context` line: opens the turn if new, and records the model
    /// and reasoning effort in force for it.
    fn handle_turn_context(&mut self, path: &Path, p: &Value) {
        let Some(turn_id) = str_field(p, "turn_id") else {
            return;
        };
        let root = str_field(p, "root_turn_id");
        let model = str_field(p, "model");
        let effort = str_field(p, "effort");
        let cwd = str_field(p, "cwd");

        let s = self.session(path);
        s.current_turn = Some(turn_id.clone());
        let turn = s.turns.entry(turn_id).or_default();
        turn.root_turn_id = root.or_else(|| turn.root_turn_id.take());
        turn.model = model.or_else(|| turn.model.take());
        turn.reasoning_effort = effort.or_else(|| turn.reasoning_effort.take());
        turn.project = cwd.or_else(|| turn.project.take());
    }

    /// Count a transcript item toward its turn and buffer any tool call it makes.
    ///
    /// Tool calls are `*_call` item types (`function_call`, `custom_tool_call`,
    /// `web_search_call`, …) and are written *before* the `token_usage_record`
    /// of the response that requested them, so they belong to the next call.
    fn handle_response_item(&mut self, path: &Path, p: &Value) {
        let item_type = p.get("type").and_then(Value::as_str).unwrap_or_default();
        let turn_id = p
            .get("internal_chat_message_metadata_passthrough")
            .and_then(|m| str_field(m, "turn_id"));

        let s = self.session(path);
        // Most items carry no turn id; attribute those to the current turn.
        let target = turn_id.or_else(|| s.current_turn.clone());
        if let Some(turn) = target.and_then(|id| s.turns.get_mut(&id)) {
            turn.item_count += 1;
        }

        if !item_type.ends_with("_call") {
            return;
        }
        let id = str_field(p, "call_id")
            .or_else(|| str_field(p, "id"))
            .unwrap_or_else(|| item_type.to_string());
        if !s.pending.ids.insert(id) {
            return;
        }
        s.pending.count += 1;
        s.pending
            .names
            .insert(str_field(p, "name").unwrap_or_else(|| item_type.to_string()));
        if item_type == "web_search_call" {
            s.pending.web_search += 1;
        }
    }

    /// Build a [`CallRecord`] from a `token_usage_record` line.
    fn handle_usage(&mut self, path: &Path, ts: DateTime<Utc>, p: &Value) -> Option<OutputRecord> {
        let response_id = str_field(p, "response_id")?;
        let u = parse_usage(p.get("usage"))?;
        let turn_id = str_field(p, "turn_id").unwrap_or_default();

        let host = self.host.clone();
        let s = self.session(path);

        // Roll the turn's running output total forward from Codex's own count.
        // The turn is created here if its `task_started` was never seen (a
        // restart resuming mid-turn); `turn_token_usage` is cumulative, so the
        // total stays correct even though we joined late.
        let turn = s.turns.entry(turn_id.clone()).or_default();
        turn.saw_usage = true;
        if let Some(t) = parse_usage(p.get("turn_token_usage")) {
            turn.output_tokens = t.output;
        }

        let turn = s.turns.get(&turn_id);
        let turn_model = turn.and_then(|t| t.model.clone());
        let turn_project = turn.and_then(|t| t.project.clone());
        let reasoning_effort = turn.and_then(|t| t.reasoning_effort.clone());

        let model = turn_model
            .or_else(|| s.fallback_model.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let project = turn_project.unwrap_or_else(|| s.project.clone());

        let tools = std::mem::take(&mut s.pending);

        let rec = CallRecord {
            date: ts.format("%Y-%m-%d").to_string(),
            timestamp: ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ingested_at: Self::now_iso(),
            event: Event {
                dataset: "codex.token_usage",
                module: "token-use",
            },
            service: Service {
                name: SERVICE,
                version: s.cli_version.clone(),
            },
            provider: PROVIDER,
            model: model.clone(),
            host: Host { name: host },
            claude: None,
            codex: Some(CodexMeta {
                response_id,
                model,
                session_id: str_field(p, "session_id").unwrap_or_else(|| s.session_id.clone()),
                thread_id: str_field(p, "thread_id").unwrap_or_default(),
                turn_id,
                root_turn_id: str_field(p, "root_turn_id"),
                project,
                git_branch: s.git_branch.clone(),
                originator: s.originator.clone(),
                reasoning_effort,
            }),
            tokens: u.to_tokens(),
            perf: None,
            tools: Tools {
                use_count: tools.count,
                names: tools.names.into_iter().collect(),
            },
            server_tool_use: if tools.web_search > 0 {
                Some(ServerToolUse {
                    web_search_requests: tools.web_search,
                    web_fetch_requests: 0,
                })
            } else {
                None
            },
        };
        Some(OutputRecord::Call(Box::new(rec)))
    }

    /// Build a [`TurnRecord`] from a `task_complete` event and close the turn.
    fn handle_task_complete(
        &mut self,
        path: &Path,
        ts: DateTime<Utc>,
        p: &Value,
    ) -> Option<OutputRecord> {
        let turn_id = str_field(p, "turn_id")?;
        let duration_ms = p.get("duration_ms").and_then(Value::as_i64).unwrap_or(0);
        let ttft = p.get("time_to_first_token_ms").and_then(Value::as_i64);

        let host = self.host.clone();
        let s = self.session(path);
        let turn = s.turns.remove(&turn_id).unwrap_or_default();
        if s.current_turn.as_deref() == Some(turn_id.as_str()) {
            s.current_turn = None;
        }
        if !turn.saw_usage {
            return None; // no token data for this turn — see `OpenTurn::saw_usage`
        }
        let output_tokens = turn.output_tokens;

        let rec = TurnRecord {
            date: ts.format("%Y-%m-%d").to_string(),
            timestamp: ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ingested_at: Self::now_iso(),
            event: Event {
                dataset: "codex.turn",
                module: "token-use",
            },
            service: Service {
                name: SERVICE,
                version: s.cli_version.clone(),
            },
            provider: PROVIDER,
            host: Host { name: host },
            claude: None,
            codex: Some(CodexTurnMeta {
                session_id: s.session_id.clone(),
                turn_id,
                root_turn_id: turn.root_turn_id,
                project: turn.project.unwrap_or_else(|| s.project.clone()),
                git_branch: s.git_branch.clone(),
                model: turn.model.or_else(|| s.fallback_model.clone()),
                originator: s.originator.clone(),
                time_to_first_token_ms: ttft,
            }),
            turn: Turn {
                duration_ms,
                message_count: turn.item_count,
                output_tokens,
                tokens_per_sec: tokens_per_sec(output_tokens, duration_ms),
            },
        };
        Some(OutputRecord::Turn(Box::new(rec)))
    }

    fn handle_event_msg(
        &mut self,
        path: &Path,
        ts: DateTime<Utc>,
        p: &Value,
    ) -> Option<OutputRecord> {
        match p.get("type").and_then(Value::as_str) {
            Some("task_started") => {
                // Opens a fresh turn, discarding any stale entry under the same id.
                if let Some(turn_id) = str_field(p, "turn_id") {
                    let s = self.session(path);
                    s.turns.insert(turn_id.clone(), OpenTurn::default());
                    s.current_turn = Some(turn_id);
                }
                None
            }
            Some("task_complete") => self.handle_task_complete(path, ts, p),
            _ => None,
        }
    }
}

impl crate::collector::Collector for CodexCollector {
    fn name(&self) -> &'static str {
        SERVICE
    }
    fn provider(&self) -> &'static str {
        PROVIDER
    }
    fn watch_root(&self) -> &Path {
        &self.watch_root
    }
    fn owns(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path.starts_with(&self.watch_root)
    }
    fn enumerate(&self) -> Vec<PathBuf> {
        if !self.watch_root.exists() {
            return Vec::new();
        }
        WalkDir::new(&self.watch_root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect()
    }

    fn consume_line(&mut self, path: &Path, line: &str) -> Result<Vec<OutputRecord>, ParseError> {
        let v: Value = serde_json::from_str(line)?;
        let Some(p) = v.get("payload") else {
            return Ok(Vec::new()); // not a rollout envelope
        };
        let ts = v.get("timestamp").and_then(parse_ts);

        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => self.handle_session_meta(path, p),
            Some("turn_context") => self.handle_turn_context(path, p),
            Some("response_item") => self.handle_response_item(path, p),
            Some("token_usage_record") => {
                if let Some(ts) = ts {
                    if let Some(r) = self.handle_usage(path, ts, p) {
                        out.push(r);
                    }
                }
            }
            Some("event_msg") => {
                if let Some(ts) = ts {
                    if let Some(r) = self.handle_event_msg(path, ts, p) {
                        out.push(r);
                    }
                }
            }
            _ => {}
        }
        Ok(out)
    }

    /// Nothing to flush: a call is emitted by its own `token_usage_record`, and a
    /// turn without its `task_complete` has no duration to report.
    fn flush(&mut self, _path: &Path) -> Vec<OutputRecord> {
        Vec::new()
    }

    /// No partially-accumulated calls exist, so idleness finalizes nothing.
    fn flush_idle(&mut self, _idle: std::time::Duration) -> Vec<OutputRecord> {
        Vec::new()
    }
}

// --- small JSON helpers -----------------------------------------------------

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(|s| s.to_string())
}

/// Fold a `session_meta` payload into the session state, keeping any value
/// already known when the payload omits it.
fn apply_session_meta(s: &mut SessionState, p: &Value) {
    if let Some(id) = str_field(p, "session_id").or_else(|| str_field(p, "id")) {
        s.session_id = id;
    }
    if let Some(cwd) = str_field(p, "cwd") {
        s.project = cwd;
    }
    s.git_branch = p
        .get("git")
        .and_then(|g| str_field(g, "branch"))
        .or_else(|| s.git_branch.take());
    s.originator = str_field(p, "originator").or_else(|| s.originator.take());
    s.cli_version = str_field(p, "cli_version").or_else(|| s.cli_version.take());
    s.fallback_model = p
        .get("base_instructions")
        .and_then(|b| b.get("provenance"))
        .and_then(|prov| str_field(prov, "model"))
        .or_else(|| s.fallback_model.take());
}

/// Read the `session_meta` payload from a rollout file's first line, if present.
fn read_session_meta(path: &Path) -> Option<Value> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(line.trim_end()).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    v.get("payload").cloned()
}

fn parse_ts(v: &Value) -> Option<DateTime<Utc>> {
    let s = v.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// One of Codex's three usage blocks (`usage`, `turn_token_usage`,
/// `thread_token_usage`), which share a shape.
struct CodexUsage {
    /// Total prompt tokens. OpenAI counts this **inclusive** of the cached and
    /// cache-write portions, unlike Anthropic's `input_tokens`.
    prompt: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
}

impl CodexUsage {
    /// Map onto the shared [`Tokens`] schema, whose `input` means the *fresh*
    /// (non-cached) prompt portion so that
    /// `input + cache_read_input + cache_creation_input == total_input` holds for
    /// every provider. `total_input` therefore keeps Codex's own prompt figure.
    fn to_tokens(&self) -> Tokens {
        let fresh = self.prompt.saturating_sub(self.cached + self.cache_write);
        Tokens {
            input: fresh,
            output: self.output,
            cache_read_input: self.cached,
            cache_creation_input: self.cache_write,
            // Codex reports no cache-TTL split.
            cache_creation_ephemeral_5m_input: 0,
            cache_creation_ephemeral_1h_input: 0,
            total_input: self.prompt,
            total: self.prompt + self.output,
            reasoning: Some(self.reasoning),
        }
    }
}

fn parse_usage(u: Option<&Value>) -> Option<CodexUsage> {
    let u = match u {
        Some(v) if v.is_object() => v,
        _ => return None,
    };
    let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(CodexUsage {
        prompt: get("input_tokens"),
        cached: get("cached_input_tokens"),
        cache_write: get("cache_write_input_tokens"),
        output: get("output_tokens"),
        reasoning: get("reasoning_output_tokens"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::Collector;

    const TURN: &str = "turn_1";
    const SESSION: &str = "sess_1";

    fn collector() -> CodexCollector {
        CodexCollector::new(Path::new("/home/test/.codex"), "testhost".into())
    }

    fn path() -> &'static Path {
        Path::new("/home/test/.codex/sessions/2026/09/09/rollout-2026-09-09T08-12-30-sess_1.jsonl")
    }

    fn session_meta() -> String {
        format!(
            r#"{{"timestamp":"2026-09-09T12:12:30.954Z","type":"session_meta","payload":{{"session_id":"{SESSION}","cwd":"/p","originator":"Codex Desktop","cli_version":"0.153.4","model_provider":"openai","base_instructions":{{"provenance":{{"type":"model","model":"gpt-5.6-meta"}}}},"git":{{"commit_hash":"abc","branch":"main","repository_url":"https://example.invalid/r.git"}}}}}}"#
        )
    }

    fn task_started(turn: &str, ts: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"task_started","turn_id":"{turn}","model_context_window":258400}}}}"#
        )
    }

    fn turn_context(turn: &str, ts: &str, model: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"turn_context","payload":{{"turn_id":"{turn}","root_turn_id":"{turn}","cwd":"/p","model":"{model}","effort":"medium","summary":"auto"}}}}"#
        )
    }

    fn task_complete(turn: &str, ts: &str, duration_ms: i64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"task_complete","turn_id":"{turn}","duration_ms":{duration_ms},"time_to_first_token_ms":2937}}}}"#
        )
    }

    fn tool_call(ts: &str, turn: &str, id: &str, name: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"response_item","payload":{{"type":"custom_tool_call","id":"ctc_{id}","status":"completed","call_id":"{id}","name":"{name}","internal_chat_message_metadata_passthrough":{{"turn_id":"{turn}"}}}}}}"#
        )
    }

    /// A `token_usage_record`; `turn_out` is the turn's running output total.
    fn usage(ts: &str, turn: &str, resp: &str, out: u64, turn_out: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"token_usage_record","payload":{{"thread_id":"thr_1","turn_id":"{turn}","session_id":"{SESSION}","root_turn_id":"{turn}","response_id":"{resp}","usage":{{"input_tokens":25091,"cached_input_tokens":17920,"cache_write_input_tokens":100,"output_tokens":{out},"reasoning_output_tokens":17,"total_tokens":0}},"turn_token_usage":{{"input_tokens":50736,"cached_input_tokens":42880,"cache_write_input_tokens":0,"output_tokens":{turn_out},"reasoning_output_tokens":17,"total_tokens":0}},"thread_token_usage":{{"input_tokens":0,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":0}}}}}}"#
        )
    }

    fn feed(c: &mut CodexCollector, lines: &[String]) -> Vec<OutputRecord> {
        let mut out = Vec::new();
        for l in lines {
            out.extend(c.consume_line(path(), l).unwrap());
        }
        out
    }

    fn only_call(recs: &[OutputRecord]) -> &CallRecord {
        let calls: Vec<_> = recs
            .iter()
            .filter_map(|r| match r {
                OutputRecord::Call(c) => Some(c.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "expected exactly one call record");
        calls[0]
    }

    #[test]
    fn one_record_per_response_id_with_token_mapping() {
        let mut c = collector();
        let recs = feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                turn_context(TURN, "2026-09-09T12:12:32.141Z", "gpt-5.6-sol"),
                tool_call("2026-09-09T12:12:37.904Z", TURN, "call_a", "exec"),
                usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 130, 130),
            ],
        );

        let call = only_call(&recs);
        let meta = call.codex.as_ref().expect("codex meta");
        assert_eq!(meta.response_id, "resp_A");
        assert_eq!(meta.session_id, SESSION);
        assert_eq!(meta.thread_id, "thr_1");
        assert_eq!(meta.turn_id, TURN);
        assert_eq!(meta.project, "/p");
        assert_eq!(meta.git_branch.as_deref(), Some("main"));
        assert_eq!(meta.originator.as_deref(), Some("Codex Desktop"));
        assert_eq!(meta.reasoning_effort.as_deref(), Some("medium"));

        assert_eq!(call.provider, "openai");
        assert_eq!(call.service.name, "codex");
        assert_eq!(call.service.version.as_deref(), Some("0.153.4"));
        assert_eq!(call.event.dataset, "codex.token_usage");
        assert_eq!(call.host.name, "testhost");
        assert_eq!(call.date, "2026-09-09");
        assert!(call.claude.is_none());

        // OpenAI's input_tokens is inclusive of the cached/cache-write portions,
        // so `input` carries only the fresh remainder.
        assert_eq!(call.tokens.cache_read_input, 17920);
        assert_eq!(call.tokens.cache_creation_input, 100);
        assert_eq!(call.tokens.input, 25091 - 17920 - 100);
        assert_eq!(call.tokens.total_input, 25091);
        assert_eq!(call.tokens.output, 130);
        assert_eq!(call.tokens.total, 25091 + 130);
        assert_eq!(call.tokens.reasoning, Some(17));

        // Tool calls precede their usage record and attach to it.
        assert_eq!(call.tools.use_count, 1);
        assert_eq!(call.tools.names, vec!["exec".to_string()]);
        assert!(call.server_tool_use.is_none());
    }

    #[test]
    fn per_call_perf_is_absent() {
        let mut c = collector();
        let recs = feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 130, 130),
            ],
        );
        // A rollout line has one timestamp, so no generation window exists.
        assert!(only_call(&recs).perf.is_none());
    }

    #[test]
    fn model_comes_from_turn_context_and_can_change_mid_session() {
        let mut c = collector();
        let recs = feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                turn_context(TURN, "2026-09-09T12:12:32.141Z", "gpt-5.6-sol"),
                usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 10, 10),
                task_complete(TURN, "2026-09-09T12:14:38.251Z", 127375),
                task_started("turn_2", "2026-09-09T12:35:41.860Z"),
                turn_context("turn_2", "2026-09-09T12:35:41.874Z", "gpt-5.6-mini"),
                usage("2026-09-09T12:35:50.000Z", "turn_2", "resp_B", 20, 20),
            ],
        );
        let models: Vec<&str> = recs
            .iter()
            .filter_map(|r| match r {
                OutputRecord::Call(c) => Some(c.model.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(models, vec!["gpt-5.6-sol", "gpt-5.6-mini"]);
    }

    #[test]
    fn model_falls_back_to_session_meta_without_turn_context() {
        let mut c = collector();
        let recs = feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 10, 10),
            ],
        );
        assert_eq!(only_call(&recs).model, "gpt-5.6-meta");
    }

    #[test]
    fn task_complete_emits_turn_with_codex_reported_totals() {
        let mut c = collector();
        let recs = feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                turn_context(TURN, "2026-09-09T12:12:32.141Z", "gpt-5.6-sol"),
                tool_call("2026-09-09T12:12:37.904Z", TURN, "call_a", "exec"),
                usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 130, 130),
                usage("2026-09-09T12:12:44.843Z", TURN, "resp_B", 183, 313),
                task_complete(TURN, "2026-09-09T12:14:38.251Z", 6000),
            ],
        );

        let turn = recs
            .iter()
            .find_map(|r| match r {
                OutputRecord::Turn(t) => Some(t),
                _ => None,
            })
            .expect("turn record");

        assert_eq!(turn.turn.duration_ms, 6000);
        // The turn's own running total, not a sum of our per-call view.
        assert_eq!(turn.turn.output_tokens, 313);
        assert_eq!(turn.turn.tokens_per_sec, Some(313.0 / 6.0));
        assert_eq!(turn.turn.message_count, 1); // one response_item in the turn
        assert_eq!(turn.event.dataset, "codex.turn");
        assert_eq!(turn.provider, "openai");
        assert!(turn.claude.is_none());

        let meta = turn.codex.as_ref().expect("codex turn meta");
        assert_eq!(meta.turn_id, TURN);
        assert_eq!(meta.session_id, SESSION);
        assert_eq!(meta.project, "/p");
        assert_eq!(meta.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(meta.time_to_first_token_ms, Some(2937));
    }

    #[test]
    fn tools_attach_only_to_the_call_that_follows_them() {
        let mut c = collector();
        let recs = feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                tool_call("2026-09-09T12:12:37.900Z", TURN, "call_a", "exec"),
                tool_call("2026-09-09T12:12:37.901Z", TURN, "call_b", "apply_patch"),
                usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 130, 130),
                // No tool call before the next response.
                usage("2026-09-09T12:12:44.843Z", TURN, "resp_B", 183, 313),
            ],
        );
        let calls: Vec<_> = recs
            .iter()
            .filter_map(|r| match r {
                OutputRecord::Call(c) => Some(c.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tools.use_count, 2);
        assert_eq!(
            calls[0].tools.names,
            vec!["apply_patch".to_string(), "exec".to_string()]
        );
        assert_eq!(calls[1].tools.use_count, 0);
        assert!(calls[1].tools.names.is_empty());
    }

    #[test]
    fn web_search_calls_populate_server_tool_use() {
        let mut c = collector();
        let search = format!(
            r#"{{"timestamp":"2026-09-09T12:12:37.900Z","type":"response_item","payload":{{"type":"web_search_call","id":"ws_1","status":"completed","internal_chat_message_metadata_passthrough":{{"turn_id":"{TURN}"}}}}}}"#
        );
        let recs = feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                search,
                usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 130, 130),
            ],
        );
        let call = only_call(&recs);
        let stu = call.server_tool_use.as_ref().expect("server tool use");
        assert_eq!(stu.web_search_requests, 1);
        // Unnamed call items fall back to their item type.
        assert_eq!(call.tools.names, vec!["web_search_call".to_string()]);
    }

    #[test]
    fn nested_turn_does_not_reset_the_parent_turns_total() {
        let mut c = collector();
        let recs = feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 130, 313),
                // A sub-agent turn runs and completes inside the parent turn.
                task_started("sub_1", "2026-09-09T12:12:40.000Z"),
                usage("2026-09-09T12:12:41.000Z", "sub_1", "resp_S", 40, 40),
                task_complete("sub_1", "2026-09-09T12:12:42.000Z", 2000),
                task_complete(TURN, "2026-09-09T12:14:38.251Z", 6000),
            ],
        );
        let turns: Vec<_> = recs
            .iter()
            .filter_map(|r| match r {
                OutputRecord::Turn(t) => Some(t.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].codex.as_ref().unwrap().turn_id, "sub_1");
        assert_eq!(turns[0].turn.output_tokens, 40);
        assert_eq!(turns[1].codex.as_ref().unwrap().turn_id, TURN);
        assert_eq!(turns[1].turn.output_tokens, 313);
    }

    #[test]
    fn resumed_mid_session_recovers_context_from_the_file() {
        // Simulate a restart: the file exists in full, but the tailer resumes at
        // a checkpoint past `session_meta` and `task_started`, so only later
        // lines are fed in.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rollout-2026-09-09T08-12-30-sess_1.jsonl");
        let tail = [
            turn_context(TURN, "2026-09-09T12:12:32.141Z", "gpt-5.6-sol"),
            usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 130, 313),
            task_complete(TURN, "2026-09-09T12:14:38.251Z", 6000),
        ];
        let mut body = session_meta();
        body.push('\n');
        body.push_str(&task_started(TURN, "2026-09-09T12:12:30.954Z"));
        body.push('\n');
        body.push_str(&tail.join("\n"));
        std::fs::write(&file, body).unwrap();

        let mut c = CodexCollector::new(dir.path(), "testhost".into());
        let mut recs = Vec::new();
        for l in &tail {
            recs.extend(c.consume_line(&file, l).unwrap());
        }

        let call = only_call(&recs);
        let meta = call.codex.as_ref().unwrap();
        assert_eq!(meta.session_id, SESSION);
        assert_eq!(meta.project, "/p");
        assert_eq!(meta.git_branch.as_deref(), Some("main"));
        assert_eq!(call.service.version.as_deref(), Some("0.153.4"));

        // The turn was never opened by a task_started, but its cumulative total
        // is still correct and the turn record is emitted.
        let turn = recs
            .iter()
            .find_map(|r| match r {
                OutputRecord::Turn(t) => Some(t),
                _ => None,
            })
            .expect("turn record");
        assert_eq!(turn.turn.output_tokens, 313);
    }

    #[test]
    fn turn_without_any_usage_record_is_dropped() {
        let mut c = collector();
        // A legacy rollout completes turns but logs no `token_usage_record`;
        // emitting a turn here would report a fabricated zero output.
        let recs = feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                turn_context(TURN, "2026-09-09T12:12:32.141Z", "gpt-5.6-sol"),
                task_complete(TURN, "2026-09-09T12:14:38.251Z", 127375),
            ],
        );
        assert!(recs.is_empty());
    }

    #[test]
    fn ignores_legacy_token_count_events_and_other_lines() {
        let mut c = collector();
        // A pre-0.15 rollout logs usage here; consuming it alongside
        // token_usage_record would double-count, so it is skipped.
        let legacy = r#"{"timestamp":"2026-05-21T19:46:20.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":150}}}}"#;
        let world = r#"{"timestamp":"2026-09-09T12:12:31.000Z","type":"world_state","payload":{"full":true,"state":{}}}"#;
        let recs = feed(
            &mut c,
            &[session_meta(), legacy.to_string(), world.to_string()],
        );
        assert!(recs.is_empty());
    }

    #[test]
    fn flush_emits_nothing_and_malformed_lines_error() {
        let mut c = collector();
        feed(
            &mut c,
            &[
                session_meta(),
                task_started(TURN, "2026-09-09T12:12:30.954Z"),
                usage("2026-09-09T12:12:37.968Z", TURN, "resp_A", 130, 130),
            ],
        );
        // The usage line already emitted its call; an open turn has no duration.
        assert!(c.flush(path()).is_empty());
        assert!(c.flush_idle(std::time::Duration::from_secs(1)).is_empty());
        assert!(c.consume_line(path(), "not json").is_err());
    }

    /// Replay a real rollout file end-to-end and check the invariants that
    /// inline fixtures can't: that every `token_usage_record` yields exactly one
    /// call, ids are unique, and the token arithmetic holds on live data.
    ///
    /// Ignored by default (needs a machine with Codex history). Run with:
    /// `TOKEN_USE_TEST_ROLLOUT=<file.jsonl> cargo test replays_a_real_rollout \
    ///  -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn replays_a_real_rollout() {
        let Ok(file) = std::env::var("TOKEN_USE_TEST_ROLLOUT") else {
            panic!("set TOKEN_USE_TEST_ROLLOUT to a rollout .jsonl path");
        };
        let body = std::fs::read_to_string(&file).expect("reading rollout");
        let p = Path::new(&file);
        let mut c = CodexCollector::new(Path::new("/unused"), "testhost".into());

        let (mut usage_lines, mut complete_lines) = (0usize, 0usize);
        let mut recs = Vec::new();
        for line in body.lines() {
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v.get("type").and_then(Value::as_str) {
                Some("token_usage_record") => usage_lines += 1,
                Some("event_msg")
                    if v.get("payload")
                        .and_then(|p| p.get("type"))
                        .and_then(Value::as_str)
                        == Some("task_complete") =>
                {
                    complete_lines += 1
                }
                _ => {}
            }
            recs.extend(c.consume_line(p, line).unwrap());
        }

        let calls: Vec<_> = recs
            .iter()
            .filter_map(|r| match r {
                OutputRecord::Call(c) => Some(c.as_ref()),
                _ => None,
            })
            .collect();
        let turns: Vec<_> = recs
            .iter()
            .filter_map(|r| match r {
                OutputRecord::Turn(t) => Some(t.as_ref()),
                _ => None,
            })
            .collect();

        println!(
            "{}: {} calls / {} usage lines, {} turns / {} task_complete lines",
            p.display(),
            calls.len(),
            usage_lines,
            turns.len(),
            complete_lines
        );
        assert_eq!(calls.len(), usage_lines, "one call per token_usage_record");
        // At most one turn per `task_complete` — fewer on a legacy rollout, whose
        // turns carry no token data and are dropped.
        assert!(turns.len() <= complete_lines, "no turn without a task_complete");
        if usage_lines == 0 {
            assert!(turns.is_empty(), "legacy rollout emits nothing");
        }

        let mut ids = BTreeSet::new();
        for call in &calls {
            let m = call.codex.as_ref().expect("codex meta");
            assert!(ids.insert(m.response_id.clone()), "duplicate response_id");
            assert!(call.claude.is_none());
            assert!(call.perf.is_none());
            assert_eq!(call.provider, "openai");
            assert_ne!(call.model, "unknown", "model resolved for every call");
            assert!(!m.session_id.is_empty() && !m.turn_id.is_empty());
            assert_eq!(call.tokens.total, call.tokens.total_input + call.tokens.output);
            assert_eq!(
                call.tokens.total_input,
                call.tokens.input + call.tokens.cache_read_input + call.tokens.cache_creation_input,
                "prompt-side breakdown sums to total_input"
            );
        }
        for turn in &turns {
            println!(
                "  turn {} {}ms out={} items={} ttft={:?}",
                turn.codex.as_ref().unwrap().turn_id,
                turn.turn.duration_ms,
                turn.turn.output_tokens,
                turn.turn.message_count,
                turn.codex.as_ref().unwrap().time_to_first_token_ms,
            );
            assert!(turn.turn.duration_ms > 0);
        }
    }

    #[test]
    fn owns_only_jsonl_under_the_sessions_root() {
        let c = collector();
        assert!(c.owns(path()));
        assert!(!c.owns(Path::new("/home/test/.codex/config.toml")));
        assert!(!c.owns(Path::new("/home/test/.claude/projects/enc/s1.jsonl")));
    }
}
