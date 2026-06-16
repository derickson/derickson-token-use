//! Anthropic Claude Code collector.
//!
//! Reads `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` transcripts.
//! Token usage lives on `type == "assistant"` lines; a single API response is
//! written as several content-block lines that all repeat the *same* `usage`
//! and `message.id`, each carrying its own block timestamp. We therefore
//! accumulate a call across its lines and finalize it (computing
//! `generation_ms` from first/last block timestamp) when the next `message.id`
//! or a non-assistant line arrives.
//!
//! `type == "system"`, `subtype == "turn_duration"` lines yield a turn record
//! summarizing the output tokens generated since the previous turn boundary.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;
use walkdir::WalkDir;

use crate::error::ParseError;
use crate::record::*;

const PROVIDER: &str = "anthropic";
const SERVICE: &str = "claude-code";

/// In-progress accumulation for one `message.id`.
struct OpenCall {
    message_id: String,
    request_id: Option<String>,
    model: String,
    session_id: String,
    project: String,
    git_branch: Option<String>,
    service_tier: Option<String>,
    is_sidechain: bool,
    stop_reason: Option<String>,
    entrypoint: Option<String>,
    version: Option<String>,
    first_ts: DateTime<Utc>,
    last_ts: DateTime<Utc>,
    usage: Usage,
    /// tool_use block ids already counted (dedup, in case a variant repeats the
    /// full content array on each line rather than one block per line).
    tool_block_ids: BTreeSet<String>,
    tool_names: BTreeSet<String>,
    tool_count: u32,
}

#[derive(Default, Clone)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    cache_creation_5m: u64,
    cache_creation_1h: u64,
    web_search: u64,
    web_fetch: u64,
    has_server_tool_use: bool,
}

pub struct ClaudeCodeCollector {
    watch_root: PathBuf,
    host: String,
    /// Open call per transcript file (one session = one file).
    open: HashMap<PathBuf, OpenCall>,
    /// Output tokens accumulated since the last turn boundary, per session file.
    turn_tokens: HashMap<PathBuf, u64>,
}

impl ClaudeCodeCollector {
    pub fn new(home: &Path, host: String) -> Self {
        ClaudeCodeCollector {
            watch_root: home.join(".claude").join("projects"),
            host,
            open: HashMap::new(),
            turn_tokens: HashMap::new(),
        }
    }

