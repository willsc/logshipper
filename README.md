# logshipper

A Rust daemon that tails live local logs or archives settled log files to **Amazon FSx for NetApp ONTAP or OpenZFS mounted over NFS on the same Linux host**. It treats files as bytes, so text, JSON, JSON Lines, binary logs, and existing compressed files use the same pipeline.

**Start with [DEPLOYMENT.md](DEPLOYMENT.md)** for host installation, NFS setup, permissions, monitoring, acceptance tests, and rollback. The daemon does not provision AWS resources or mount NFS itself.

## Application isolation and backpressure

The application writes **only to local files**. The daemon is a separate process that reads those files and writes to FSx. There is no application-to-daemon pipe, acknowledgement, or FSx call in the application's write path. An FSx stall blocks the daemon, not the application.

The existing local logs are the durable buffer. Live mode holds at most one unacknowledged segment in memory, capped by `tail.mem_buf_limit`. It reads the next segment only after the previous one has a durable FSx receipt and an acknowledged local cursor. When FSx is slow or absent, consumption stops and unread bytes remain in the local logs. This provides backpressure without copying logs into another local disk spool.

Prompt live reads will often hit the OS page cache. That is not guaranteed: older backlog or memory pressure can cause physical disk reads. The same shared I/O limits apply during live work and catch-up. A full source disk or contention for shared CPU/disk can still affect the application, so retention, capacity alerts, and resource budgets remain necessary. FSx throughput/latency is separate from local disk capacity.

## Capabilities

- Live tailing with bounded payload memory, persistent byte cursors, append/rename-rotation handling, and destination-driven consumption.
- Fixed-size streaming buffers and 64-bit offsets for large files.
- Optional Zstandard or gzip compression; recognized compressed inputs can pass through unchanged.
- Shared source/archive read bandwidth, output bandwidth, data-operation, and directory-scan limits.
- Durable SQLite checkpoints on local storage; compressed transfers resume at complete frame/member boundaries.
- SHA-256 checksums of source and stored bytes for each chunk, with a checksummed manifest and atomic completion receipt.
- Validation of every durable output chunk before resuming. Damaged incomplete output restarts from the retained source.
- Missing-mount startup, outage retries with capped backoff/jitter, export identity checks, and protection against local fallback writes.
- An independent HTTP monitoring thread with Prometheus metrics, liveness, readiness, and stalled-worker detection.
- Graceful SIGINT/SIGTERM, a restricted systemd service, release packaging, installation tooling, and CI checks.

Source files are retained. The daemon neither deletes nor truncates them.

## Quick start

Build on Linux with Rust 1.89+ and a C compiler/native build tools. The validated development compiler is recorded in each release's `build-info.json`. SQLite and Zstandard are compiled from bundled sources. FSx access uses the mounted filesystem and the service account's permissions; AWS API credentials are unnecessary.

```sh
cargo build --release --locked
cp config.example.toml config.toml
# Edit paths, namespace, export identity, and log rotation patterns.
./target/release/logshipper --config config.toml --check-config
./target/release/logshipper --config config.toml --once --json
./target/release/logshipper --config config.toml --json
```

`--check-config` checks settings and source directories without writing state or requiring FSx to be mounted. `--once` runs one scan and returns nonzero for mount/transfer failures; tail sources deliver at most one eligible segment per file in that scan. Unsettled/excluded files are skipped. Normal mode remains running and retries. Configuration changes require a restart.

For a local trial, configure separate source, state, and destination directories, set `require_mount = false`, and set `min_free_bytes = 0`. Keep mount validation enabled in production.

## Configuration

See the commented [config.example.toml](config.example.toml). Unknown keys and overlapping source/state/destination trees are rejected. Each source has a unique name, a directory path, and include/exclude globs relative to that directory. Excluded directories are pruned; excluded files still count toward scan pacing. Symlinks and nonregular files are skipped.

| Setting | Default | Meaning |
| --- | --- | --- |
| `settle_seconds` | 120 | Archive mode: minimum age since modification/inode change |
| `tail.poll_seconds` | 1 | Idle polling interval for live sources |
| `tail.flush_seconds` | 5 | Small-append batching interval; full segments send sooner |
| `tail.mem_buf_limit` | 8388608 | Maximum unacknowledged source payload in memory |
| `tail.segment_bytes` | 1048576 | One in-flight segment, at most `mem_buf_limit` |
| `scan_interval_seconds` | 30 | Delay after a completed scan |
| `buffer_bytes` | 262144 | Transfer buffer and maximum data syscall size |
| `checkpoint_bytes` | 67108864 | Source bytes between durable checkpoints |
| `io.read_bytes_per_second` | 20971520 | Shared source reads and archive integrity reads |
| `io.write_bytes_per_second` | 10485760 | Actual stored data bytes after compression |
| `io.operations_per_second` | 100 | Combined data read/write calls and checkpoint file syncs |
| `io.scan_entries_per_second` | 100 | Directory traversal entries |
| `destination.min_free_bytes` | 1073741824 | Remote reserve, plus allowance for the next chunk |
| `min_state_free_bytes` | 67108864 | Local state filesystem reserve |
| `state_retention_days` | 30 | Age for pruning completed local state records |
| `monitoring.listen` | disabled | Example config enables `127.0.0.1:9898` |
| `monitoring.stall_seconds` | 300 | Busy worker without activity becomes unready |

