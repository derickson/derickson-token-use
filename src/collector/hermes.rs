//! Hermes AI-agent collector (SQLite poll source).
//!
//! Hermes stores its state in SQLite, not append-only text, so it can't be
//! byte-tailed like a Claude Code transcript. Instead this [`PollCollector`]
//! scans each persona database on demand.
//!
//! Token usage lives in the `sessions` table as a per-session aggregate
//! (`messages.token_count` is unpopulated), so we emit **one record per
//! session** — and **exactly once, ever**. A session is only written after it
//! has *settled* (its `ended_at` is set, or its newest message is older than the
//! idle window), and its id is then recorded in a durable "already emitted"
//! ledger ([`State::mark`]) so it is never written again, even across restarts.
//! This makes the NDJSON locally deduplicated: every line is a unique, final
//! session — which is what a Fleet data stream needs, since those assign their
//! own `_id` and cannot upsert.
//!
//! The cost of once-only is fidelity: if a settled session later resumes, its
//! earlier snapshot is already committed and the delta is not re-emitted. The
//! idle window (`TOKEN_USE_HERMES_IDLE_SECS`) trades latency against that risk;
//! `ended_at` is the precise signal and always wins.
//!
//! "Personas" are profiles: `~/.hermes/state.db` is the `default` persona and
//! each `~/.hermes/profiles/<name>/state.db` is persona `<name>`. (Backup copies
//! under `state-snapshots/` are deliberately not enumerated.)

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use tracing::{debug, warn};

use crate::collector::PollCollector;
use crate::record::*;
use crate::state::State;

const PROVIDER: &str = "openai-codex";
const SERVICE: &str = "hermes";

pub struct HermesCollector {
    /// Root of the Hermes install, normally `~/.hermes`.
    hermes_dir: PathBuf,
    host: String,
    /// A session is finalized once it has been idle this long (unless ended).
    idle: Duration,
}

impl HermesCollector {
    pub fn new(hermes_dir: &Path, host: String, idle: Duration) -> Self {
        HermesCollector {
            hermes_dir: hermes_dir.to_path_buf(),
            host,
            idle,
        }
    }

    fn now_iso() -> String {
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// All persona databases as `(path, persona)` pairs. The default persona's
    /// DB plus one per `profiles/<name>/state.db`. Missing files are skipped.
    fn enumerate(&self) -> Vec<(PathBuf, String)> {
        let mut dbs = Vec::new();

        let root = self.hermes_dir.join("state.db");
        if root.is_file() {
            dbs.push((root, "default".to_string()));
        }

        let profiles = self.hermes_dir.join("profiles");
        if let Ok(entries) = std::fs::read_dir(&profiles) {
            for entry in entries.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let db = entry.path().join("state.db");
                if db.is_file() {
                    let persona = entry.file_name().to_string_lossy().to_string();
                    dbs.push((db, persona));
                }
            }
        }
        dbs
    }

