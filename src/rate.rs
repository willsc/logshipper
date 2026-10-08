use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub type Stop = Arc<AtomicBool>;
pub fn check_stop(stop: &Stop) -> io::Result<()> {
    if stop.load(Ordering::Relaxed) {
        Err(io::Error::other("shutdown requested"))
    } else {
        Ok(())
    }
}
pub fn sleep(duration: Duration, stop: &Stop) -> io::Result<()> {
    let end = Instant::now() + duration;
    loop {
        check_stop(stop)?;
        let now = Instant::now();
        if now >= end {
            return Ok(());
        }
        thread::sleep((end - now).min(Duration::from_millis(50)));
    }
}

/// No accumulated idle credit: pay before every operation, including the first.
pub struct Rate {
    per_second: u64,
    ready: Instant,
}
impl Rate {
    pub fn new(per_second: u64) -> Self {
        Self {
            per_second,
            ready: Instant::now(),
        }
    }
    pub fn acquire(&mut self, units: u64, stop: &Stop) -> io::Result<()> {
        let now = Instant::now();
        let start = self.ready.max(now);
        self.ready = start + Duration::from_secs_f64(units as f64 / self.per_second as f64);
        sleep(self.ready.saturating_duration_since(now), stop)
    }
}

pub struct Limits {
    pub read: Rate,
    pub write: Rate,
    pub ops: Rate,
    pub scan: Rate,
    pub stop: Stop,
    pub metrics: Arc<crate::monitoring::Metrics>,
}
impl Limits {
    pub fn new(c: &crate::config::IoConfig, stop: Stop) -> Self {
        Self {
            read: Rate::new(c.read_bytes_per_second),
            write: Rate::new(c.write_bytes_per_second),
            ops: Rate::new(c.operations_per_second),
            scan: Rate::new(c.scan_entries_per_second),
            stop,
            metrics: Arc::new(crate::monitoring::Metrics::default()),
        }
    }
    pub fn reading(&mut self, bytes: usize) -> io::Result<()> {
        self.metrics.touch();
        self.read.acquire(bytes as u64, &self.stop)?;
        self.ops.acquire(1, &self.stop)
    }
    pub fn writing(&mut self, bytes: usize) -> io::Result<()> {
        self.metrics.touch();
        self.write.acquire(bytes as u64, &self.stop)?;
        self.ops.acquire(1, &self.stop)
    }
}
