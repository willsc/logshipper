use logshipper::{
    config::{Codec, Config, IoConfig, Source},
    daemon,
    destination::Destination,
    rate::{Limits, Rate},
    state::{Progress, State},
    transfer::{self, Fingerprint, Outcome, Receipt},
};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use walkdir::WalkDir;

struct Fixture {
    _tmp: TempDir,
    c: Config,
}
impl Fixture {
    fn new(codec: Codec) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let mount = tmp.path().join("mount");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&mount).unwrap();
        let mut c = Config {
            state_dir: tmp.path().join("state"),
            settle_seconds: 0,
            buffer_bytes: 4096,
            checkpoint_bytes: 16384,
            scan_interval_seconds: 1,
            sources: vec![Source {
                name: "app".into(),
                path: source,
                mode: logshipper::config::SourceMode::Archive,
                include: vec!["**/*".into()],
                exclude: vec![],
            }],
            io: IoConfig {
                read_bytes_per_second: 1024 * 1024 * 1024,
                write_bytes_per_second: 1024 * 1024 * 1024,
                operations_per_second: 1_000_000,
                scan_entries_per_second: 1_000_000,
            },
            ..Config::default()
        };
        c.destination.mount_path = mount;
        c.destination.namespace = "test-host".into();
        c.destination.require_mount = false;
        c.destination.min_free_bytes = 0;
        c.compression.format = codec;
        c.validate().unwrap();
        Self { _tmp: tmp, c }
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.c.sources[0].path.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }
    fn limits(&self) -> Limits {
        Limits::new(&self.c.io, Arc::new(AtomicBool::new(false)))
    }
    fn state(&self) -> State {
        State::open(&self.c.state_dir, "test").unwrap()
    }
    fn receipts(&self) -> Vec<PathBuf> {
        WalkDir::new(&self.c.destination.mount_path)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() == "receipt.json")
            .map(|e| e.into_path())
            .collect()
    }
    fn scan(&self) -> daemon::Summary {
        daemon::scan(
            &self.c,
            &Destination::open(&self.c.destination).unwrap(),
            &self.state(),
            &mut self.limits(),
        )
        .unwrap()
    }
    fn config_file(&self, slow: bool) -> PathBuf {
        let p = self._tmp.path().join("config.toml");
        let codec = match self.c.compression.format {
            Codec::None => "none",
            Codec::Gzip => "gzip",
            Codec::Zstd => "zstd",
        };
        fs::write(
            &p,
            format!(
                r#"
state_dir = {:?}
settle_seconds = 0
buffer_bytes = 4096
checkpoint_bytes = 16384
[destination]
mount_path = {:?}
namespace = "test-host"
require_mount = false
min_free_bytes = 0
[io]
read_bytes_per_second = {}
write_bytes_per_second = 1073741824
operations_per_second = 1000000
scan_entries_per_second = 1000000
[compression]
format = "{}"
[[sources]]
name = "app"
path = {:?}
"#,
                self.c.state_dir,
                self.c.destination.mount_path,
                if slow { 65536 } else { 1073741824 },
                codec,
                self.c.sources[0].path
            ),
        )
        .unwrap();
        p
    }
}
fn decoded(receipt_path: &Path) -> Vec<u8> {
    let receipt: Receipt = serde_json::from_slice(&fs::read(receipt_path).unwrap()).unwrap();
    let data = fs::read(
        receipt_path
            .parent()
            .unwrap()
            .join(receipt.compression.filename()),
    )
    .unwrap();
    match receipt.compression {
        Codec::None => data,
        Codec::Gzip => {
            let mut bytes = vec![];
            flate2::read::MultiGzDecoder::new(&data[..])
                .read_to_end(&mut bytes)
                .unwrap();
            bytes
        }
        Codec::Zstd => zstd::stream::decode_all(&data[..]).unwrap(),
    }
}