    /// Scan one persona DB and emit each **settled, not-yet-emitted** session
    /// exactly once, marking it in the durable ledger (`ns` = the DB path).
    ///
    /// A session is settled when it has ended or its newest message is older than
    /// the idle window. We scan all sessions every poll (cheap at these volumes);
    /// the ledger — not a cursor — is what guarantees once-only emission.
    fn scan_db(
        &self,
        db_path: &Path,
        persona: &str,
        now: DateTime<Utc>,
        ns: &str,
        state: &mut State,
    ) -> rusqlite::Result<Vec<OutputRecord>> {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;

        // (session_id, ended_at, latest message timestamp) for every active session.
        let candidates: Vec<(String, Option<f64>, Option<f64>)> = {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.ended_at, MAX(m.timestamp) \
                 FROM sessions s JOIN messages m ON m.session_id = s.id \
                 WHERE s.id IS NOT NULL GROUP BY s.id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<f64>>(1)?,
                    r.get::<_, Option<f64>>(2)?,
                ))
            })?;
            rows.filter_map(Result::ok).collect()
        };

        let now_secs = now.timestamp_millis() as f64 / 1000.0;
        let cutoff = now_secs - self.idle.as_secs_f64();

        let mut out = Vec::new();
        for (session_id, ended_at, last_ts) in candidates {
            if state.is_marked(ns, &session_id) {
                continue; // already emitted — never write twice
            }
            let settled = ended_at.is_some() || last_ts.is_some_and(|t| t < cutoff);
            if !settled {
                continue; // still active — wait for it to settle
            }
            match self.build_record(&conn, persona, &session_id) {
                Ok(Some(rec)) => {
                    out.push(rec);
                    state.mark(ns, &session_id);
                }
                Ok(None) => {} // session row gone (shouldn't happen) — skip
                Err(e) => warn!(session = %session_id, error = %e, "hermes: skipping session"),
            }
        }
        Ok(out)
    }

    /// Build the current snapshot record for one session id.
    fn build_record(
        &self,
        conn: &Connection,
        persona: &str,
        session_id: &str,
    ) -> rusqlite::Result<Option<OutputRecord>> {
        let row = conn.query_row(
            "SELECT source, user_id, model, billing_provider, billing_mode, \
                    started_at, ended_at, message_count, api_call_count, \
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
                    reasoning_tokens, estimated_cost_usd, actual_cost_usd, cost_status, \
                    title, cwd, parent_session_id \
             FROM sessions WHERE id = ?1",
            [session_id],
            |r| {
                Ok(SessionRow {
                    source: r.get(0)?,
                    user_id: r.get(1)?,
                    model: r.get(2)?,
                    billing_provider: r.get(3)?,
                    billing_mode: r.get(4)?,
                    started_at: r.get(5)?,
                    ended_at: r.get(6)?,
                    message_count: r.get(7)?,
                    api_call_count: r.get(8)?,
                    input_tokens: r.get(9)?,
                    output_tokens: r.get(10)?,
                    cache_read_tokens: r.get(11)?,
                    cache_write_tokens: r.get(12)?,
                    reasoning_tokens: r.get(13)?,
                    estimated_cost_usd: r.get(14)?,
                    actual_cost_usd: r.get(15)?,
                    cost_status: r.get(16)?,
                    title: r.get(17)?,
                    cwd: r.get(18)?,
                    parent_session_id: r.get(19)?,
                })
            },
        );
        let s = match row {
            Ok(s) => s,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e),
        };

        // Prefer the session start for the event time; fall back to ingestion
        // time so a session missing `started_at` still lands in a dated file.
        let started = s.started_at.and_then(unix_to_dt);
        let (timestamp, date) = match started {
            Some(dt) => (
                dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                dt.format("%Y-%m-%d").to_string(),
            ),
            None => {
                let now = Utc::now();
                (
                    now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    now.format("%Y-%m-%d").to_string(),
                )
            }
        };

        let duration_ms = match (s.started_at, s.ended_at) {
            (Some(a), Some(b)) => Some(((b - a) * 1000.0).round() as i64),
            _ => None,
        };

        let input = as_u64(s.input_tokens);
        let output = as_u64(s.output_tokens);
        let cache_read = as_u64(s.cache_read_tokens);
        let cache_write = as_u64(s.cache_write_tokens);
        let total_input = input + cache_read + cache_write;

        let rec = HermesRecord {
            date,
            timestamp,
            ingested_at: Self::now_iso(),
            event: Event {
                dataset: "hermes.token_usage",
                module: "token-use",
            },
            service: Service {
                name: SERVICE,
                version: None,
            },
            provider: s.billing_provider.clone().unwrap_or_else(|| PROVIDER.to_string()),
            model: s.model.clone().unwrap_or_else(|| "unknown".to_string()),
            host: Host {
                name: self.host.clone(),
            },
            hermes: HermesMeta {
                session_id: session_id.to_string(),
                persona: persona.to_string(),
                ended: s.ended_at.is_some(),
                source: s.source,
                user_id: s.user_id,
                billing_provider: s.billing_provider,
                billing_mode: s.billing_mode,
                cost_status: s.cost_status,
                estimated_cost_usd: s.estimated_cost_usd,
                actual_cost_usd: s.actual_cost_usd,
                api_call_count: s.api_call_count.map(|v| v.max(0) as u64),
                message_count: s.message_count.map(|v| v.max(0) as u64),
                duration_ms,
                title: s.title,
                cwd: s.cwd,
                parent_session_id: s.parent_session_id,
            },
            tokens: Tokens {
                input,
                output,
                cache_read_input: cache_read,
                cache_creation_input: cache_write,
                cache_creation_ephemeral_5m_input: 0,
                cache_creation_ephemeral_1h_input: 0,
                total_input,
                total: total_input + output,
                reasoning: Some(as_u64(s.reasoning_tokens)),
            },
        };
        Ok(Some(OutputRecord::Hermes(Box::new(rec))))
    }
}