    fn now_iso() -> String {
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// Finalize the open call for `path`, if any, into a `CallRecord`.
    fn finalize(&mut self, path: &Path) -> Option<OutputRecord> {
        let call = self.open.remove(path)?;
        let generation_ms = (call.last_ts - call.first_ts).num_milliseconds();
        let u = &call.usage;
        let total_input = u.input + u.cache_read + u.cache_creation;

        *self.turn_tokens.entry(path.to_path_buf()).or_insert(0) += u.output;

        let rec = CallRecord {
            date: call.first_ts.format("%Y-%m-%d").to_string(),
            timestamp: call
                .first_ts
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ingested_at: Self::now_iso(),
            event: Event {
                dataset: "claude_code.token_usage",
                module: "token-use",
            },
            service: Service {
                name: SERVICE,
                version: call.version.clone(),
            },
            provider: PROVIDER,
            model: call.model.clone(),
            host: Host {
                name: self.host.clone(),
            },
            claude: ClaudeMeta {
                message_id: call.message_id,
                request_id: call.request_id,
                model: call.model,
                session_id: call.session_id,
                project: call.project,
                git_branch: call.git_branch,
                service_tier: call.service_tier,
                is_sidechain: call.is_sidechain,
                stop_reason: call.stop_reason,
                entrypoint: call.entrypoint,
            },
            tokens: Tokens {
                input: u.input,
                output: u.output,
                cache_read_input: u.cache_read,
                cache_creation_input: u.cache_creation,
                cache_creation_ephemeral_5m_input: u.cache_creation_5m,
                cache_creation_ephemeral_1h_input: u.cache_creation_1h,
                total_input,
                total: total_input + u.output,
            },
            perf: Perf {
                generation_ms,
                tokens_per_sec: tokens_per_sec(u.output, generation_ms),
            },
            tools: Tools {
                use_count: call.tool_count,
                names: call.tool_names.into_iter().collect(),
            },
            server_tool_use: if u.has_server_tool_use {
                Some(ServerToolUse {
                    web_search_requests: u.web_search,
                    web_fetch_requests: u.web_fetch,
                })
            } else {
                None
            },
        };
        Some(OutputRecord::Call(Box::new(rec)))
    }

    /// Build a turn record from a `turn_duration` line and reset the counter.
    fn make_turn(&mut self, path: &Path, v: &Value) -> Option<OutputRecord> {
        let ts = parse_ts(v.get("timestamp")?)?;
        let duration_ms = v.get("durationMs").and_then(Value::as_i64).unwrap_or(0);
        let message_count = v.get("messageCount").and_then(Value::as_u64).unwrap_or(0);
        let output_tokens = self.turn_tokens.remove(path).unwrap_or(0);

        let rec = TurnRecord {
            date: ts.format("%Y-%m-%d").to_string(),
            timestamp: ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ingested_at: Self::now_iso(),
            event: Event {
                dataset: "claude_code.turn",
                module: "token-use",
            },
            service: Service {
                name: SERVICE,
                version: str_field(v, "version"),
            },
            provider: PROVIDER,
            host: Host {
                name: self.host.clone(),
            },
            claude: TurnMeta {
                session_id: str_field(v, "sessionId").unwrap_or_default(),
                project: str_field(v, "cwd").unwrap_or_default(),
                git_branch: str_field(v, "gitBranch"),
                entrypoint: str_field(v, "entrypoint"),
            },
            turn: Turn {
                duration_ms,
                message_count,
                output_tokens,
                tokens_per_sec: tokens_per_sec(output_tokens, duration_ms),
            },
        };
        Some(OutputRecord::Turn(Box::new(rec)))
    }

    /// Fold one assistant content-block line into the open call (starting a new
    /// one and emitting the previous if the `message.id` changed).
    fn handle_assistant(
        &mut self,
        path: &Path,
        v: &Value,
        out: &mut Vec<OutputRecord>,
    ) -> Result<(), ParseError> {
        let msg = v
            .get("message")
            .ok_or_else(|| ParseError::Shape("assistant line without message".into()))?;
        let model = str_field(msg, "model").unwrap_or_default();

        // Synthetic lines carry null usage and are not real API calls.
        if model == "<synthetic>" {
            if let Some(r) = self.finalize(path) {
                out.push(r);
            }
            return Ok(());
        }

        let message_id = match str_field(msg, "id") {
            Some(id) => id,
            None => return Ok(()), // not a token-bearing assistant line
        };
        let ts = match v.get("timestamp").and_then(parse_ts) {
            Some(t) => t,
            None => return Ok(()),
        };

        // Boundary: a different message.id closes the previous call.
        if let Some(existing) = self.open.get(path) {
            if existing.message_id != message_id {
                if let Some(r) = self.finalize(path) {
                    out.push(r);
                }
            }
        }

        let entry = self.open.entry(path.to_path_buf());
        let call = match entry {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let usage = parse_usage(msg.get("usage"));
                slot.insert(OpenCall {
                    message_id: message_id.clone(),
                    request_id: str_field(v, "requestId"),
                    model: model.clone(),
                    session_id: str_field(v, "sessionId").unwrap_or_default(),
                    project: str_field(v, "cwd").unwrap_or_default(),
                    git_branch: str_field(v, "gitBranch"),
                    service_tier: str_field(msg.get("usage").unwrap_or(&Value::Null), "service_tier"),
                    is_sidechain: v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false),
                    stop_reason: str_field(msg, "stop_reason"),
                    entrypoint: str_field(v, "entrypoint"),
                    version: str_field(v, "version"),
                    first_ts: ts,
                    last_ts: ts,
                    usage,
                    tool_block_ids: BTreeSet::new(),
                    tool_names: BTreeSet::new(),
                    tool_count: 0,
                })
            }
        };

        // Widen the generation window.
        if ts < call.first_ts {
            call.first_ts = ts;
        }
        if ts > call.last_ts {
            call.last_ts = ts;
        }
        if call.stop_reason.is_none() {
            call.stop_reason = str_field(msg, "stop_reason");
        }