#[test]
fn arbitrary_bytes_round_trip_all_codecs_and_multiple_frames() {
    let data: Vec<_> = (0..100_000).map(|n| ((n * 31) % 256) as u8).collect();
    for codec in [Codec::None, Codec::Gzip, Codec::Zstd] {
        let f = Fixture::new(codec);
        f.write("nested/input.json", &data);
        assert_eq!(f.scan().shipped, 1);
        assert_eq!(f.scan().skipped, 1);
        let receipts = f.receipts();
        assert_eq!(receipts.len(), 1);
        assert_eq!(decoded(&receipts[0]), data);
        logshipper::integrity::verify(&receipts[0], &mut f.limits(), f.c.buffer_bytes).unwrap();
        assert_eq!(
            fs::read(f.c.sources[0].path.join("nested/input.json")).unwrap(),
            data
        );
    }
}
#[test]
fn empty_inputs_are_valid_archives() {
    for codec in [Codec::None, Codec::Gzip, Codec::Zstd] {
        let f = Fixture::new(codec);
        f.write("empty", b"");
        assert_eq!(f.scan().shipped, 1);
        assert!(decoded(&f.receipts()[0]).is_empty());
        logshipper::integrity::verify(&f.receipts()[0], &mut f.limits(), f.c.buffer_bytes).unwrap();
    }
}
#[test]
fn compressed_input_passes_through_by_magic_or_extension() {
    let f = Fixture::new(Codec::Zstd);
    let mut encoder = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
    encoder.write_all(b"existing gzip").unwrap();
    let bytes = encoder.finish().unwrap();
    f.write("no-extension", &bytes);
    f.write("opaque.br", b"opaque compressed stream");
    assert_eq!(f.scan().shipped, 2);
    for path in f.receipts() {
        let r: Receipt = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(r.compression, Codec::None);
        assert_eq!(
            decoded(&path),
            fs::read(f.c.sources[0].path.join(r.relative_path)).unwrap()
        );
    }
}
#[test]
fn recompression_can_be_requested_explicitly() {
    let mut f = Fixture::new(Codec::Gzip);
    f.c.compression.skip_compressed = false;
    f.write("already.gz", b"some bytes");
    f.scan();
    let r: Receipt = serde_json::from_slice(&fs::read(&f.receipts()[0]).unwrap()).unwrap();
    assert_eq!(r.compression, Codec::Gzip);
    assert_eq!(decoded(&f.receipts()[0]), b"some bytes");
}
#[test]
fn changing_file_creates_a_separate_version() {
    let f = Fixture::new(Codec::None);
    f.write("app.log", b"first");
    f.scan();
    f.write("app.log", b"second version");
    assert_eq!(f.scan().shipped, 1);
    let mut contents: Vec<_> = f.receipts().iter().map(|p| decoded(p)).collect();
    contents.sort();
    assert_eq!(
        contents,
        vec![b"first".to_vec(), b"second version".to_vec()]
    );
}
#[test]
fn settlement_filters_and_symlinks() {
    let mut f = Fixture::new(Codec::None);
    f.c.settle_seconds = 600;
    f.write("app.log", b"active");
    assert_eq!(f.scan().unsettled, 1);
    f.c.settle_seconds = 0;
    f.write("secret/no.log", b"excluded");
    f.write("other.txt", b"ignored");
    std::os::unix::fs::symlink(
        f.c.sources[0].path.join("app.log"),
        f.c.sources[0].path.join("link.log"),
    )
    .unwrap();
    f.c.sources[0].include = vec!["**/*.log".into()];
    f.c.sources[0].exclude = vec!["secret".into()];
    assert_eq!(f.scan().shipped, 1);
    assert_eq!(f.receipts().len(), 1);
}
#[test]
fn source_symlink_parent_is_rejected() {
    let f = Fixture::new(Codec::None);
    let p = f.write("real/log", b"test");
    std::os::unix::fs::symlink(
        f.c.sources[0].path.join("real"),
        f.c.sources[0].path.join("link"),
    )
    .unwrap();
    assert!(
        transfer::archive(
            &f.c,
            &f.c.sources[0],
            Path::new("link/log"),
            &Fingerprint::of(&fs::metadata(p).unwrap()),
            &Destination::open(&f.c.destination).unwrap(),
            &f.state(),
            &mut f.limits()
        )
        .is_err()
    );
}
#[test]
fn free_space_reserve_prevents_publication() {
    let mut f = Fixture::new(Codec::None);
    f.c.destination.min_free_bytes = u64::MAX;
    assert!(Destination::open(&f.c.destination).is_err());
    assert!(f.receipts().is_empty());
}
#[test]
fn locks_prevent_two_writers() {
    let f = Fixture::new(Codec::None);
    let _state = f.state();
    assert!(State::open(&f.c.state_dir, "test").is_err());
    let _destination = Destination::open(&f.c.destination).unwrap();
    assert!(Destination::open(&f.c.destination).is_err());
}
#[test]
fn checkpoint_offsets_support_more_than_four_gib() {
    let f = Fixture::new(Codec::None);
    let state = f.state();
    let offset = (1u64 << 32) + 500;
    state
        .save(
            "large",
            Progress {
                input: offset,
                output: offset,
                done: false,
            },
        )
        .unwrap();
    assert_eq!(state.get("large").unwrap().input, offset);
    let path = f.write("huge", b"");
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(offset)
        .unwrap();
    assert_eq!(Fingerprint::of(&fs::metadata(path).unwrap()).size, offset);
}
#[test]
fn state_cannot_be_reused_for_a_different_destination() {
    let f = Fixture::new(Codec::None);
    drop(f.state());
    assert!(State::open(&f.c.state_dir, "other").is_err());
}
#[test]
fn mount_absence_never_writes_to_local_mountpoint() {
    let mut f = Fixture::new(Codec::None);
    f.c.destination.require_mount = true;
    assert!(Destination::open(&f.c.destination).is_err());
    assert_eq!(
        fs::read_dir(&f.c.destination.mount_path).unwrap().count(),
        0
    );
}
#[test]
fn daemon_waits_for_missing_mount_at_startup_and_stops_cleanly() {
    let mut f = Fixture::new(Codec::None);
    f.c.destination.require_mount = true;
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let config = f.c.clone();
    let worker = thread::spawn(move || daemon::run(&config, false, worker_stop));
    thread::sleep(Duration::from_millis(150));
    assert!(!worker.is_finished());
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap().unwrap();
    assert_eq!(
        fs::read_dir(&f.c.destination.mount_path).unwrap().count(),
        0
    );
}
#[test]
fn daemon_reconnects_after_destination_appears() {
    let mut f = Fixture::new(Codec::None);
    f.write("app", b"eventually available");
    // Use the explicitly opted-in local-directory mode to simulate disappearance without mount privileges.
    f.c.destination.mount_path = f._tmp.path().join("not-yet-present");
    f.c.validate().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let config = f.c.clone();
    let worker = thread::spawn(move || daemon::run(&config, false, worker_stop));
    thread::sleep(Duration::from_millis(150));
    assert!(!worker.is_finished());
    fs::create_dir(&f.c.destination.mount_path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.receipts().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::Relaxed);
    worker.join().unwrap().unwrap();
    assert_eq!(decoded(&f.receipts()[0]), b"eventually available");
}
#[test]
fn configuration_rejects_overlap_and_unknown_keys() {
    let mut f = Fixture::new(Codec::None);
    f.c.state_dir = f.c.sources[0].path.join("state");
    assert!(f.c.validate().is_err());
    assert!(toml::from_str::<Config>("misspelled = true").is_err());
}
#[test]
fn rate_limiter_paces_first_request_and_cancels_promptly() {
    let stop = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    Rate::new(1000).acquire(100, &stop).unwrap();
    assert!(start.elapsed() >= Duration::from_millis(100));
    stop.store(true, Ordering::Relaxed);
    let start = Instant::now();
    assert!(Rate::new(1).acquire(1_000_000, &stop).is_err());
    assert!(start.elapsed() < Duration::from_millis(100));
}
#[test]
fn source_mutation_during_copy_is_never_published() {
    let mut f = Fixture::new(Codec::Zstd);
    f.c.io.read_bytes_per_second = 65536;
    let path = f.write("app", &vec![b'x'; 128 * 1024]);
    let expected = Fingerprint::of(&fs::metadata(&path).unwrap());
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"new data")
            .unwrap();
    });
    let result = transfer::archive(
        &f.c,
        &f.c.sources[0],
        Path::new("app"),
        &expected,
        &Destination::open(&f.c.destination).unwrap(),
        &f.state(),
        &mut f.limits(),
    );
    writer.join().unwrap();
    assert!(result.is_err());
    assert!(f.receipts().is_empty());
}
#[test]
fn killed_process_resumes_all_codecs_and_discards_uncommitted_tail() {
    for codec in [Codec::None, Codec::Gzip, Codec::Zstd] {
        let f = Fixture::new(codec);
        let bytes: Vec<_> = (0..256 * 1024).map(|n| (n % 251) as u8).collect();
        f.write("app", &bytes);
        let config_path = f.config_file(true);
        let mut child = Command::new(env!("CARGO_BIN_EXE_logshipper"))
            .args(["--once", "--config"])
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let saved = loop {
            let offset = rusqlite::Connection::open(f.c.state_dir.join("state.sqlite"))
                .ok()
                .and_then(|db| {
                    db.query_row("SELECT input FROM progress LIMIT 1", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .ok()
                })
                .unwrap_or(0);
            if offset > 0 {
                break offset;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("checkpoint not written");
            }
            thread::sleep(Duration::from_millis(10));
        };
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(saved < bytes.len() as i64);
        let partial = WalkDir::new(&f.c.destination.mount_path)
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| e.file_name() == "data.partial")
            .unwrap()
            .into_path();
        fs::OpenOptions::new()
            .append(true)
            .open(partial)
            .unwrap()
            .write_all(b"uncommitted corrupt tail")
            .unwrap();
        f.config_file(false);
        let result = Command::new(env!("CARGO_BIN_EXE_logshipper"))
            .args(["--once", "--json", "--config"])
            .arg(config_path)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let resumed = String::from_utf8_lossy(&result.stderr)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .any(|v| {
                v["fields"]["resumed_bytes"]
                    .as_u64()
                    .is_some_and(|n| n >= saved as u64)
            });
        assert!(resumed);
        assert_eq!(decoded(&f.receipts()[0]), bytes);
    }
}
#[test]
fn recovery_after_data_rename_does_not_recopy() {
    let f = Fixture::new(Codec::Gzip);
    let p = f.write("app", b"recover this");
    f.scan();
    let receipt = f.receipts()[0].clone();
    fs::remove_file(receipt).unwrap();
    let db = rusqlite::Connection::open(f.c.state_dir.join("state.sqlite")).unwrap();
    db.execute("UPDATE progress SET done=0", []).unwrap();
    drop(db);
    let result = transfer::archive(
        &f.c,
        &f.c.sources[0],
        Path::new("app"),
        &Fingerprint::of(&fs::metadata(p).unwrap()),
        &Destination::open(&f.c.destination).unwrap(),
        &f.state(),
        &mut f.limits(),
    )
    .unwrap();
    assert!(matches!(result, Outcome::Shipped { resumed: 12, .. }));
    assert_eq!(decoded(&f.receipts()[0]), b"recover this");
}
#[test]
fn completed_receipts_recover_lost_local_state_and_detect_truncation() {
    let mut f = Fixture::new(Codec::None);
    f.write("app", b"original bytes");
    f.scan();
    f.c.state_dir = f._tmp.path().join("new-state");
    assert_eq!(f.scan().skipped, 1);
    fs::write(f.receipts()[0].parent().unwrap().join("data"), b"short").unwrap();
    assert_eq!(f.scan().failed, 1);
    assert_eq!(
        fs::read(f.c.sources[0].path.join("app")).unwrap(),
        b"original bytes"
    );
}

