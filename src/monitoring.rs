//! A separate, bounded HTTP thread stays responsive while the worker is blocked in NFS.
use anyhow::{Context, Result};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
#[derive(Default)]
pub struct Metrics {
    pub available: AtomicBool,
    pub busy: AtomicBool,
    pub activity: AtomicU64,
    pub shipped: AtomicU64,
    pub skipped: AtomicU64,
    pub errors: AtomicU64,
    pub retries: AtomicU64,
    pub read_bytes: AtomicU64,
    pub written_bytes: AtomicU64,
    pub checkpoints: AtomicU64,
    pub last_success: AtomicU64,
    pub last_scan: AtomicU64,
    pub pending: AtomicU64,
    pub integrity_failures: AtomicU64,
    pub buffered_bytes: AtomicU64,
    pub peak_buffered_bytes: AtomicU64,
    pub tail_resets: AtomicU64,
}
impl Metrics {
    pub fn touch(&self) {
        self.activity.store(now(), Ordering::Relaxed);
    }
    pub fn ready(&self, stall_seconds: u64) -> bool {
        self.available.load(Ordering::Relaxed)
            && (!self.busy.load(Ordering::Relaxed)
                || now().saturating_sub(self.activity.load(Ordering::Relaxed)) < stall_seconds)
    }
    pub fn render(&self, stall: u64) -> String {
        let mut out = String::from(
            "# HELP logshipper_ready Worker can access the destination and is not stalled.\n# TYPE logshipper_ready gauge\n",
        );
        out.push_str(&format!(
            "logshipper_ready {}\n",
            u8::from(self.ready(stall))
        ));
        for (name, help, counter, value) in [
            (
                "tail_buffered_bytes",
                "Current unacknowledged tail payload in memory.",
                false,
                &self.buffered_bytes,
            ),
            (
                "tail_peak_buffered_bytes",
                "Peak tail payload memory since process startup.",
                false,
                &self.peak_buffered_bytes,
            ),
            (
                "tail_resets_total",
                "Source truncations or overwrites detected by tail cursors.",
                true,
                &self.tail_resets,
            ),
            (
                "files_shipped_total",
                "Completed archives.",
                true,
                &self.shipped,
            ),
            (
                "files_skipped_total",
                "Previously completed archives seen during scans.",
                true,
                &self.skipped,
            ),
            (
                "errors_total",
                "Transfer, scan, or destination errors.",
                true,
                &self.errors,
            ),
            (
                "destination_retries_total",
                "Destination reopen retries.",
                true,
                &self.retries,
            ),
            (
                "data_read_bytes_total",
                "Data bytes read including integrity verification.",
                true,
                &self.read_bytes,
            ),
            (
                "data_written_bytes_total",
                "Archive data bytes written including retries.",
                true,
                &self.written_bytes,
            ),
            (
                "checkpoints_total",
                "Durable transfer checkpoints.",
                true,
                &self.checkpoints,
            ),
            (
                "integrity_failures_total",
                "Detected integrity failures.",
                true,
                &self.integrity_failures,
            ),
            (
                "last_success_timestamp_seconds",
                "Unix time of latest archive completion.",
                false,
                &self.last_success,
            ),
            (
                "last_scan_timestamp_seconds",
                "Unix time of latest completed scan.",
                false,
                &self.last_scan,
            ),
            (
                "last_activity_timestamp_seconds",
                "Unix time of latest worker progress.",
                false,
                &self.activity,
            ),
            (
                "pending_transfers",
                "Incomplete transfers in local state; not the full source backlog.",
                false,
                &self.pending,
            ),
        ] {
            out.push_str(&format!("# HELP logshipper_{name} {help}\n# TYPE logshipper_{name} {}\nlogshipper_{name} {}\n", if counter { "counter" } else { "gauge" }, value.load(Ordering::Relaxed)));
        }
        out
    }
}

pub struct Server {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    pub address: SocketAddr,
}
impl Server {
    pub fn start(address: SocketAddr, metrics: Arc<Metrics>, stall: u64) -> Result<Self> {
        let listener = TcpListener::bind(address).context("bind monitoring listener")?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("logshipper-metrics".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = respond(stream, &metrics, stall);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(25))
                        }
                        Err(e) => {
                            tracing::error!(error=%e, "monitoring accept failed");
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
            address,
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn respond(mut stream: TcpStream, metrics: &Metrics, stall: u64) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    stream.set_write_timeout(Some(Duration::from_millis(200)))?;
    let mut request = [0u8; 4096];
    let mut used = 0;
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    while used < request.len() && std::time::Instant::now() < deadline {
        let n = stream.read(&mut request[used..])?;
        if n == 0 {
            break;
        }
        used += n;
        if request[..used].windows(4).any(|s| s == b"\r\n\r\n") {
            break;
        }
    }
    let first = std::str::from_utf8(&request[..used])
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next();
    let path = parts.next();
    let (code, body) = match (method, path) {
        (Some("GET"), Some("/metrics")) => ("200 OK", metrics.render(stall)),
        (Some("GET"), Some("/healthz")) => ("200 OK", "alive\n".into()),
        (Some("GET"), Some("/readyz")) if metrics.ready(stall) => ("200 OK", "ready\n".into()),
        (Some("GET"), Some("/readyz")) => (
            "503 Service Unavailable",
            "waiting, degraded, or stalled\n".into(),
        ),
        (Some("GET"), _) => ("404 Not Found", "not found\n".into()),
        _ => ("405 Method Not Allowed", "GET required\n".into()),
    };
    write!(
        stream,
        "HTTP/1.1 {code}\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
