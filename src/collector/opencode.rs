//! opencode collector (SQLite poll source).
//!
//! opencode keeps everything in SQLite — `~/.local/share/opencode/opencode-<channel>.db`
//! — so like Hermes it is a [`PollCollector`] rather than a byte-tailing
//! [`Collector`](crate::collector::Collector). Unlike Hermes it is *call*-grained:
//! each row of the `message` table holds a JSON blob with the complete usage of
//! one API response, so this emits a [`CallRecord`] per call, directly comparable
//! with Claude Code and Codex.
//!
//! # One record per assistant message
//!
//! opencode writes **one assistant message per API call**: a tool loop appends
//! further assistant messages rather than extra steps inside one. The `part`
//! table *also* carries `step-finish` rows with a full `tokens` block, so reading
//! both tables would double-count — the same trap the Codex collector documents
//! for legacy `event_msg` lines. Only `message` is read.
//!
//! This is checkable, and the check is worth stating because it is what licenses
//! ignoring `step-finish`: opencode maintains running totals on the `session`
//! row, and those columns equal the sum of the per-message tokens exactly. The
//! `reconciles_with_session_aggregates` test asserts that invariant against a
//! real database.
//!
//! # Token mapping
//!
//! opencode reports `input` exclusive of cache reads, and `reasoning` exclusive
//! of `output` (its own `tokens.total` is `input + output + reasoning +
//! cache.read`, which is how both were confirmed). Our schema's response side is
//! inclusive of thinking — Claude Code counts thinking inside `output`, and
//! OpenAI's `completion_tokens` includes `reasoning_tokens` — so `output` is
//! emitted as opencode's `output + reasoning`, with `reasoning` kept alongside as
//! the breakdown. Without that, opencode's reasoning-heavy local models would
//! read as near-zero output next to every other service.
//!
//! # Settling, and why full history is always recoverable
//!
//! A message is final when its `time.completed` is set; until then it is in
//! flight and is left alone. Emission is gated by the durable once-only ledger
//! ([`State::mark`]), exactly as in Hermes, so the NDJSON stays locally
//! deduplicated for a Fleet data stream that assigns its own `_id`.
//!
//! The cursor is a *watermark* over `message.time_created`, held in the same
//! per-path map the file tailer uses. Two properties matter for backfill:
//!
//!   * It starts at **0**, so the first poll against a fresh checkpoint walks the
//!     entire history — however old — and emits every settled call ever written.
//!     Nothing about steady-state operation short-circuits that; a full
//!     re-ingest is always just a matter of starting from an empty ledger.
//!   * It only advances to the oldest message still *unsettled*, never past it.
//!     An in-flight call cannot be stranded behind the cursor when it completes.
//!
//! Because the watermark can only move forward over decided messages, marks
//! below it can never be rescanned, so the ledger is pruned to the rescan window
//! ([`State::retain_marks`]) — otherwise a per-call ledger would grow without
//! bound across a long history.
//!
//! A call that is killed mid-stream (opencode crashes, the process is signalled)
//! keeps `time.created` and never gains `time.completed`, so it stays "in flight"
//! forever. Left alone that would pin the watermark at its timestamp for good and
//! grow the rescan window without bound, so an unsettled call older than
//! [`ABANDONED_AFTER`] is treated as decided and the watermark moves past it.
//! Real databases do contain these.
//!
//! Aborted calls (`MessageAbortedError`, zero tokens) are settled but carry
//! nothing to count, so they are skipped rather than emitted as zeroes that would
//! drag down per-call averages. No turn records are emitted: opencode has no
//! turn-duration event of its own, and inferring one from message spacing would
//! be a different measurement than the one Claude Code and Codex report.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::{debug, warn};

use crate::collector::PollCollector;
use crate::record::*;
use crate::state::State;

/// An unsettled call older than this is considered abandoned rather than in
/// flight, so the watermark is not pinned behind it forever. Far longer than any
/// real API response, so a slow call is never cut off early.
const ABANDONED_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Fallback when a message carries no `providerID` of its own.
const PROVIDER: &str = "opencode";
const SERVICE: &str = "opencode";

pub struct OpencodeCollector {
    /// opencode's data dir, normally `~/.local/share/opencode`.
    opencode_dir: PathBuf,
    host: String,
}

impl OpencodeCollector {
    pub fn new(opencode_dir: &Path, host: String) -> Self {
        OpencodeCollector {
            opencode_dir: opencode_dir.to_path_buf(),
            host,
        }
    }

    fn now_iso() -> String {
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// Every `opencode-<channel>.db` in the data dir, as `(path, channel)`.
    ///
    /// Globbed rather than hardcoded to `opencode-stable.db` so a dev/nightly
    /// channel installed alongside stable is picked up without a code change.
    /// The `-wal`/`-shm` sidecars are not databases and are excluded.
    fn enumerate(&self) -> Vec<(PathBuf, String)> {
        let mut dbs = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.opencode_dir) else {
            return dbs;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Some(channel) = name
                .strip_prefix("opencode-")
                .and_then(|rest| rest.strip_suffix(".db"))
            {
                dbs.push((path.clone(), channel.to_string()));
            }
        }
        dbs.sort();
        dbs
    }