#[test]
fn non_utf8_filenames_are_preserved_in_receipts() {
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new(Codec::None);
    let name = std::ffi::OsString::from_vec(b"log-\xff.json".to_vec());
    fs::write(f.c.sources[0].path.join(name), b"opaque bytes").unwrap();
    assert_eq!(f.scan().shipped, 1);
    let r: Receipt = serde_json::from_slice(&fs::read(&f.receipts()[0]).unwrap()).unwrap();
    assert_eq!(r.relative_path_hex, hex::encode(b"log-\xff.json"));
    assert_eq!(decoded(&f.receipts()[0]), b"opaque bytes");
}

#[test]
fn symlink_in_destination_cannot_redirect_archive_writes() {
    let f = Fixture::new(Codec::None);
    let outside = f._tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, f.c.destination.mount_path.join("logshipper")).unwrap();
    assert!(Destination::open(&f.c.destination).is_err());
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}

#[test]
fn changed_source_cannot_resume_a_stale_fingerprint() {
    let f = Fixture::new(Codec::None);
    let path = f.write("app", b"before");
    let expected = Fingerprint::of(&fs::metadata(&path).unwrap());
    fs::write(path, b"after and larger").unwrap();
    assert!(
        transfer::archive(
            &f.c,
            &f.c.sources[0],
            Path::new("app"),
            &expected,
            &Destination::open(&f.c.destination).unwrap(),
            &f.state(),
            &mut f.limits()
        )
        .is_err()
    );
    assert!(f.receipts().is_empty());
}