All I/O rates must be positive. Compression is `none`, `gzip` (levels 0–9), or `zstd` (levels 1–19). `skip_compressed = true` recognizes common filename extensions and magic bytes, preserving that input as opaque bytes. Change source patterns to fit your rotation convention; the production example selects live and rotated files in `mode = "tail"`. Use `mode = "archive"` for immutable whole files; omitted mode retains archive behavior for compatibility.

The worker is sequential and accumulates no idle burst credit. Read/write pacing, operation limits, compression, and sync time add together, so actual throughput may be lower than either bandwidth limit. Memory depends on buffers, compression level, traversal, and SQLite cache rather than file size. There is no queue containing the entire source tree.

Application limits do not equal physical device IOPS: kernel readahead/writeback, directory metadata, receipt writes, and SQLite also consume I/O. Apply cgroup limits to the source/state block devices when a hard disk budget is required. See the deployment guide. Tail mode services at most one bounded segment per file in each pass and repeats immediately while making progress. Archive mode copies a whole file and can delay tail latency when both modes share an instance. Use a tail-only instance for predictable live service; budget combined I/O if deploying multiple processes.

## Source modes and consistency

In **tail mode**, select active and retained rotated files. New files start at byte zero. Polling defaults to one second, with up to five seconds of batching for small appends. Actual latency also includes directory-scan time, I/O limits, compression, and FSx service time; no fixed latency is promised under overload. Segments preserve exact bytes, including partial lines, long JSON records, and compressed/binary input. They are byte ranges, not parsed log events.

A cursor follows device/inode identity across a rename within the selected tree. Rename-and-create rotation should retain the old file until its unread bytes are delivered. Truncation or a changed cursor anchor starts a new generation and increments `tail_resets_total`. Copy-truncate/rewrite and rapid inode reuse cannot be made lossless by polling; avoid them. Unread bytes deleted or overwritten externally cannot be recovered.

A pending segment's exact range/hash is saved locally before remote publication. The cursor advances only after the data, integrity manifest, and receipt are durable. A crash after remote commit recovers the existing receipt without widening the pending range. Partial tail segments are rewritten from retained source bytes, bounded by the segment size. Loss of tail state causes replay from byte zero and may produce overlapping segments; restore rejects overlaps rather than silently dropping data. Keep the state backup.

In **archive mode**, use immutable, rotated files. A quiet period cannot prove a writer has closed a file. The daemon checks device, inode, size, modification time, and inode-change time before copying, after each checkpoint, and before publication. An observed change leaves that version incomplete; the next eligible version gets a different ID. Intermittently active files may create multiple complete versions. These checks are not transactional application snapshots.

Retain local logs long enough for the maximum outage, backlog, and transfer time in either mode. The daemon does not ask the application to wait and does not manage its log rotation or retention.

## Outages and NFS behavior

The daemon starts when FSx is missing and retries with approximately 1, 2, 4, 8, 16, 32, then at most 60-second delays with jitter. It reopens the destination after a failed/completed scan. `mount_path` must be the exact mountpoint. Set `expected_source` from `findmnt` to refuse an unexpected NFS export mounted at that path.

Mount identity is checked through `/proc/self/mountinfo` and the opened descriptor's mount ID. Pinned directory descriptors prevent an unmount race from redirecting writes into local storage. NFS mounts with `soft`, `softerr`, `softreval`, `nolock`, or local-only flock options are rejected. A host namespace must have exactly one intended writer; filesystem and local state locks enforce normal concurrent-process exclusion.

A hard-mounted but unreachable NFS server can block a kernel syscall indefinitely. The monitoring thread stays independent: `/healthz` reports process liveness and `/readyz` becomes unavailable after the configured stall interval. Application retry intervals are **not** syscall deadlines. Test actual FSx failover and lock reclaim behavior on your client kernel; do not rely on restarting a process to fix a kernel-blocked NFS request.

## Archive format and integrity

```text
<mount>/<directory>/<namespace>/<source>/<id-prefix>/<id>/
    data                 # unchanged input, including already-compressed bytes
    data.gz              # OR gzip output
    data.zst             # OR Zstandard output
    checksums.jsonl      # ordered chunk offsets, lengths, source/stored SHA-256
    receipt.json         # completion marker, totals, checksum-manifest SHA-256
```

Only one data filename is used. Consumers must require `receipt.json`; partial files and unreceipted data are incomplete. Receipts retain relative filenames and their exact raw bytes in hex for non-UTF-8 names. The object ID hashes identity, metadata, and compression settings; it is not itself a content hash.