    /// Scan one database from its watermark and emit every settled, not-yet-emitted
    /// call. `ns` (the DB path) namespaces both the ledger and the cursor.
    fn scan_db(
        &self,
        db_path: &Path,
        channel: &str,
        now: DateTime<Utc>,
        state: &mut State,
    ) -> rusqlite::Result<Vec<OutputRecord>> {
        // Read-only, and deliberately *not* `immutable=1`: opencode holds a large
        // WAL, and an immutable open would silently read a stale snapshot that
        // excludes it.
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;

        // 0 on a fresh checkpoint — the whole history is in scope.
        let watermark = state.offset_for(db_path) as i64;

        let sessions = load_sessions(&conn)?;

        let rows: Vec<(String, String, i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, time_created, data FROM message \
                 WHERE time_created >= ?1 ORDER BY time_created ASC",
            )?;
            let rows = stmt.query_map([watermark], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            rows.filter_map(Result::ok).collect()
        };

        // Tool names for a message, fetched only for calls we actually emit.
        let mut tool_stmt = conn.prepare("SELECT data FROM part WHERE message_id = ?1")?;

        let mut out = Vec::new();
        let mut keep: HashSet<String> = HashSet::new();
        let mut oldest_unsettled: Option<i64> = None;
        let mut newest_seen = watermark;
        let abandoned_before = now.timestamp_millis() - ABANDONED_AFTER.as_millis() as i64;

        for (msg_id, session_id, time_created, data) in rows {
            newest_seen = newest_seen.max(time_created);
            keep.insert(msg_id.clone());

            let Ok(v) = serde_json::from_str::<Value>(&data) else {
                warn!(message = %msg_id, "opencode: unparseable message data; skipping");
                continue;
            };
            if v.get("role").and_then(Value::as_str) != Some("assistant") {
                continue; // user/system messages carry no usage
            }

            if !is_settled(&v) {
                if time_created < abandoned_before {
                    // Never completed and far too old to still be running: treat
                    // as decided so the watermark is not pinned here forever.
                    debug!(message = %msg_id, "opencode: abandoning never-completed call");
                    continue;
                }
                // Hold the watermark at (not past) the oldest in-flight call, so
                // it is still in scope on the poll after it completes.
                oldest_unsettled =
                    Some(oldest_unsettled.map_or(time_created, |t: i64| t.min(time_created)));
                continue;
            }

            if state.is_marked(&conn_ns(db_path), &msg_id) {
                continue; // already emitted — never write twice
            }

            let u = TokenFields::from_message(&v);
            if u.is_empty() {
                // Settled but nothing to count (aborted, or an error before any
                // token was billed). Decided, so the watermark may pass it.
                continue;
            }

            let session = sessions.get(&session_id);
            let tools = load_tools(&mut tool_stmt, &msg_id);
            match self.build_record(&v, &msg_id, &session_id, session, channel, &u, tools) {
                Some(rec) => {
                    out.push(rec);
                    state.mark(&conn_ns(db_path), &msg_id);
                }
                None => warn!(message = %msg_id, "opencode: incomplete message; skipping"),
            }
        }

        // Advance to the oldest still-open call, else past everything seen.
        let new_watermark = oldest_unsettled.unwrap_or(newest_seen).max(0);
        if new_watermark != watermark {
            state.set_offset(db_path, new_watermark as u64, Some(Self::now_iso()));
        }
        // Anything below the watermark can never be rescanned, so its mark is
        // dead weight; `keep` holds every id still within the rescan window.
        state.retain_marks(&conn_ns(db_path), &keep);

        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_record(
        &self,
        v: &Value,
        msg_id: &str,
        session_id: &str,
        session: Option<&SessionRow>,
        channel: &str,
        u: &TokenFields,
        tools: (u32, Vec<String>),
    ) -> Option<OutputRecord> {
        let created = v.pointer("/time/created").and_then(Value::as_i64)?;
        let completed = v.pointer("/time/completed").and_then(Value::as_i64);
        let dt = DateTime::from_timestamp_millis(created)?;

        // Prefer the message's own cwd; fall back to the session directory.
        let project = v
            .pointer("/path/cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| session.and_then(|s| s.directory.clone()))
            .unwrap_or_default();

        let model = v
            .get("modelID")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| session.and_then(|s| s.model.clone()))
            .unwrap_or_else(|| "unknown".to_string());

        let provider = v
            .get("providerID")
            .and_then(Value::as_str)
            .unwrap_or(PROVIDER)
            .to_string();

        // A real within-response window, unlike Codex's single-timestamp lines.
        let perf = completed.map(|c| {
            let generation_ms = (c - created).max(0);
            Perf {
                generation_ms,
                tokens_per_sec: tokens_per_sec(u.output_with_reasoning(), generation_ms),
            }
        });

        let parent_session_id = session.and_then(|s| s.parent_id.clone());
        let rec = CallRecord {
            date: dt.format("%Y-%m-%d").to_string(),
            timestamp: dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ingested_at: Self::now_iso(),
            event: Event {
                dataset: "opencode.token_usage",
                module: "token-use",
            },
            service: Service {
                name: SERVICE,
                version: session.and_then(|s| s.version.clone()),
            },
            provider,
            model: model.clone(),
            host: Host {
                name: self.host.clone(),
            },
            claude: None,
            codex: None,
            opencode: Some(OpencodeMeta {
                message_id: msg_id.to_string(),
                model,
                session_id: session_id.to_string(),
                project,
                session_title: session.and_then(|s| s.title.clone()),
                agent: v
                    .get("agent")
                    .or_else(|| v.get("mode"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                stop_reason: v.get("finish").and_then(Value::as_str).map(str::to_string),
                is_sidechain: parent_session_id.is_some(),
                parent_session_id,
                channel: channel.to_string(),
            }),
            tokens: u.to_tokens(),
            perf,
            tools: Tools {
                use_count: tools.0,
                names: tools.1,
            },
            // opencode does not report provider-side server tools separately.
            server_tool_use: None,
        };
        Some(OutputRecord::Call(Box::new(rec)))
    }
}

impl PollCollector for OpencodeCollector {
    fn name(&self) -> &'static str {
        SERVICE
    }
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn poll(&mut self, state: &mut State) -> Vec<OutputRecord> {
        let now = Utc::now();
        let mut out = Vec::new();
        for (db_path, channel) in self.enumerate() {
            match self.scan_db(&db_path, &channel, now, state) {
                Ok(mut recs) => {
                    if !recs.is_empty() {
                        debug!(db = %db_path.display(), channel = %channel, calls = recs.len(), "opencode: emitting settled calls");
                    }
                    out.append(&mut recs);
                }
                Err(e) => warn!(db = %db_path.display(), error = %e, "opencode: scan failed"),
            }
        }
        out
    }
}

/// Ledger/cursor namespace for a database.
fn conn_ns(db_path: &Path) -> String {
    db_path.to_string_lossy().to_string()
}

/// The `session` columns we surface, keyed by session id.
struct SessionRow {
    title: Option<String>,
    version: Option<String>,
    parent_id: Option<String>,
    directory: Option<String>,
    model: Option<String>,
}

/// Load every session row. Sessions are few (one per opencode run) and the join
/// would otherwise repeat per message, so this is a map lookup instead.
fn load_sessions(conn: &Connection) -> rusqlite::Result<HashMap<String, SessionRow>> {
    let mut stmt =
        conn.prepare("SELECT id, title, version, parent_id, directory, model FROM session")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            SessionRow {
                title: r.get(1)?,
                version: r.get(2)?,
                parent_id: r.get(3)?,
                directory: r.get(4)?,
                // `session.model` is JSON (`{"id":...,"providerID":...}`); the
                // per-message modelID is authoritative, so this is only a
                // fallback and is stored raw.
                model: r
                    .get::<_, Option<String>>(5)?
                    .and_then(|m| session_model_id(&m)),
            },
        ))
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Pull `.id` out of the session's JSON `model` column.
fn session_model_id(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
}

/// Tool calls attached to one message: `(count, sorted unique names)`.
fn load_tools(stmt: &mut rusqlite::Statement<'_>, msg_id: &str) -> (u32, Vec<String>) {
    let mut count = 0u32;
    let mut names = BTreeSet::new();
    let Ok(rows) = stmt.query_map([msg_id], |r| r.get::<_, String>(0)) else {
        return (0, Vec::new());
    };
    for data in rows.filter_map(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        count += 1;
        if let Some(name) = v.get("tool").and_then(Value::as_str) {
            names.insert(name.to_string());
        }
    }
    (count, names.into_iter().collect())
}

/// A message is final once the provider has closed it out.
fn is_settled(v: &Value) -> bool {
    v.pointer("/time/completed")
        .and_then(Value::as_i64)
        .is_some()
}

/// Token counts as opencode reports them, before schema mapping.
#[derive(Default, Debug, PartialEq)]
struct TokenFields {
    input: u64,
    output: u64,
    reasoning: u64,
    cache_read: u64,
    cache_write: u64,
}

impl TokenFields {
    fn from_message(v: &Value) -> Self {
        let t = v.get("tokens");
        let get = |key: &str| {
            t.and_then(|t| t.get(key))
                .and_then(Value::as_f64)
                .map(|n| n.max(0.0) as u64)
                .unwrap_or(0)
        };
        let cache = |key: &str| {
            t.and_then(|t| t.pointer(&format!("/cache/{key}")))
                .and_then(Value::as_f64)
                .map(|n| n.max(0.0) as u64)
                .unwrap_or(0)
        };
        TokenFields {
            input: get("input"),
            output: get("output"),
            reasoning: get("reasoning"),
            cache_read: cache("read"),
            cache_write: cache("write"),
        }
    }

