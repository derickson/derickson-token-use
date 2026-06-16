//! NDJSON output writer with daily rotation.
//!
//! Records are routed to `logs/<service>-<YYYY-MM-DD>.ndjson` by the date in
//! their `@timestamp` (so backfilled history lands in the correct dated file).
//! Each record is written as one complete `{...}\n` and flushed, so a tailing
//! Filebeat/filestream never observes a torn line.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use crate::record::OutputRecord;

pub struct OutputWriter {
    dir: PathBuf,
    prefix: String,
    // One open appender per active date; small and bounded in practice.
    handles: HashMap<String, BufWriter<File>>,
}

impl OutputWriter {
    pub fn new(dir: PathBuf, prefix: impl Into<String>) -> Self {
        OutputWriter {
            dir,
            prefix: prefix.into(),
            handles: HashMap::new(),
        }
    }

    /// Serialize and append one record to its dated file, flushing immediately.
    pub fn write(&mut self, rec: &OutputRecord) -> std::io::Result<()> {
        let date = rec.date().to_string();
        let line = serde_json::to_string(rec)?;

        let writer = self.handle_for(&date)?;
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }

    fn handle_for(&mut self, date: &str) -> std::io::Result<&mut BufWriter<File>> {
        if !self.handles.contains_key(date) {
            std::fs::create_dir_all(&self.dir)?;
            let path = self.dir.join(format!("{}-{}.ndjson", self.prefix, date));
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            self.handles
                .insert(date.to_string(), BufWriter::new(file));
        }
        Ok(self.handles.get_mut(date).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::*;

    fn call(date: &str, ts: &str) -> OutputRecord {
        OutputRecord::Call(Box::new(CallRecord {
            date: date.into(),
            timestamp: ts.into(),
            ingested_at: ts.into(),
            event: Event { dataset: "claude_code.token_usage", module: "token-use" },
            service: Service { name: "claude-code", version: None },
            provider: "anthropic",
            model: "claude-opus-4-8".into(),
            host: Host { name: "h".into() },
            claude: ClaudeMeta {
                message_id: "msg_1".into(),
                request_id: None,
                model: "claude-opus-4-8".into(),
                session_id: "s".into(),
                project: "p".into(),
                git_branch: None,
                service_tier: None,
                is_sidechain: false,
                stop_reason: None,
                entrypoint: None,
            },
            tokens: Tokens {
                input: 1, output: 2, cache_read_input: 0, cache_creation_input: 0,
                cache_creation_ephemeral_5m_input: 0, cache_creation_ephemeral_1h_input: 0,
                total_input: 1, total: 3,
            },
            perf: Perf { generation_ms: 0, tokens_per_sec: None },
            tools: Tools { use_count: 0, names: vec![] },
            server_tool_use: None,
        }))
    }

    #[test]
    fn routes_records_to_daily_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = OutputWriter::new(dir.path().to_path_buf(), "claude-code");
        w.write(&call("2026-06-15", "2026-06-15T10:00:00Z")).unwrap();
        w.write(&call("2026-06-16", "2026-06-16T10:00:00Z")).unwrap();
        w.write(&call("2026-06-16", "2026-06-16T11:00:00Z")).unwrap();

        let d15 = std::fs::read_to_string(dir.path().join("claude-code-2026-06-15.ndjson")).unwrap();
        let d16 = std::fs::read_to_string(dir.path().join("claude-code-2026-06-16.ndjson")).unwrap();
        assert_eq!(d15.lines().count(), 1);
        assert_eq!(d16.lines().count(), 2);
        // Each line is valid standalone JSON with the @timestamp field.
        for l in d16.lines() {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert!(v.get("@timestamp").is_some());
            assert_eq!(v["provider"], "anthropic");
        }
    }
}