#[test]
fn excluded_files_still_obey_directory_scan_pacing() {
    let mut f = Fixture::new(Codec::None);
    f.c.io.scan_entries_per_second = 40;
    f.c.sources[0].exclude = vec!["**/*.tmp".into()];
    for i in 0..8 {
        f.write(&format!("{i}.tmp"), b"excluded");
    }
    let start = Instant::now();
    assert_eq!(f.scan().shipped, 0);
    assert!(start.elapsed() >= Duration::from_millis(200));
}

#[test]
fn full_verification_rejects_same_length_corruption() {
    for codec in [Codec::None, Codec::Gzip, Codec::Zstd] {
        let f = Fixture::new(codec);
        f.write("input", &vec![b'x'; 50000]);
        f.scan();
        let receipt = f.receipts()[0].clone();
        let data = receipt.parent().unwrap().join(codec.filename());
        let mut bytes = fs::read(&data).unwrap();
        let pos = bytes.len() / 2;
        bytes[pos] ^= 0xff;
        fs::write(data, bytes).unwrap();
        assert!(
            logshipper::integrity::verify(&receipt, &mut f.limits(), f.c.buffer_bytes).is_err()
        );
    }
}

#[test]
fn checksum_manifest_is_bound_to_receipt() {
    let f = Fixture::new(Codec::None);
    f.write("input", b"source");
    f.scan();
    let receipt = f.receipts()[0].clone();
    let path = receipt.parent().unwrap().join("checksums.jsonl");
    // Valid equivalent JSON with a different digest must be rejected too.
    let bytes = fs::read(&path).unwrap();
    let mut modified = b" ".to_vec();
    modified.extend(bytes);
    fs::write(path, modified).unwrap();
    assert!(logshipper::integrity::verify(&receipt, &mut f.limits(), f.c.buffer_bytes).is_err());
}

