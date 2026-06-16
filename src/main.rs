//! token-use — a per-call AI token-usage collector.
//!
//! Watches local AI-tool transcripts (Claude Code today; OpenAI/Ollama later)
//! and emits high-fidelity NDJSON token-usage records for an Elasticsearch
//! filestream/Filebeat integration to tail.

mod collector;
mod config;
mod daemon;
mod error;
mod output;
mod record;
mod state;
mod tailer;

use anyhow::Result;
use tracing_subscriber::{fmt, EnvFilter};

fn main() -> Result<()> {
    // Operational logging to stderr (journald/launchd capture it). This is the
    // daemon's own log — separate from the NDJSON token-usage output.
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = config::Config::from_env();
    tracing::info!(
        out_dir = %config.out_dir.display(),
        state_dir = %config.state_dir.display(),
        "configuration resolved"
    );

    let mut daemon = daemon::Daemon::new(config)?;
    daemon.run()
}