impl PollCollector for HermesCollector {
    fn name(&self) -> &'static str {
        SERVICE
    }
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn poll(&mut self, state: &mut State) -> Vec<OutputRecord> {
        let now = Utc::now();
        let mut out = Vec::new();
        for (db_path, persona) in self.enumerate() {
            let ns = db_path.to_string_lossy().to_string();
            match self.scan_db(&db_path, &persona, now, &ns, state) {
                Ok(mut recs) => {
                    if !recs.is_empty() {
                        debug!(db = %db_path.display(), persona = %persona, sessions = recs.len(), "hermes: emitting settled sessions");
                    }
                    out.append(&mut recs);
                }
                Err(e) => warn!(db = %db_path.display(), error = %e, "hermes: scan failed"),
            }
        }
        out
    }
}

/// One row of the `sessions` table (only the columns we surface).
struct SessionRow {
    source: Option<String>,
    user_id: Option<String>,
    model: Option<String>,
    billing_provider: Option<String>,
    billing_mode: Option<String>,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    message_count: Option<i64>,
    api_call_count: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    estimated_cost_usd: Option<f64>,
    actual_cost_usd: Option<f64>,
    cost_status: Option<String>,
    title: Option<String>,
    cwd: Option<String>,
    parent_session_id: Option<String>,
}

fn as_u64(v: Option<i64>) -> u64 {
    v.map(|n| n.max(0) as u64).unwrap_or(0)
}