#[test]
fn monitoring_reports_outage_and_stall_without_blocking_worker() {
    use logshipper::monitoring::{Metrics, Server, now};
    let metrics = Arc::new(Metrics::default());
    let server = Server::start("127.0.0.1:0".parse().unwrap(), metrics.clone(), 5).unwrap();
    let get = |path: &str| {
        let mut client = std::net::TcpStream::connect(server.address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(client, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        reply
    };
    assert!(get("/healthz").contains("200 OK"));
    assert!(get("/readyz").contains("503 Service Unavailable"));
    metrics.available.store(true, Ordering::Relaxed);
    assert!(get("/readyz").contains("200 OK"));
    metrics.busy.store(true, Ordering::Relaxed);
    metrics.activity.store(now() - 10, Ordering::Relaxed);
    assert!(get("/readyz").contains("503 Service Unavailable"));
    assert!(get("/healthz").contains("200 OK"));
    metrics.shipped.store(42, Ordering::Relaxed);
    assert!(get("/metrics").contains("logshipper_files_shipped_total 42"));
    assert!(get("/unknown").contains("404 Not Found"));
}

#[test]
fn state_schema_migrates_and_completed_records_are_pruned() {
    let f = Fixture::new(Codec::None);
    fs::create_dir_all(&f.c.state_dir).unwrap();
    let path = f.c.state_dir.join("state.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE progress (id TEXT PRIMARY KEY,input INTEGER NOT NULL,output INTEGER NOT NULL,done INTEGER NOT NULL);
        INSERT INTO progress VALUES ('old',42,42,1),('pending',10,10,0);").unwrap();
    drop(db);
    let state = f.state();
    assert_eq!(state.get("old").unwrap().input, 42);
    assert_eq!(state.prune(30).unwrap(), 1);
    assert!(!state.get("old").unwrap().done);
    assert_eq!(state.get("pending").unwrap().input, 10);
    assert_eq!(state.pending().unwrap(), 1);
}

#[test]
fn newer_state_schema_is_not_modified() {
    let f = Fixture::new(Codec::None);
    fs::create_dir_all(&f.c.state_dir).unwrap();
    let db = rusqlite::Connection::open(f.c.state_dir.join("state.sqlite")).unwrap();
    db.execute_batch("PRAGMA user_version=99").unwrap();
    drop(db);
    assert!(State::open(&f.c.state_dir, "test").is_err());
}

#[test]
fn local_state_free_space_guard_defers_work() {
    let mut f = Fixture::new(Codec::None);
    f.c.min_state_free_bytes = u64::MAX;
    f.write("file", b"not copied");
    assert!(
        daemon::scan(
            &f.c,
            &Destination::open(&f.c.destination).unwrap(),
            &f.state(),
            &mut f.limits()
        )
        .is_err()
    );
    assert!(f.receipts().is_empty());
}

#[test]
fn corrupted_durable_prefix_is_replaced_from_source() {
    let f = Fixture::new(Codec::None);
    let bytes = vec![b'x'; 50000];
    let path = f.write("input", &bytes);
    f.scan();
    let receipt = f.receipts()[0].clone();
    let data = receipt.parent().unwrap().join("data");
    fs::write(&data, vec![b'y'; bytes.len()]).unwrap();
    fs::remove_file(receipt).unwrap();
    let db = rusqlite::Connection::open(f.c.state_dir.join("state.sqlite")).unwrap();
    db.execute("UPDATE progress SET done=0", []).unwrap();
    drop(db);
    let mut limits = f.limits();
    let result = transfer::archive(
        &f.c,
        &f.c.sources[0],
        Path::new("input"),
        &Fingerprint::of(&fs::metadata(path).unwrap()),
        &Destination::open(&f.c.destination).unwrap(),
        &f.state(),
        &mut limits,
    )
    .unwrap();
    assert!(matches!(result, Outcome::Shipped { resumed: 0, .. }));
    assert_eq!(limits.metrics.integrity_failures.load(Ordering::Relaxed), 1);
    assert_eq!(decoded(&f.receipts()[0]), bytes);
    logshipper::integrity::verify(&f.receipts()[0], &mut f.limits(), f.c.buffer_bytes).unwrap();
}

fn make_tail(f: &mut Fixture) {
    f.c.sources[0].mode = logshipper::config::SourceMode::Tail;
    f.c.tail.segment_bytes = 4096;
    f.c.tail.flush_seconds = 0;
    f.c.tail.mem_buf_limit = 4096;
    f.c.settle_seconds = 3600;
}

fn tail_streams(f: &Fixture) -> std::collections::BTreeMap<(String, u64), Vec<u8>> {
    let mut segments: Vec<_> = f
        .receipts()
        .into_iter()
        .map(|p| {
            let receipt: Receipt = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
            logshipper::integrity::verify(&p, &mut f.limits(), f.c.buffer_bytes).unwrap();
            (receipt.stream.unwrap(), decoded(&p))
        })
        .collect();
    segments.sort_by_key(|(s, _)| (s.stream_id.clone(), s.generation, s.offset));
    let mut streams = std::collections::BTreeMap::<(String, u64), Vec<u8>>::new();
    for (segment, bytes) in segments {
        let output = streams
            .entry((segment.stream_id, segment.generation))
            .or_default();
        assert_eq!(
            segment.offset,
            output.len() as u64,
            "duplicate or missing tail segment"
        );
        output.extend(bytes);
    }
    streams
}

#[test]
fn live_tail_bypasses_settlement_and_preserves_partial_lines_all_codecs() {
    let bytes = b"{\"message\":\"long JSON line without a terminating newline\"}".repeat(211);
    for codec in [Codec::None, Codec::Gzip, Codec::Zstd] {
        let mut f = Fixture::new(codec);
        make_tail(&mut f);
        f.write("live.json", &bytes);
        for _ in 0..4 {
            f.scan();
        }
        assert_eq!(f.scan().tail_shipped, 0);
        assert_eq!(
            tail_streams(&f).into_values().collect::<Vec<_>>(),
            vec![bytes.clone()]
        );
    }
}

#[test]
fn tail_follows_appends_and_rename_rotation_across_restarts() {
    let mut f = Fixture::new(Codec::Zstd);
    make_tail(&mut f);
    let path = f.write("app.log", b"first\n");
    f.scan();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"second\n")
        .unwrap();
    f.scan();
    fs::rename(path, f.c.sources[0].path.join("app.log.1")).unwrap();
    f.write("app.log", b"new inode\n");
    f.scan();
    f.scan();
    let mut streams: Vec<_> = tail_streams(&f).into_values().collect();
    streams.sort();
    assert_eq!(
        streams,
        vec![b"first\nsecond\n".to_vec(), b"new inode\n".to_vec()]
    );
}

#[test]
fn stalled_tail_upload_has_bounded_memory_and_never_blocks_application_writes() {
    let mut f = Fixture::new(Codec::None);
    make_tail(&mut f);
    f.c.io.write_bytes_per_second = 1024; // A full segment cannot finish for at least four seconds.
    let path = f.write("live.log", &vec![b'x'; 65536]);
    let stop = Arc::new(AtomicBool::new(false));
    let mut limits = Limits::new(&f.c.io, stop.clone());
    let metrics = limits.metrics.clone();
    let config = f.c.clone();
    let worker = thread::spawn(move || {
        let destination = Destination::open(&config.destination).unwrap();
        let state = State::open(&config.state_dir, "test").unwrap();
        logshipper::tail::ship(
            &config,
            &config.sources[0],
            Path::new("live.log"),
            &destination,
            &state,
            &mut limits,
        )
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while metrics.read_bytes.load(Ordering::Relaxed) < 4096 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let start = Instant::now();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"application is independent\n")
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(!worker.is_finished());
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        metrics.read_bytes.load(Ordering::Relaxed),
        4096,
        "backlog read-ahead must stop at the unacknowledged segment"
    );
    assert_eq!(metrics.buffered_bytes.load(Ordering::Relaxed), 4096);
    assert_eq!(metrics.peak_buffered_bytes.load(Ordering::Relaxed), 4096);
    stop.store(true, Ordering::Relaxed);
    assert!(worker.join().unwrap().is_err());
    assert_eq!(metrics.buffered_bytes.load(Ordering::Relaxed), 0);
    assert!(f.receipts().is_empty());
    let db = rusqlite::Connection::open(f.c.state_dir.join("state.sqlite")).unwrap();
    let saved: String = db
        .query_row("SELECT state_json FROM tail_streams", [], |r| r.get(0))
        .unwrap();
    let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["offset"], 0);
    assert_eq!(saved["pending"]["segment"]["bytes"], 4096);
    drop(db);
    f.c.io.write_bytes_per_second = 1024 * 1024 * 1024;
    for _ in 0..17 {
        f.scan();
    }
    assert_eq!(
        tail_streams(&f).into_values().next().unwrap(),
        fs::read(path).unwrap()
    );
}

#[test]
fn tail_recovers_publication_before_cursor_ack_without_overlap() {
    use sha2::{Digest, Sha256};
    let mut f = Fixture::new(Codec::None);
    make_tail(&mut f);
    let path = f.write("live", b"first");
    f.scan();
    let receipt: Receipt = serde_json::from_slice(&fs::read(&f.receipts()[0]).unwrap()).unwrap();
    let pending = serde_json::json!({"segment": receipt.stream, "fingerprint": receipt.fingerprint,
        "codec": "none", "level": 3, "anchor_bytes": 5, "anchor_sha256": hex::encode(Sha256::digest(b"first"))});
    let saved = serde_json::json!({"generation": 0, "offset": 0, "anchor_bytes": 0, "anchor_sha256": "", "pending": pending});
    let db = rusqlite::Connection::open(f.c.state_dir.join("state.sqlite")).unwrap();
    db.execute(
        "UPDATE tail_streams SET state_json=?1,pending=1",
        [saved.to_string()],
    )
    .unwrap();
    drop(db);
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"second")
        .unwrap();
    f.scan();
    assert_eq!(f.receipts().len(), 1);
    f.scan();
    assert_eq!(
        tail_streams(&f).into_values().next().unwrap(),
        b"firstsecond"
    );
}

#[test]
fn tail_truncation_starts_a_distinct_generation() {
    let mut f = Fixture::new(Codec::None);
    make_tail(&mut f);
    let path = f.write("live", b"before truncation");
    f.scan();
    fs::write(path, b"after").unwrap();
    f.scan();
    let streams = tail_streams(&f);
    assert_eq!(streams.len(), 2);
    assert_eq!(
        streams.values().cloned().collect::<Vec<_>>(),
        vec![b"before truncation".to_vec(), b"after".to_vec()]
    );
}
