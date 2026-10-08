use anyhow::{Context, Result};
use clap::Parser;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    version,
    about = "Ship local logs to mounted FSx with bounded I/O and backpressure"
)]
struct Args {
    #[arg(short, long, default_value = "/etc/logshipper/config.toml")]
    config: PathBuf,
    /// Process one scan; return nonzero on transfer or mount failure.
    #[arg(long)]
    once: bool,
    /// Validate configuration without opening the destination or modifying state.
    #[arg(long)]
    check_config: bool,
    /// Emit structured JSON logs (otherwise human-readable logs).
    #[arg(long)]
    json: bool,
    /// Verify a completed v2 archive, including decoded data and per-chunk SHA-256.
    #[arg(long, conflicts_with_all = ["once", "check_config"])]
    verify: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    if args.json {
        subscriber.json().init();
    } else {
        subscriber.init();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = stop.clone();
    ctrlc::set_handler(move || {
        signal_stop.store(true, Ordering::Relaxed);
    })
    .context("install signal handler")?;
    if let Some(receipt) = args.verify {
        let defaults = logshipper::config::Config::default();
        let mut limits = logshipper::rate::Limits::new(&defaults.io, stop);
        let verified = logshipper::integrity::verify(&receipt, &mut limits, defaults.buffer_bytes)?;
        println!(
            "Verified {}: {} source bytes, {} stored bytes, {} chunks",
            verified.id, verified.fingerprint.size, verified.stored_bytes, verified.chunks
        );
        return Ok(());
    }
    let config = logshipper::config::Config::load(&args.config)?;
    if args.check_config {
        println!("Configuration is valid");
        return Ok(());
    }
    logshipper::daemon::run(&config, args.once, stop)
}
