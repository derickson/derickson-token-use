//! Runtime configuration, resolved from defaults + environment overrides.
//!
//! Everything has a sensible default so the daemon runs with zero arguments;
//! each value can be overridden by an environment variable (handy in the
//! systemd unit / launchd plist).

use std::path::PathBuf;
use std::time::Duration;

/// Resolved configuration for a daemon run.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory that receives the NDJSON output files (`logs/` under the project).
    pub out_dir: PathBuf,
    /// Directory holding the durable checkpoint (`state.json`).
    pub state_dir: PathBuf,
    /// Home directory used to locate per-service transcript roots.
    pub home: PathBuf,
    /// Root of the Codex CLI install, normally `~/.codex`; its `sessions/`
    /// subdirectory holds the rollout transcripts.
    pub codex_dir: PathBuf,
    /// Root of the Hermes install (the SQLite poll source), normally `~/.hermes`.
    pub hermes_dir: PathBuf,
    /// opencode's data directory, normally `~/.local/share/opencode`; it holds
    /// the `opencode-<channel>.db` SQLite databases.
    pub opencode_dir: PathBuf,
    /// A Hermes session is finalized and emitted **once** after it has been idle
    /// (no new message) for this long, unless it has already ended. Keeps a
    /// growing session from being written before its totals settle.
    pub hermes_idle: Duration,
    /// Debounce window for filesystem events.
    pub debounce: Duration,
    /// Safety-net tick: rescan known files, discover new ones, checkpoint state.
    pub tick: Duration,
}

impl Config {
    /// Build configuration from the environment, falling back to defaults.
    ///
    /// Recognised variables:
    ///   * `TOKEN_USE_OUT_DIR`   — output dir (default: `<cwd-of-project>/logs`)
    ///   * `TOKEN_USE_STATE_DIR` — state dir (default: XDG state / App Support)
    ///   * `TOKEN_USE_HOME`      — home override (default: `$HOME`)
    ///   * `TOKEN_USE_CODEX_DIR`  — Codex install dir (default: `$HOME/.codex`)
    ///   * `TOKEN_USE_HERMES_DIR` — Hermes install dir (default: `$HOME/.hermes`)
    ///   * `TOKEN_USE_OPENCODE_DIR` — opencode data dir
    ///     (default: `$XDG_DATA_HOME/opencode`, else `$HOME/.local/share/opencode`)
    ///   * `TOKEN_USE_HERMES_IDLE_SECS` — settle window before a session is
    ///     emitted once (default 600)
    ///   * `TOKEN_USE_DEBOUNCE_MS` (default 1500)
    ///   * `TOKEN_USE_TICK_SECS`   (default 300)
    pub fn from_env() -> Self {
        let home = std::env::var_os("TOKEN_USE_HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));

        let codex_dir = std::env::var_os("TOKEN_USE_CODEX_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));

        let hermes_dir = std::env::var_os("TOKEN_USE_HERMES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".hermes"));

        // opencode follows the XDG data spec on both Linux and macOS (it does
        // *not* use ~/Library/Application Support), so honour XDG_DATA_HOME first
        // and fall back to the spec's own default rather than `dirs::data_dir()`.
        let opencode_dir = std::env::var_os("TOKEN_USE_OPENCODE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .unwrap_or_else(|| home.join(".local").join("share"))
                    .join("opencode")
            });

        let hermes_idle = env_u64("TOKEN_USE_HERMES_IDLE_SECS")
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(600));

        // Default output dir: the project's own `logs/` directory. We anchor to
        // the binary's working directory so the service writes where it is run;
        // operators normally set TOKEN_USE_OUT_DIR explicitly in the unit file.
        let out_dir = std::env::var_os("TOKEN_USE_OUT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("logs"));

        let state_dir = std::env::var_os("TOKEN_USE_STATE_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::state_dir().map(|d| d.join("token-use")))
            .or_else(|| dirs::data_dir().map(|d| d.join("token-use")))
            .unwrap_or_else(|| home.join(".token-use"));

        let debounce = env_u64("TOKEN_USE_DEBOUNCE_MS")
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(1500));

        let tick = env_u64("TOKEN_USE_TICK_SECS")
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(300));

        Config {
            out_dir,
            state_dir,
            home,
            codex_dir,
            hermes_dir,
            opencode_dir,
            hermes_idle,
            debounce,
            tick,
        }
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}