    /// Nothing billed — an aborted or failed call.
    fn is_empty(&self) -> bool {
        self.input == 0
            && self.output == 0
            && self.reasoning == 0
            && self.cache_read == 0
            && self.cache_write == 0
    }

    /// Response side, made comparable with the other services (see module docs).
    fn output_with_reasoning(&self) -> u64 {
        self.output + self.reasoning
    }

    fn to_tokens(&self) -> Tokens {
        let total_input = self.input + self.cache_read + self.cache_write;
        let output = self.output_with_reasoning();
        Tokens {
            input: self.input,
            output,
            cache_read_input: self.cache_read,
            cache_creation_input: self.cache_write,
            // opencode does not expose Anthropic's TTL split.
            cache_creation_ephemeral_5m_input: 0,
            cache_creation_ephemeral_1h_input: 0,
            total_input,
            total: total_input + output,
            reasoning: Some(self.reasoning),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an opencode-shaped database with the columns and JSON we read.
    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, slug TEXT,
                directory TEXT, title TEXT, version TEXT, model TEXT, agent TEXT,
                time_created INTEGER, time_updated INTEGER,
                tokens_input INTEGER DEFAULT 0, tokens_output INTEGER DEFAULT 0,
                tokens_reasoning INTEGER DEFAULT 0, tokens_cache_read INTEGER DEFAULT 0,
                tokens_cache_write INTEGER DEFAULT 0
             );
             CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                time_updated INTEGER, data TEXT
             );
             CREATE TABLE part (
                id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                time_created INTEGER, time_updated INTEGER, data TEXT
             );",
        )
        .unwrap();
        conn.close().unwrap();
    }

    fn add_session(path: &Path, id: &str, parent: Option<&str>) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO session
               (id, project_id, parent_id, slug, directory, title, version, model, agent,
                time_created, time_updated)
             VALUES (?1,'p1',?2,'slug','/Users/dave/dev/proj','A Title','1.15.10',
                     '{\"id\":\"fallback-model\",\"providerID\":\"lmstudio\"}','build',
                     1789076993787, 1789076993787)",
            rusqlite::params![id, parent],
        )
        .unwrap();
        conn.close().unwrap();
    }

    struct Msg<'a> {
        id: &'a str,
        session: &'a str,
        created: i64,
        completed: Option<i64>,
        input: u64,
        output: u64,
        reasoning: u64,
        cache_read: u64,
        cache_write: u64,
        role: &'a str,
        error: bool,
    }

    impl<'a> Msg<'a> {
        fn new(id: &'a str, session: &'a str, created: i64) -> Self {
            Msg {
                id,
                session,
                created,
                completed: Some(created + 10_000),
                input: 100,
                output: 20,
                reasoning: 5,
                cache_read: 0,
                cache_write: 0,
                role: "assistant",
                error: false,
            }
        }
    }

    fn add_message(path: &Path, m: &Msg) {
        let time = match m.completed {
            Some(c) => format!(r#"{{"created":{},"completed":{}}}"#, m.created, c),
            None => format!(r#"{{"created":{}}}"#, m.created),
        };
        let err = if m.error {
            r#","error":{"name":"MessageAbortedError","data":{"message":"Aborted"}}"#
        } else {
            ""
        };
        let data = format!(
            r#"{{"role":"{}","agent":"build","mode":"build","finish":"stop",
                 "path":{{"cwd":"/Users/dave/dev/proj","root":"/Users/dave/dev/proj"}},
                 "modelID":"google/gemma-4-26b-a4b-qat","providerID":"lmstudio",
                 "tokens":{{"total":0,"input":{},"output":{},"reasoning":{},
                            "cache":{{"read":{},"write":{}}}}},
                 "time":{}{}}}"#,
            m.role, m.input, m.output, m.reasoning, m.cache_read, m.cache_write, time, err
        );
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO message (id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            rusqlite::params![m.id, m.session, m.created, data],
        )
        .unwrap();
        conn.close().unwrap();
    }

    fn add_tool_part(path: &Path, part_id: &str, msg_id: &str, tool: &str) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?1, ?2, 's1', 1, 1, ?3)",
            rusqlite::params![
                part_id,
                msg_id,
                format!(r#"{{"type":"tool","tool":"{tool}","callID":"c1"}}"#)
            ],
        )
        .unwrap();
        conn.close().unwrap();
    }

    /// A database with one settled assistant call, ready to poll.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode-stable.db");
        make_db(&db);
        add_session(&db, "s1", None);
        (dir, db)
    }

    fn state() -> (tempfile::TempDir, State) {
        let dir = tempfile::tempdir().unwrap();
        let st = State::load(dir.path()).unwrap();
        (dir, st)
    }

    /// Wall-clock now in ms. In-flight fixtures must be recent, or the
    /// abandoned-call rule (correctly) treats them as dead.
    fn now_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    fn collector(dir: &Path) -> OpencodeCollector {
        OpencodeCollector::new(dir, "testhost".into())
    }

    fn calls(recs: &[OutputRecord]) -> Vec<&CallRecord> {
        recs.iter()
            .map(|r| match r {
                OutputRecord::Call(c) => &**c,
                _ => panic!("expected call record"),
            })
            .collect()
    }

    fn one_call(recs: &[OutputRecord]) -> &CallRecord {
        assert_eq!(recs.len(), 1, "expected exactly one record");
        calls(recs)[0]
    }

    #[test]
    fn emits_one_call_per_assistant_message_with_token_mapping() {
        let (dir, db) = fixture();
        let mut m = Msg::new("msg_a", "s1", 1789076993787);
        m.input = 6844;
        m.output = 11;
        m.reasoning = 30;
        m.cache_read = 1792;
        m.cache_write = 8;
        add_message(&db, &m);
        add_tool_part(&db, "prt_1", "msg_a", "write");
        add_tool_part(&db, "prt_2", "msg_a", "read");

        let (_d, mut st) = state();
        let recs = collector(dir.path()).poll(&mut st);
        let c = one_call(&recs);

        assert_eq!(c.service.name, "opencode");
        assert_eq!(c.event.dataset, "opencode.token_usage");
        assert_eq!(c.provider, "lmstudio", "per-message provider, not a constant");
        assert_eq!(c.model, "google/gemma-4-26b-a4b-qat");
        assert_eq!(c.service.version.as_deref(), Some("1.15.10"));
        assert_eq!(c.host.name, "testhost");

        assert_eq!(c.tokens.input, 6844);
        // Response side is inclusive of reasoning (see module docs).
        assert_eq!(c.tokens.output, 11 + 30);
        assert_eq!(c.tokens.reasoning, Some(30));
        assert_eq!(c.tokens.cache_read_input, 1792);
        assert_eq!(c.tokens.cache_creation_input, 8);
        assert_eq!(c.tokens.total_input, 6844 + 1792 + 8);
        assert_eq!(c.tokens.total, 6844 + 1792 + 8 + 41);

        let meta = c.opencode.as_ref().unwrap();
        assert_eq!(meta.message_id, "msg_a");
        assert_eq!(meta.session_id, "s1");
        assert_eq!(meta.project, "/Users/dave/dev/proj");
        assert_eq!(meta.session_title.as_deref(), Some("A Title"));
        assert_eq!(meta.agent.as_deref(), Some("build"));
        assert_eq!(meta.stop_reason.as_deref(), Some("stop"));
        assert_eq!(meta.channel, "stable");
        assert!(!meta.is_sidechain);
        assert_eq!(c.tools.use_count, 2);
        assert_eq!(c.tools.names, vec!["read".to_string(), "write".to_string()]);
        assert!(c.claude.is_none() && c.codex.is_none());
    }

    #[test]
    fn derives_per_call_throughput_from_the_completion_window() {
        let (dir, db) = fixture();
        let mut m = Msg::new("msg_a", "s1", 1789076993787);
        m.completed = Some(1789076993787 + 2000); // 2s
        m.output = 90;
        m.reasoning = 10;
        add_message(&db, &m);

        let (_d, mut st) = state();
        let recs = collector(dir.path()).poll(&mut st);
        let perf = one_call(&recs).perf.as_ref().expect("opencode records timing");
        assert_eq!(perf.generation_ms, 2000);
        // 100 response tokens over 2s.
        assert_eq!(perf.tokens_per_sec, Some(50.0));
    }

    #[test]
    fn in_flight_call_is_held_and_emitted_once_it_completes() {
        let (dir, db) = fixture();
        let mut m = Msg::new("msg_a", "s1", now_ms());
        m.completed = None; // still streaming
        add_message(&db, &m);

        let (_d, mut st) = state();
        let mut c = collector(dir.path());
        assert!(c.poll(&mut st).is_empty(), "in-flight call is not emitted");

        // The watermark must not have advanced past it, or it would be stranded.
        m.completed = Some(m.created + 5000);
        add_message(&db, &m);
        let recs = c.poll(&mut st);
        assert_eq!(one_call(&recs).opencode.as_ref().unwrap().message_id, "msg_a");
        assert!(c.poll(&mut st).is_empty(), "and only once");
    }

    #[test]
    fn a_later_call_does_not_strand_an_earlier_in_flight_one() {
        let (dir, db) = fixture();
        let now = now_ms();
        let mut open = Msg::new("msg_open", "s1", now);
        open.completed = None;
        add_message(&db, &open);
        add_message(&db, &Msg::new("msg_done", "s1", now + 1000));

        let (_d, mut st) = state();
        let mut c = collector(dir.path());
        // The settled later call emits; the earlier open one does not.
        let recs = c.poll(&mut st);
        assert_eq!(one_call(&recs).opencode.as_ref().unwrap().message_id, "msg_done");

        // Completing the older call still emits it, despite a newer call having
        // already been written past it.
        open.completed = Some(now + 500);
        add_message(&db, &open);
        let recs = c.poll(&mut st);
        assert_eq!(one_call(&recs).opencode.as_ref().unwrap().message_id, "msg_open");
    }

    /// A call killed mid-stream never gains `time.completed`. It must not pin the
    /// watermark behind it forever — real databases contain these.
    #[test]
    fn a_never_completed_call_is_abandoned_rather_than_pinning_the_watermark() {
        let (dir, db) = fixture();
        let ancient = now_ms() - Duration::from_secs(30 * 24 * 3600).as_millis() as i64;
        let mut stranded = Msg::new("msg_stranded", "s1", ancient);
        stranded.completed = None;
        add_message(&db, &stranded);
        add_message(&db, &Msg::new("msg_after", "s1", ancient + 1000));

        let (_d, mut st) = state();
        let mut c = collector(dir.path());
        let recs = c.poll(&mut st);
        assert_eq!(one_call(&recs).opencode.as_ref().unwrap().message_id, "msg_after");

        // The watermark cleared both, so the rescan window does not stay anchored
        // to the stranded call.
        assert!(st.offset_for(&db) as i64 >= ancient + 1000);
        assert!(c.poll(&mut st).is_empty());
    }

    #[test]
    fn aborted_and_non_assistant_messages_are_skipped() {
        let (dir, db) = fixture();
        let mut aborted = Msg::new("msg_abort", "s1", 1000);
        aborted.error = true;
        aborted.input = 0;
        aborted.output = 0;
        aborted.reasoning = 0;
        add_message(&db, &aborted);

        let mut user = Msg::new("msg_user", "s1", 1100);
        user.role = "user";
        add_message(&db, &user);

        add_message(&db, &Msg::new("msg_real", "s1", 1200));

        let (_d, mut st) = state();
        let recs = collector(dir.path()).poll(&mut st);
        let c = one_call(&recs);
        assert_eq!(c.opencode.as_ref().unwrap().message_id, "msg_real");
    }

    #[test]
    fn child_session_calls_are_flagged_as_sidechain() {
        let (dir, db) = fixture();
        add_session(&db, "s2", Some("s1"));
        add_message(&db, &Msg::new("msg_child", "s2", 1000));

        let (_d, mut st) = state();
        let recs = collector(dir.path()).poll(&mut st);
        let meta = one_call(&recs).opencode.as_ref().unwrap();
        assert!(meta.is_sidechain);
        assert_eq!(meta.parent_session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn each_call_is_emitted_at_most_once_across_restarts() {
        let (dir, db) = fixture();
        add_message(&db, &Msg::new("msg_a", "s1", 1000));

        let statedir = tempfile::tempdir().unwrap();
        {
            let mut st = State::load(statedir.path()).unwrap();
            assert_eq!(collector(dir.path()).poll(&mut st).len(), 1);
            st.save().unwrap();
        }
        // Fresh State load (simulated restart) must not re-emit.
        let mut st = State::load(statedir.path()).unwrap();
        assert!(
            collector(dir.path()).poll(&mut st).is_empty(),
            "persisted ledger blocks re-emit after restart"
        );
    }

    /// The install-time guarantee: a fresh checkpoint replays *all* history,
    /// however old, in one poll.
    #[test]
    fn a_fresh_checkpoint_backfills_the_entire_history() {
        let (dir, db) = fixture();
        // Span years, oldest first, to prove nothing recency-gates the scan.
        let ancient = 1_600_000_000_000i64; // 2020
        for (i, created) in [ancient, ancient + 86_400_000, 1789076993787i64]
            .iter()
            .enumerate()
        {
            add_message(&db, &Msg::new(&format!("msg_{i}"), "s1", *created));
        }

        let (_d, mut st) = state();
        let recs = collector(dir.path()).poll(&mut st);
        assert_eq!(recs.len(), 3, "every historical call is emitted");
        let dates: Vec<_> = calls(&recs).iter().map(|c| c.date.clone()).collect();
        assert_eq!(dates[0], "2020-09-13", "oldest history is routed to its own daily file");
        assert!(dates[2].starts_with("2026-"));
    }

    /// The install-day case: opencode is added to a daemon that has *already*
    /// been running for the other collectors, so `state.json` exists and is
    /// non-empty. Its cursor namespace is per-database, so an unrelated
    /// checkpoint must not be mistaken for "already caught up" — the full
    /// history still has to land.
    #[test]
    fn adding_the_collector_to_an_existing_checkpoint_still_backfills() {
        let (dir, db) = fixture();
        for i in 0..4 {
            add_message(&db, &Msg::new(&format!("msg_{i}"), "s1", 1_600_000_000_000 + i as i64));
        }

        let statedir = tempfile::tempdir().unwrap();
        {
            // A checkpoint from the other collectors: file offsets and a Hermes
            // ledger, but nothing about opencode.
            let mut st = State::load(statedir.path()).unwrap();
            st.set_offset(Path::new("/Users/dave/.claude/projects/enc/s1.jsonl"), 4096, None);
            st.set_offset(Path::new("/Users/dave/.codex/sessions/2026/09/10/rollout.jsonl"), 91, None);
            st.mark("/Users/dave/.hermes/state.db", "some-hermes-session");
            st.save().unwrap();
        }

        let mut st = State::load(statedir.path()).unwrap();
        let recs = collector(dir.path()).poll(&mut st);
        assert_eq!(recs.len(), 4, "full opencode history lands despite a pre-existing checkpoint");
        // ...and the other collectors' checkpoints are left intact.
        assert_eq!(
            st.offset_for(Path::new("/Users/dave/.claude/projects/enc/s1.jsonl")),
            4096
        );
        assert!(st.is_marked("/Users/dave/.hermes/state.db", "some-hermes-session"));
    }

    /// Re-ingesting from scratch is a matter of clearing the checkpoint, so a
    /// full re-run must reproduce the identical record set.
    #[test]
    fn clearing_the_checkpoint_replays_everything_identically() {
        let (dir, db) = fixture();
        for i in 0..5 {
            add_message(&db, &Msg::new(&format!("msg_{i}"), "s1", 1000 + i as i64 * 10));
        }

        let (_d1, mut first) = state();
        let a = collector(dir.path()).poll(&mut first);
        let (_d2, mut fresh) = state();
        let b = collector(dir.path()).poll(&mut fresh);

        assert_eq!(a.len(), 5);
        let ids = |r: &[OutputRecord]| -> Vec<String> {
            calls(r)
                .iter()
                .map(|c| c.opencode.as_ref().unwrap().message_id.clone())
                .collect()
        };
        assert_eq!(ids(&a), ids(&b), "a from-scratch replay is identical");
    }

    #[test]
    fn ledger_is_pruned_to_the_rescan_window() {
        let (dir, db) = fixture();
        for i in 0..50 {
            add_message(&db, &Msg::new(&format!("msg_{i:02}"), "s1", 1000 + i as i64));
        }
        let (_d, mut st) = state();
        let mut c = collector(dir.path());
        assert_eq!(c.poll(&mut st).len(), 50);

        let ns = db.to_string_lossy().to_string();
        let marks = |st: &State| st.marks.get(&ns).map(|m| m.len()).unwrap_or(0);
        // The emitting poll must keep every mark it just wrote: those ids are all
        // still at or above the watermark, so a rescan could return them.
        assert_eq!(marks(&st), 50);

        // Once the watermark has moved past them, only the boundary row can come
        // back, and the ledger converges to that window instead of growing one
        // mark per call forever.
        assert!(c.poll(&mut st).is_empty(), "pruned calls are not re-emitted");
        assert!(marks(&st) <= 1, "ledger not pruned, got {}", marks(&st));
        assert!(c.poll(&mut st).is_empty(), "pruning does not resurrect calls");
    }

    #[test]
    fn each_database_channel_is_tracked_separately() {
        let dir = tempfile::tempdir().unwrap();
        for channel in ["stable", "dev"] {
            let db = dir.path().join(format!("opencode-{channel}.db"));
            make_db(&db);
            add_session(&db, "s1", None);
            add_message(&db, &Msg::new("msg_a", "s1", 1000));
        }
        let (_d, mut st) = state();
        let recs = collector(dir.path()).poll(&mut st);
        let mut channels: Vec<_> = calls(&recs)
            .iter()
            .map(|c| c.opencode.as_ref().unwrap().channel.clone())
            .collect();
        channels.sort();
        assert_eq!(channels, vec!["dev".to_string(), "stable".to_string()]);
        assert_eq!(recs.len(), 2, "same message id in two channels is not deduped away");
    }

    /// Reconcile a *real* opencode database against opencode's own running
    /// totals: the sum of what we emit must equal the `session` aggregate
    /// columns, component by component.
    ///
    /// This is the check that licenses reading only the `message` table and
    /// ignoring `step-finish` parts — if we double-counted, or missed a call,
    /// these sums would diverge. Opt-in because it needs real data:
    ///
    /// ```text
    /// sqlite3 "file:$HOME/.local/share/opencode/opencode-stable.db?mode=ro" \
    ///     "VACUUM INTO '/tmp/oc-snapshot/opencode-stable.db'"
    /// TOKEN_USE_OPENCODE_TEST_DB=/tmp/oc-snapshot \
    ///     cargo test reconciles_with_session_aggregates -- --ignored --nocapture
    /// ```
    ///
    /// Use a snapshot (`VACUUM INTO` folds in the WAL), not the live file, so
    /// the totals cannot move mid-test.
    #[test]
    #[ignore = "requires a real opencode database; see doc comment"]
    fn reconciles_with_session_aggregates() {
        let Some(dir) = std::env::var_os("TOKEN_USE_OPENCODE_TEST_DB") else {
            panic!("set TOKEN_USE_OPENCODE_TEST_DB to a dir holding opencode-<channel>.db");
        };
        let dir = PathBuf::from(dir);

        let (_d, mut st) = state();
        let mut c = collector(&dir);
        let recs = c.poll(&mut st);
        assert!(!recs.is_empty(), "no records emitted from {}", dir.display());
        // Idempotency against real data: a second poll of an unchanged database
        // must add nothing.
        assert!(c.poll(&mut st).is_empty(), "re-poll duplicated records");

        let mut got = TokenFields::default();
        for c in calls(&recs) {
            got.input += c.tokens.input;
            got.output += c.tokens.output;
            got.reasoning += c.tokens.reasoning.unwrap_or(0);
            got.cache_read += c.tokens.cache_read_input;
            got.cache_write += c.tokens.cache_creation_input;
        }

        let mut want = TokenFields::default();
        for (db, _) in collector(&dir).enumerate() {
            let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT COALESCE(SUM(tokens_input),0), COALESCE(SUM(tokens_output),0), \
                            COALESCE(SUM(tokens_reasoning),0), COALESCE(SUM(tokens_cache_read),0), \
                            COALESCE(SUM(tokens_cache_write),0) FROM session",
                )
                .unwrap();
            let row = stmt
                .query_row([], |r| {
                    Ok(TokenFields {
                        input: r.get::<_, i64>(0)? as u64,
                        output: r.get::<_, i64>(1)? as u64,
                        reasoning: r.get::<_, i64>(2)? as u64,
                        cache_read: r.get::<_, i64>(3)? as u64,
                        cache_write: r.get::<_, i64>(4)? as u64,
                    })
                })
                .unwrap();
            want.input += row.input;
            want.output += row.output;
            want.reasoning += row.reasoning;
            want.cache_read += row.cache_read;
            want.cache_write += row.cache_write;
        }

        // Daily-file routing across the whole history, which is what a
        // from-the-beginning backfill has to get right.
        let mut per_day: std::collections::BTreeMap<String, usize> = Default::default();
        for c in calls(&recs) {
            *per_day.entry(c.date.clone()).or_default() += 1;
        }
        println!("emitted {} calls across {} day(s):", recs.len(), per_day.len());
        for (day, n) in &per_day {
            println!("  {day}  {n:>5}");
        }
        println!(
            "sample record:\n{}",
            serde_json::to_string_pretty(&recs[0]).unwrap()
        );
        println!("ours    {got:?}");
        println!("opencode {want:?}");

        assert_eq!(got.input, want.input, "input tokens");
        assert_eq!(got.cache_read, want.cache_read, "cache read tokens");
        assert_eq!(got.cache_write, want.cache_write, "cache write tokens");
        assert_eq!(got.reasoning, want.reasoning, "reasoning tokens");
        // Ours folds reasoning into the response side; opencode keeps them apart.
        assert_eq!(
            got.output,
            want.output + want.reasoning,
            "output tokens (reasoning-inclusive)"
        );
    }

    /// The emitted field names are the contract with Elasticsearch, so pin the
    /// ones unique to this collector.
    #[test]
    fn serializes_with_the_expected_field_names() {
        let (dir, db) = fixture();
        add_message(&db, &Msg::new("msg_a", "s1", 1789076993787));
        let (_d, mut st) = state();
        let recs = collector(dir.path()).poll(&mut st);

        let v: Value = serde_json::from_str(&serde_json::to_string(&recs[0]).unwrap()).unwrap();
        assert_eq!(v["service"]["name"], "opencode");
        assert_eq!(v["event"]["dataset"], "opencode.token_usage");
        assert_eq!(v["provider"], "lmstudio");
        assert_eq!(v["opencode"]["message_id"], "msg_a");
        assert_eq!(v["opencode"]["project"], "/Users/dave/dev/proj");
        assert_eq!(v["opencode"]["channel"], "stable");
        assert!(v["tokens"]["total_input"].is_number());
        assert!(v["perf"]["generation_ms"].is_number());
        assert!(v["@timestamp"].as_str().unwrap().starts_with("2026-"));
        // Sibling metadata must not leak into an opencode document.
        assert!(v.get("claude").is_none() && v.get("codex").is_none());
        // `date` is a routing field only and must never be serialized.
        assert!(v.get("date").is_none());
    }

    #[test]
    fn missing_data_dir_is_not_an_error() {
        let (_d, mut st) = state();
        let recs = collector(Path::new("/nonexistent/opencode")).poll(&mut st);
        assert!(recs.is_empty());
    }
}
