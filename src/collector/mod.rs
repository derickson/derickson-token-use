//! Pluggable per-service collectors.
//!
//! A collector is a *stateful line consumer*: the tailer feeds it transcript
//! lines in file order and it emits finalized [`OutputRecord`]s as boundaries
//! are crossed. This streaming shape is what lets a collector compute per-call
//! metrics (e.g. `generation_ms`, which needs a call's first and last block
//! timestamp) before emitting.

use std::path::{Path, PathBuf};

use crate::error::ParseError;
use crate::record::OutputRecord;

pub mod claude_code;

pub trait Collector: Send {
    /// `service.name` for emitted records, e.g. `"claude-code"`.
    fn name(&self) -> &'static str;

    /// `provider` dimension, e.g. `"anthropic"`.
    fn provider(&self) -> &'static str;

    /// Filename prefix for this service's daily output files.
    fn out_prefix(&self) -> &'static str {
        self.name()
    }

    /// Directory watched recursively for this service's transcripts.
    fn watch_root(&self) -> &Path;

    /// Whether `path` is a transcript this collector should tail.
    fn owns(&self, path: &Path) -> bool;

    /// All existing transcript files, for the initial backfill.
    fn enumerate(&self) -> Vec<PathBuf>;

    /// Feed one transcript line (in file order). Returns any records finalized
    /// by *this* line — typically the previously-open call when a new
    /// `message.id` or a non-assistant line arrives, plus turn records.
    fn consume_line(
        &mut self,
        path: &Path,
        line: &str,
    ) -> Result<Vec<OutputRecord>, ParseError>;

    /// Flush any call still buffered for `path` (call at a backfilled file's EOF
    /// so a final message with no following boundary line still emits).
    fn flush(&mut self, path: &Path) -> Vec<OutputRecord>;

    /// Flush calls that have been idle (no new block) longer than `idle` across
    /// all files. Called on the periodic tick: a finished response whose
    /// boundary line is delayed/absent is emitted, while a call still actively
    /// streaming is left open to avoid a premature, partial emit.
    fn flush_idle(&mut self, idle: std::time::Duration) -> Vec<OutputRecord>;
}