For whole-file archives, each checkpoint syncs output and its directory before atomically committing offsets and chunk checksums to local SQLite. On resume, persisted output hashes are checked and the uncommitted tail is removed. Publication syncs data and the checksum manifest, then atomically installs and syncs the receipt. Crashes between publication stages recover using local state. Completed receipts can be rediscovered after loss/pruning of local state; incomplete transfers then need their retained sources and restart from zero.

Verify an archive without the original source or daemon configuration:

```sh
logshipper --verify /mnt/fsx/logshipper/HOST/SOURCE/PREFIX/ID/receipt.json
```

Verification streams all stored chunks, decodes generated compression, checks source and stored SHA-256, and validates the manifest digest/totals. It uses the conservative default read/operation limits. Ordinary scans check completed object presence/length; schedule explicit verification to detect same-length corruption. Checksums detect accidental damage; they are not signatures against an attacker able to rewrite both data and receipts.

To restore a **whole-file archive** after successful verification:

```sh
# Select the data filename identified by the receipt's compression field.
gzip -dc /path/to/object/data.gz > /safe/restore/application.log
zstd -dc /path/to/object/data.zst > /safe/restore/application.log
cp /path/to/object/data /safe/restore/original-name
```

Gzip output uses concatenated members; Zstandard uses concatenated frames. Use decoders that consume them all. Pass-through data retains its original encoding and should be restored with the filename from the receipt. Ownership, ACLs, permissions, and extended attributes are not reconstructed.

Tail receipts additionally contain a `stream` object with `stream_id`, generation, byte offset/length, original filename, and payload digest. Restore a live stream by selecting a single ID/generation, checking every segment, and concatenating contiguous offsets:

```sh
python3 scripts/restore-stream.py /mnt/fsx/logshipper/HOST/SOURCE \
  --stream STREAM_ID --generation 0 --output /safe/restore/application.log
```

The release includes this helper as `restore-stream.py`; installation places it in `/usr/local/share/doc/logshipper/`. It requires Python 3, the `logshipper` verifier, and `zstd` for Zstandard segments. It refuses gaps, overlaps, corrupt segments, and existing output paths. Output is published only after verification. It can prove continuity through the last available receipt, not that an active source has finished or has no undelivered suffix. Stop writers or record/compare an expected final size for a complete restore. Run restore/scrubbing as budgeted maintenance: the helper verifies and then rereads data for reconstruction.

## Monitoring and maintenance

The example enables these unauthenticated, loopback-only endpoints:

- `GET /healthz`: 200 while the monitoring thread responds.
- `GET /readyz`: 503 for unavailable/degraded/stalled work, otherwise 200.
- `GET /metrics`: Prometheus counters and gauges for files, data bytes, errors, retries, checkpoints, integrity failures, activity, successful scans/transfers, known pending transfers, tail payload memory/high-water mark, and source reset counts.

`pending_transfers` counts incomplete SQLite entries, not all files waiting in source directories. Counters reset on restart. JSON logs include failures, retry delays, byte counts, and resumed offsets. `RUST_LOG=debug` adds checkpoint messages. [Example alerts](packaging/alerts.yml) are included; monitor source disk capacity/backlog age separately.

At most 1,000 expired completed local records are pruned per successful daemon scan; archive receipts remain authoritative. SQLite reuses freed pages rather than automatically shrinking its file. Tail cursors, incomplete state, and abandoned partial objects are retained; they are not covered by completed-record pruning. Remote archive retention, automatic partial deletion, and source deletion are deliberately operator-managed; the deployment guide includes maintenance procedures.

## Release and validation

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
bash scripts/test-mount-recovery.sh
bash scripts/build-release.sh
```

The mount test needs user/mount namespace support and loopback networking; it uses a private tmpfs mount, never your FSx mount. It covers missing-mount startup, mid-transfer detach, no local fallback, remount, resume, and full checksum verification. Integration tests also exercise process killing, all codecs, corruption, mutation, locking, state migration/pruning, free space, and monitoring. CI runs tests and a RustSec dependency audit.

Release bundles in `dist/` contain the native Linux binary, configuration, service, alerts, documentation, installer, build information, and SHA-256 checksums. Build on the oldest supported target userspace or on the deployment distribution: a binary built against a newer glibc may not run on an older host. Bundle checksums verify transfer integrity; distribute/sign releases through your trusted artifact channel.

A prior local test of the streaming engine processed a synthetic 5 GiB + 17-byte file using 10,524 KiB peak RSS; [its record](validation/large-file-local.json) is from version 0.1.0 and is not a v0.2/NFS performance benchmark. The deployment guide defines the remaining live-NFS acceptance tests. No FSx volume has been provisioned or tested in this workspace.

**Upgrade from 0.1.0:** v0.2 uses new archive IDs and integrity receipts. Existing v0.1 archives remain untouched, but retained source files are archived once more in v2 format. SQLite migrates to schema 2. Back up state while stopped and follow the rollback procedure before upgrading.
