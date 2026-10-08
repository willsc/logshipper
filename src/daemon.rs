use crate::{
    config::{Config, SourceMode},
    destination::Destination,
    monitoring::{self, Server},
    rate::{Limits, Stop, sleep},
    state::State,
    transfer::{self, Fingerprint, Outcome},
};
use anyhow::{Context, Result, ensure};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use walkdir::WalkDir;

#[derive(Debug, Default)]
pub struct Summary {
    pub shipped: u64,
    pub skipped: u64,
    pub unsettled: u64,
    pub failed: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub tail_shipped: u64,
}
fn patterns(patterns: &[String]) -> Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for pattern in patterns {
        set.add(Glob::new(pattern)?);
    }
    Ok(set.build()?)
}

pub fn scan(
    c: &Config,
    destination: &Destination,
    state: &State,
    limits: &mut Limits,
) -> Result<Summary> {
    state.check_space(c.min_state_free_bytes)?;
    destination.check(0)?;
    limits.metrics.available.store(true, Ordering::Relaxed);
    limits.metrics.busy.store(true, Ordering::Relaxed);
    limits.metrics.touch();
    let mut result = Summary::default();
    let mut health_check = Instant::now();
    for source in c
        .sources
        .iter()
        .filter(|s| s.mode == SourceMode::Tail)
        .chain(c.sources.iter().filter(|s| s.mode == SourceMode::Archive))
    {
        let include = patterns(&source.include)?;
        let exclude = patterns(&source.exclude)?;
        let mut walker = WalkDir::new(&source.path)
            .follow_links(false)
            .follow_root_links(false)
            .max_open(16)
            .into_iter();
        loop {
            // Pace even excluded entries: filtering inside the iterator could scan
            // an entire excluded directory's files without ever reaching this limit.
            limits.scan.acquire(1, &limits.stop)?;
            limits.metrics.touch();
            // Stop walking the backlog promptly when FSx disappears.
            if health_check.elapsed() >= Duration::from_secs(1) {
                destination.check(0)?;
                health_check = Instant::now();
            }
            let Some(entry) = walker.next() else {
                break;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot scan source entry");
                    result.failed += 1;
                    limits.metrics.errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            let relative = entry.path().strip_prefix(&source.path)?;
            if entry.depth() > 0 && exclude.is_match(relative) {
                if entry.file_type().is_dir() {
                    walker.skip_current_dir();
                }
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            if !include.is_match(relative) {
                continue;
            }
            let mut attempt = || -> Result<Option<Outcome>> {
                if source.mode == SourceMode::Tail {
                    return Ok(Some(crate::tail::ship(
                        c,
                        source,
                        relative,
                        destination,
                        state,
                        limits,
                    )?));
                }
                let fingerprint = Fingerprint::of(&entry.metadata()?);
                if !fingerprint.settled(c.settle_seconds) {
                    return Ok(None);
                }
                Ok(Some(transfer::archive(
                    c,
                    source,
                    relative,
                    &fingerprint,
                    destination,
                    state,
                    limits,
                )?))
            };
            match attempt() {
                Ok(None) => result.unsettled += 1,
                Ok(Some(Outcome::Skipped)) => {
                    result.skipped += 1;
                    limits.metrics.skipped.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Some(Outcome::Shipped {
                    input,
                    output,
                    resumed,
                })) => {
                    result.shipped += 1;
                    if source.mode == SourceMode::Tail {
                        result.tail_shipped += 1;
                    }
                    result.input_bytes += input;
                    result.output_bytes += output;
                    limits.metrics.shipped.fetch_add(1, Ordering::Relaxed);
                    limits
                        .metrics
                        .last_success
                        .store(monitoring::now(), Ordering::Relaxed);
                    tracing::info!(path = %entry.path().display(), input_bytes = input, output_bytes = output, resumed_bytes = resumed, "archive complete");
                }
                Err(e) => {
                    if limits.stop.load(Ordering::Relaxed) {
                        return Err(e);
                    }
                    tracing::warn!(path = %entry.path().display(), error = %format!("{e:#}"), "transfer deferred; will retry");
                    result.failed += 1;
                    limits.metrics.errors.fetch_add(1, Ordering::Relaxed);
                    state.check_space(c.min_state_free_bytes)?;
                    // A missing/replaced mount or low space defers the whole scan.
                    destination.check(
                        c.checkpoint_bytes
                            .saturating_add(c.checkpoint_bytes / 8)
                            .saturating_add(1024 * 1024),
                    )?;
                }
            }
        }
    }
    limits
        .metrics
        .last_scan
        .store(monitoring::now(), Ordering::Relaxed);
    limits
        .metrics
        .available
        .store(result.failed == 0, Ordering::Relaxed);
    limits.metrics.busy.store(false, Ordering::Relaxed);
    limits
        .metrics
        .pending
        .store(state.pending()?, Ordering::Relaxed);
    tracing::info!(
        shipped = result.shipped,
        skipped = result.skipped,
        unsettled = result.unsettled,
        failed = result.failed,
        input_bytes = result.input_bytes,
        output_bytes = result.output_bytes,
        "scan complete"
    );
    Ok(result)
}

pub fn run(c: &Config, once: bool, stop: Stop) -> Result<()> {
    let binding = serde_json::to_string(&(
        &c.destination.mount_path,
        &c.destination.directory,
        &c.destination.namespace,
    ))?;
    let state = State::open(&c.state_dir, &binding)?;
    let mut limits = Limits::new(&c.io, stop.clone());
    let _server = c
        .monitoring
        .listen
        .map(|address| Server::start(address, limits.metrics.clone(), c.monitoring.stall_seconds))
        .transpose()?;
    if let Some(server) = &_server {
        tracing::info!(address=%server.address, "monitoring listener started");
    }
    let mut backoff = 1u64;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Reopen after each scan/failure: an old descriptor must not pin recovery to a stale mount.
        // Opening the mount is deliberately inside the retry loop, including initial startup.
        limits.metrics.busy.store(true, Ordering::Relaxed);
        limits.metrics.touch();
        let attempt = state
            .check_space(c.min_state_free_bytes)
            .and_then(|_| Destination::open(&c.destination))
            .and_then(|destination| scan(c, &destination, &state, &mut limits));
        limits.metrics.busy.store(false, Ordering::Relaxed);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if once {
            let result = attempt.context("single scan failed")?;
            ensure!(
                result.failed == 0,
                "{} source entries failed",
                result.failed
            );
            return Ok(());
        }
        let delay = match attempt {
            Ok(summary) => {
                let pruned = state.prune(c.state_retention_days)?;
                if pruned > 0 {
                    tracing::info!(pruned, "expired completed local state records");
                }
                backoff = 1;
                if summary.tail_shipped > 0 {
                    0
                } else if c.sources.iter().any(|s| s.mode == SourceMode::Tail) {
                    c.tail.poll_seconds
                } else {
                    c.scan_interval_seconds
                }
            }
            Err(e) => {
                limits.metrics.available.store(false, Ordering::Relaxed);
                limits.metrics.errors.fetch_add(1, Ordering::Relaxed);
                limits.metrics.retries.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %format!("{e:#}"), retry_seconds = backoff,
                    "archive unavailable or scan failed; waiting to retry");
                let delay = backoff;
                backoff = (backoff * 2).min(60);
                delay
            }
        };
        // Small jitter spreads retries across a fleet after a shared NFS outage.
        let jitter = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64
            + std::process::id() as u64)
            % 401;
        let millis = if backoff > 1 {
            (delay * (800 + jitter)).min(60_000)
        } else {
            delay * 1000
        };
        if sleep(Duration::from_millis(millis), &stop).is_err() {
            break;
        }
    }
    tracing::info!("shutdown complete; interrupted transfers resume from durable checkpoints");
    Ok(())
}