        // Accumulate tool usage from this line's content blocks (dedup by id).
        if let Some(Value::Array(blocks)) = msg.get("content") {
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let id = str_field(b, "id").unwrap_or_default();
                    if call.tool_block_ids.insert(id) {
                        call.tool_count += 1;
                        if let Some(name) = str_field(b, "name") {
                            call.tool_names.insert(name);
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

impl crate::collector::Collector for ClaudeCodeCollector {
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

    fn consume_line(
        &mut self,
        path: &Path,
        line: &str,
    ) -> Result<Vec<OutputRecord>, ParseError> {
        let v: Value = serde_json::from_str(line)?;
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => self.handle_assistant(path, &v, &mut out)?,
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("turn_duration") => {
                // A turn boundary closes any open call first, then emits the turn.
                if let Some(r) = self.finalize(path) {
                    out.push(r);
                }
                if let Some(r) = self.make_turn(path, &v) {
                    out.push(r);
                }
            }
            // Any other line type (user, tool result, other system) closes the
            // open call as a boundary but emits nothing else.
            _ => {
                if let Some(r) = self.finalize(path) {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    fn flush(&mut self, path: &Path) -> Vec<OutputRecord> {
        self.finalize(path).into_iter().collect()
    }

    fn flush_idle(&mut self, idle: std::time::Duration) -> Vec<OutputRecord> {
        let idle = match chrono::Duration::from_std(idle) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let cutoff = Utc::now() - idle;
        let stale: Vec<PathBuf> = self
            .open
            .iter()
            .filter(|(_, call)| call.last_ts < cutoff)
            .map(|(p, _)| p.clone())
            .collect();
        stale
            .iter()
            .filter_map(|p| self.finalize(p))
            .collect()
    }
}

// --- small JSON helpers -----------------------------------------------------

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

fn parse_ts(v: &Value) -> Option<DateTime<Utc>> {
    let s = v.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_usage(u: Option<&Value>) -> Usage {
    let u = match u {
        Some(v) if v.is_object() => v,
        _ => return Usage::default(),
    };
    let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cc = u.get("cache_creation");
    let cc_get = |k: &str| {
        cc.and_then(|c| c.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let stu = u.get("server_tool_use");
    let has_stu = stu.map(|s| s.is_object()).unwrap_or(false);
    let stu_get = |k: &str| {
        stu.and_then(|s| s.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };

    Usage {
        input: get("input_tokens"),
        output: get("output_tokens"),
        cache_read: get("cache_read_input_tokens"),
        cache_creation: get("cache_creation_input_tokens"),
        cache_creation_5m: cc_get("ephemeral_5m_input_tokens"),
        cache_creation_1h: cc_get("ephemeral_1h_input_tokens"),
        web_search: stu_get("web_search_requests"),
        web_fetch: stu_get("web_fetch_requests"),
        has_server_tool_use: has_stu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::Collector;

    fn collector() -> ClaudeCodeCollector {
        ClaudeCodeCollector::new(Path::new("/home/test"), "testhost".into())
    }

    fn assistant_line(id: &str, ts: &str, block: &str, out_tokens: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"s1","cwd":"/p","gitBranch":"main","version":"2.1.0","requestId":"req_1","isSidechain":false,"entrypoint":"cli","message":{{"id":"{id}","model":"claude-opus-4-8","stop_reason":"tool_use","content":[{block}],"usage":{{"input_tokens":10,"output_tokens":{out_tokens},"cache_read_input_tokens":100,"cache_creation_input_tokens":20,"service_tier":"standard","cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":20}},"server_tool_use":{{"web_search_requests":0,"web_fetch_requests":0}}}}}}}}"#
        )
    }

    #[test]
    fn one_record_per_message_id_with_metrics() {
        let mut c = collector();
        let p = Path::new("/home/test/.claude/projects/enc/s1.jsonl");

        // Three content-block lines of ONE message, distinct timestamps.
        let mut recs = Vec::new();
        recs.extend(c.consume_line(p, &assistant_line("msg_A", "2026-06-16T10:00:00.000Z", r#"{"type":"thinking"}"#, 300)).unwrap());
        recs.extend(c.consume_line(p, &assistant_line("msg_A", "2026-06-16T10:00:02.000Z", r#"{"type":"text"}"#, 300)).unwrap());
        recs.extend(c.consume_line(p, &assistant_line("msg_A", "2026-06-16T10:00:05.000Z", r#"{"type":"tool_use","id":"t1","name":"Bash"}"#, 300)).unwrap());
        // Nothing finalized yet (still open).
        assert!(recs.is_empty());

        // A user line closes the call.
        recs.extend(c.consume_line(p, r#"{"type":"user","timestamp":"2026-06-16T10:00:06.000Z"}"#).unwrap());
        assert_eq!(recs.len(), 1);
        let OutputRecord::Call(call) = &recs[0] else { panic!("expected call") };

        assert_eq!(call.claude.message_id, "msg_A");
        assert_eq!(call.tokens.output, 300);
        assert_eq!(call.tokens.cache_read_input, 100);
        assert_eq!(call.tokens.total_input, 10 + 100 + 20);
        assert_eq!(call.perf.generation_ms, 5000); // 10:00:05 - 10:00:00
        assert_eq!(call.perf.tokens_per_sec, Some(60.0)); // 300 / 5s
        assert_eq!(call.tools.use_count, 1);
        assert_eq!(call.tools.names, vec!["Bash".to_string()]);
        assert_eq!(call.provider, "anthropic");
        assert_eq!(call.model, "claude-opus-4-8");
        assert_eq!(call.date, "2026-06-16");
    }

    #[test]
    fn single_block_message_has_null_tokens_per_sec() {
        let mut c = collector();
        let p = Path::new("/home/test/.claude/projects/enc/s1.jsonl");
        c.consume_line(p, &assistant_line("msg_B", "2026-06-16T10:00:00.000Z", r#"{"type":"text"}"#, 50)).unwrap();
        let recs = c.flush(p);
        assert_eq!(recs.len(), 1);
        let OutputRecord::Call(call) = &recs[0] else { panic!() };
        assert_eq!(call.perf.generation_ms, 0);
        assert_eq!(call.perf.tokens_per_sec, None);
    }

    #[test]
    fn synthetic_and_new_id_finalize_previous() {
        let mut c = collector();
        let p = Path::new("/home/test/.claude/projects/enc/s1.jsonl");
        c.consume_line(p, &assistant_line("msg_A", "2026-06-16T10:00:00.000Z", r#"{"type":"text"}"#, 10)).unwrap();
        // New message id finalizes msg_A.
        let recs = c.consume_line(p, &assistant_line("msg_C", "2026-06-16T10:00:01.000Z", r#"{"type":"text"}"#, 10)).unwrap();
        assert_eq!(recs.len(), 1);
        assert!(matches!(&recs[0], OutputRecord::Call(c) if c.claude.message_id == "msg_A"));
    }

    #[test]
    fn turn_duration_sums_output_and_resets() {
        let mut c = collector();
        let p = Path::new("/home/test/.claude/projects/enc/s1.jsonl");
        c.consume_line(p, &assistant_line("msg_A", "2026-06-16T10:00:00.000Z", r#"{"type":"text"}"#, 100)).unwrap();
        c.consume_line(p, r#"{"type":"user","timestamp":"2026-06-16T10:00:01.000Z"}"#).unwrap();
        c.consume_line(p, &assistant_line("msg_B", "2026-06-16T10:00:02.000Z", r#"{"type":"text"}"#, 200)).unwrap();

        let turn_line = r#"{"type":"system","subtype":"turn_duration","durationMs":6000,"messageCount":4,"timestamp":"2026-06-16T10:00:06.000Z","sessionId":"s1","cwd":"/p","gitBranch":"main","version":"2.1.0","entrypoint":"cli"}"#;
        let recs = c.consume_line(p, turn_line).unwrap();
        // Finalizes msg_B (still open) then emits the turn.
        let turn = recs.iter().find_map(|r| match r {
            OutputRecord::Turn(t) => Some(t),
            _ => None,
        }).expect("turn record");
        assert_eq!(turn.turn.output_tokens, 300); // 100 + 200
        assert_eq!(turn.turn.duration_ms, 6000);
        assert_eq!(turn.turn.tokens_per_sec, Some(50.0)); // 300 / 6s

        // Counter reset: a second turn with no calls reports zero.
        let recs2 = c.consume_line(p, turn_line).unwrap();
        let turn2 = recs2.iter().find_map(|r| match r {
            OutputRecord::Turn(t) => Some(t),
            _ => None,
        }).unwrap();
        assert_eq!(turn2.turn.output_tokens, 0);
    }

    #[test]
    fn skips_synthetic_usage() {
        let mut c = collector();
        let p = Path::new("/home/test/.claude/projects/enc/s1.jsonl");
        let synthetic = r#"{"type":"assistant","timestamp":"2026-06-16T10:00:00Z","sessionId":"s1","cwd":"/p","isSidechain":false,"message":{"id":"msg_S","model":"<synthetic>","content":[{"type":"text"}],"usage":null}}"#;
        let recs = c.consume_line(p, synthetic).unwrap();
        assert!(recs.is_empty());
        assert!(c.flush(p).is_empty());
    }
}
