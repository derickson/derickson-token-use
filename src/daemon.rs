//! The daemon: backfill, then a filesystem-watch loop with a periodic safety
//! tick. Single-threaded by design — all state, dedup, and output mutation
//! happen on the main loop, so there is no locking.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{select, unbounded};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tracing::{debug, info, warn};

use crate::collector::claude_code::ClaudeCodeCollector;
use crate::collector::Collector;
use crate::config::Config;
use crate::output::OutputWriter;
use crate::record::OutputRecord;
use crate::state::State;
use crate::tailer;

/// Calls idle this long with no new block are finalized on the tick.
const IDLE_FLUSH: Duration = Duration::from_secs(30);

pub struct Daemon {
    config: Config,
    collectors: Vec<Box<dyn Collector>>,
    writers: HashMap<String, OutputWriter>,
    state: State,
}

impl Daemon {
    pub fn new(config: Config) -> Result<Self> {
        let host = gethostname::gethostname().to_string_lossy().to_string();
        info!(host = %host, "starting token-use daemon");

        let collectors: Vec<Box<dyn Collector>> =
            vec![Box::new(ClaudeCodeCollector::new(&config.home, host))];

        let mut writers = HashMap::new();
        for c in &collectors {
            info!(service = c.name(), provider = c.provider(), "collector registered");
            writers.insert(
                c.name().to_string(),
                OutputWriter::new(config.out_dir.clone(), c.out_prefix()),
            );
        }

        let state = State::load(&config.state_dir).context("loading checkpoint state")?;

        Ok(Daemon {
            config,
            collectors,
            writers,
            state,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        self.backfill()?;
        self.watch_loop()
    }

    /// Process every existing transcript once, then persist the checkpoint.
    fn backfill(&mut self) -> Result<()> {
        info!("backfill: scanning existing transcripts");
        let mut files = 0usize;
        for ci in 0..self.collectors.len() {
            for path in self.collectors[ci].enumerate() {
                self.process_path(&path, true);
                files += 1;
            }
        }
        self.state.save().context("saving state after backfill")?;
        info!(files, "backfill complete");
        Ok(())
    }

    fn watch_loop(&mut self) -> Result<()> {
        let (tx, rx) = unbounded::<std::path::PathBuf>();

        // One debouncer per distinct watch root. Kept alive for the loop.
        let mut debouncers = Vec::new();
        let mut roots = Vec::new();
        for c in &self.collectors {
            roots.push(c.watch_root().to_path_buf());
        }
        roots.sort();
        roots.dedup();

        for root in &roots {
            if !root.exists() {
                warn!(root = %root.display(), "watch root does not exist yet; relying on tick rescan");
                continue;
            }
            let tx = tx.clone();
            let mut debouncer = new_debouncer(
                self.config.debounce,
                None,
                move |res: DebounceEventResult| {
                    if let Ok(events) = res {
                        for ev in events {
                            for p in ev.event.paths.iter() {
                                let _ = tx.send(p.clone());
                            }
                        }
                    }
                },
            )
            .context("creating filesystem debouncer")?;
            debouncer
                .watch(root, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", root.display()))?;
            info!(root = %root.display(), "watching");
            debouncers.push(debouncer);
        }

        let ticker = crossbeam_channel::tick(self.config.tick);
        info!(tick_secs = self.config.tick.as_secs(), "entering watch loop");

        loop {
            select! {
                recv(rx) -> msg => {
                    match msg {
                        Ok(path) => {
                            if self.owning_collector(&path).is_some() {
                                self.process_path(&path, false);
                            }
                        }
                        Err(_) => break, // all senders dropped
                    }
                }
                recv(ticker) -> _ => {
                    self.on_tick();
                }
            }
        }
        Ok(())
    }

    /// Periodic safety net: rescan all known files (catches events coalesced or
    /// missed by the platform watcher and discovers new files), finalize idle
    /// calls, and checkpoint state.
    fn on_tick(&mut self) {
        debug!("tick: rescanning transcripts");
        for ci in 0..self.collectors.len() {
            for path in self.collectors[ci].enumerate() {
                self.process_path(&path, false);
            }
        }
        // Finalize finished-but-unbounded calls.
        for ci in 0..self.collectors.len() {
            let recs = self.collectors[ci].flush_idle(IDLE_FLUSH);
            for r in recs {
                self.emit(r);
            }
        }
        if let Err(e) = self.state.save_if_dirty() {
            warn!(error = %e, "failed to persist state on tick");
        }
    }

    fn owning_collector(&self, path: &Path) -> Option<usize> {
        self.collectors.iter().position(|c| c.owns(path))
    }

    /// Tail one file from its checkpoint, feed new lines to the owning
    /// collector, persist the new offset, and emit any finalized records.
    fn process_path(&mut self, path: &Path, final_flush: bool) {
        let Some(ci) = self.owning_collector(path) else {
            return;
        };
        let offset = self.state.offset_for(path);

        // Borrow the collector only within this block so `self` is free to emit.
        let (records, new_offset) = {
            let collector = &mut self.collectors[ci];
            let mut records = Vec::new();
            let scan = tailer::scan_file(path, offset, |line| {
                match collector.consume_line(path, line) {
                    Ok(mut rs) => records.append(&mut rs),
                    Err(e) => warn!(file = %path.display(), error = %e, "skipping malformed line"),
                }
            });
            match scan {
                Ok(res) => {
                    if final_flush {
                        records.append(&mut collector.flush(path));
                    }
                    (records, Some(res.new_offset))
                }
                Err(e) => {
                    warn!(file = %path.display(), error = %e, "scan failed");
                    (records, None)
                }
            }
        };

        if let Some(off) = new_offset {
            let now = chrono::Utc::now()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            self.state.set_offset(path, off, Some(now));
        }
        for r in records {
            self.emit(r);
        }
    }

    /// Write a record, dropping cross-restart duplicates (calls only).
    fn emit(&mut self, rec: OutputRecord) {
        if let Some(key) = rec.dedup_key() {
            if self.state.seen_recent(key) {
                return;
            }
            self.state.mark_recent(key.to_string());
        }
        match self.writers.get_mut(rec.service_name()) {
            Some(w) => {
                if let Err(e) = w.write(&rec) {
                    warn!(service = rec.service_name(), error = %e, "failed to write record");
                }
            }
            None => warn!(service = rec.service_name(), "no writer for service"),
        }
    }
}
