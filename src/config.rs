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
    ///   * `TOKEN_USE_DEBOUNCE_MS` (default 1500)
    ///   * `TOKEN_USE_TICK_SECS`   (default 300)
    pub fn from_env() -> Self {
        let home = std::env::var_os("TOKEN_USE_HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));

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
            debounce,
            tick,
        }
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}
