//! Typed error kinds for the parse and state layers.
//!
//! The daemon's top-level loop uses `anyhow` for contextual errors; the
//! library-ish layers below use these `thiserror` enums so callers can branch
//! on the kind (e.g. tolerate a malformed transcript line but fail hard on a
//! corrupt state file).

use thiserror::Error;

/// Error produced while turning a single transcript line into records.
///
/// A `ParseError` is non-fatal to the daemon: the offending line is logged and
/// skipped so one bad line never stalls the stream.
#[derive(Debug, Error)]
pub enum ParseError {
    /// The line was not valid JSON.
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// The line was valid JSON but did not contain the fields we expect for the
    /// record type it claimed to be.
    #[error("unexpected transcript shape: {0}")]
    Shape(String),
}

/// Error produced while loading or persisting the checkpoint state file.
#[derive(Debug, Error)]
pub enum StateError {
    #[error("state I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("state (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),
}