/// Convert a fractional unix timestamp (seconds) to a UTC datetime.
fn unix_to_dt(secs: f64) -> Option<DateTime<Utc>> {
    let whole = secs.trunc() as i64;
    let nanos = (secs.fract() * 1_000_000_000.0).round() as u32;
    DateTime::from_timestamp(whole, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Create a Hermes-shaped state.db at `path` with the columns we read.
    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, source TEXT, user_id TEXT, model TEXT,
                billing_provider TEXT, billing_mode TEXT, started_at REAL, ended_at REAL,
                message_count INTEGER, api_call_count INTEGER,
                input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER,
                cache_write_tokens INTEGER, reasoning_tokens INTEGER,
                estimated_cost_usd REAL, actual_cost_usd REAL, cost_status TEXT,
                title TEXT, cwd TEXT, parent_session_id TEXT
             );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT,
                timestamp REAL, token_count INTEGER
             );",
        )
        .unwrap();
        conn.close().unwrap();
    }

    fn insert_session(path: &Path, id: &str, model: &str, input: i64, output: i64, ended: Option<f64>) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO sessions
               (id, source, model, billing_provider, billing_mode, started_at, ended_at,
                message_count, api_call_count, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens, cost_status, title)
             VALUES (?1,'discord',?2,'openai-codex','subscription_included',
                     1781561091.5, ?3, 10, 4, ?4, ?5, 5000, 0, 42, 'included', 'A Title')
             ON CONFLICT(id) DO UPDATE SET
                input_tokens=excluded.input_tokens, output_tokens=excluded.output_tokens,
                ended_at=excluded.ended_at",
            rusqlite::params![id, model, ended, input, output],
        )
        .unwrap();
        conn.close().unwrap();
    }

    fn add_message_at(path: &Path, session_id: &str, ts: f64) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, timestamp) VALUES (?1, 'assistant', ?2)",
            rusqlite::params![session_id, ts],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.close().unwrap();
        id
    }

    /// A long-settled message timestamp (~2026-06-15), older than any idle window.
    const OLD_TS: f64 = 1781561092.0;

    fn now_secs() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    fn collector(home: &Path, idle_secs: u64) -> HermesCollector {
        HermesCollector::new(home, "testhost".into(), Duration::from_secs(idle_secs))
    }

    fn state() -> (tempfile::TempDir, State) {
        let dir = tempfile::tempdir().unwrap();
        let st = State::load(dir.path()).unwrap();
        (dir, st)
    }

    fn one_hermes(recs: &[OutputRecord]) -> &HermesRecord {
        assert_eq!(recs.len(), 1, "expected exactly one record");
        match &recs[0] {
            OutputRecord::Hermes(h) => h,
            _ => panic!("expected hermes record"),
        }
    }

    #[test]
    fn emits_one_settled_record_per_session_with_token_mapping() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("state.db");
        make_db(&db);
        insert_session(&db, "s1", "gpt-5.5", 100, 20, Some(1781561100.5));
        add_message_at(&db, "s1", OLD_TS);

        let (_d, mut st) = state();
        let mut c = collector(home.path(), 600);
        let recs = c.poll(&mut st);
        let h = one_hermes(&recs);

        assert_eq!(h.hermes.session_id, "s1");
        assert_eq!(h.hermes.persona, "default");
        assert_eq!(h.provider, "openai-codex");
        assert_eq!(h.model, "gpt-5.5");
        assert_eq!(h.service.name, "hermes");
        assert_eq!(h.event.dataset, "hermes.token_usage");
        assert_eq!(h.tokens.input, 100);
        assert_eq!(h.tokens.output, 20);
        assert_eq!(h.tokens.cache_read_input, 5000);
        assert_eq!(h.tokens.cache_creation_input, 0);
        assert_eq!(h.tokens.total_input, 100 + 5000 + 0);
        assert_eq!(h.tokens.total, 100 + 5000 + 20);
        assert_eq!(h.tokens.reasoning, Some(42));
        assert!(h.hermes.ended);
        assert_eq!(h.hermes.duration_ms, Some(9000)); // 1781561100.5 - 1781561091.5
        assert!(h.timestamp.starts_with("2026-")); // RFC3339
        assert_eq!(h.host.name, "testhost");
    }

    #[test]
    fn each_session_is_emitted_at_most_once() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("state.db");
        make_db(&db);
        insert_session(&db, "s1", "gpt-5.5", 100, 20, None);
        add_message_at(&db, "s1", OLD_TS);

        let (_d, mut st) = state();
        let mut c = collector(home.path(), 600);
        assert_eq!(c.poll(&mut st).len(), 1, "settled session emits once");

        // Even if the session grows and gains a new (still-old) message, the
        // ledger prevents a second write — the NDJSON stays unique.
        insert_session(&db, "s1", "gpt-5.5", 150, 55, None);
        add_message_at(&db, "s1", OLD_TS + 1.0);
        assert!(c.poll(&mut st).is_empty(), "already-emitted session never re-emits");
    }

    #[test]
    fn ledger_survives_restart() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("state.db");
        make_db(&db);
        insert_session(&db, "s1", "gpt-5.5", 100, 20, None);
        add_message_at(&db, "s1", OLD_TS);

        let statedir = tempfile::tempdir().unwrap();
        {
            let mut st = State::load(statedir.path()).unwrap();
            let mut c = collector(home.path(), 600);
            assert_eq!(c.poll(&mut st).len(), 1);
            st.save().unwrap();
        }
        // Fresh State load (simulated restart) must not re-emit the session.
        let mut st = State::load(statedir.path()).unwrap();
        let mut c = collector(home.path(), 600);
        assert!(c.poll(&mut st).is_empty(), "persisted ledger blocks re-emit after restart");
    }

    #[test]
    fn active_session_waits_until_settled() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("state.db");
        make_db(&db);
        // Session with a brand-new message and no ended_at: still active.
        insert_session(&db, "s1", "gpt-5.5", 100, 20, None);
        add_message_at(&db, "s1", now_secs());

        let (_d, mut st) = state();
        let mut c = collector(home.path(), 3600); // 1h idle window
        assert!(c.poll(&mut st).is_empty(), "fresh active session is not emitted yet");

        // Marking it ended finalizes it immediately, regardless of idle window.
        insert_session(&db, "s1", "gpt-5.5", 120, 30, Some(now_secs()));
        let recs = c.poll(&mut st);
        let h = one_hermes(&recs);
        assert_eq!(h.tokens.output, 30);
        assert!(h.hermes.ended);
        // ...and only once.
        assert!(c.poll(&mut st).is_empty());
    }

    #[test]
    fn profile_db_yields_persona_name() {
        let home = tempfile::tempdir().unwrap();
        let prof = home.path().join("profiles").join("aitube");
        std::fs::create_dir_all(&prof).unwrap();
        let db = prof.join("state.db");
        make_db(&db);
        insert_session(&db, "p1", "gpt-5.4-mini", 10, 5, None);
        add_message_at(&db, "p1", OLD_TS);

        let (_d, mut st) = state();
        let mut c = collector(home.path(), 600);
        let recs = c.poll(&mut st);
        let h = one_hermes(&recs);
        assert_eq!(h.hermes.persona, "aitube");
        assert_eq!(h.model, "gpt-5.4-mini");
    }
}
