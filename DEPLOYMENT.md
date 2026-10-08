# Deployment and operations guide

This guide deploys logshipper on a **Linux/systemd host using FSx for NetApp ONTAP or FSx for OpenZFS over NFS**. It covers one writer per host namespace, retained local source logs, and a local SQLite state directory. See [README.md](README.md) for the archive format and configuration reference.

The repository includes production hardening and automated fault tests. A deployment is ready for service only after the live-NFS acceptance checks below pass on the actual filesystem, mount options, client kernel, and application workload. The local mount test is not an AWS failover certification.

## Architecture requirement: the application never waits for FSx

Keep the application's log destination on local disk. Do not point it at `/mnt/fsx`, pipe its logging through the shipper, or wait for shipper acknowledgements. Logshipper alone performs NFS operations. A slow or unavailable FSx share can stall its worker while the application continues to append locally.

The local logs themselves are the durable queue. There is no additional data spool. One bounded in-memory segment is read, delivered, and durably acknowledged before another is consumed. This makes backlog reads follow destination progress. Retention and capacity must cover the outage: local disk exhaustion can still affect the application even though FSx is outside its write path.

## 1. Deployment inputs and prerequisites

Record these values before rollout:

| Input | Example | Requirement |
| --- | --- | --- |
| FSx service | ONTAP or OpenZFS | Existing, reachable NFS volume/export |
| NFS source | `svm.example:/archive-volume` | Exact endpoint/export supplied by storage administration |
| Mountpoint | `/mnt/fsx` | Exact host mountpoint, not a directory beneath it |
| Archive namespace | `prod-app-01` | Stable and unique for this writer |
| Source roots | `/var/log/myapp` | Local files only; live + retained rotated patterns for tail mode |
| State | `/var/lib/logshipper` | Persistent local disk, outside source/destination trees |
| Source retention | outage + backlog + margin | No external deletion before successful archiving |
| Resource budgets | initially 20 MiB/s read, 10 MiB/s write | Measured against foreground application needs |
| Numeric service UID/GID | chosen by your identity policy | Mapped to archive permissions on the NFS server |

Requirements:

- Linux with `/proc`, systemd, and a working NFS client. Use the architecture of the supplied binary.
- Routing, DNS, security groups, network ACLs, and export policy that allow the client to reach the selected FSx endpoint. NFSv4 normally uses TCP 2049; use the service's complete network requirements when configuring AWS.
- A writable private archive namespace with server-coordinated locking and working file/directory `fsync` and rename.
- Local capacity for retained logs, the SQLite database/WAL, and system logs. No local copy of the archive data is spooled.
- A monitoring/alerting path and an owner for NFS outages, storage capacity, and retention.

The daemon needs no AWS IAM credentials. It relies on NFS filesystem permissions. Do not disable root squash to make installation convenient; have storage administration provision access for the service's numeric identity.

## 2. Build and produce a release

Build on a compatible target distribution or an older supported glibc baseline. For example, install the native build dependencies on an Ubuntu build host:

```sh
sudo apt-get update
sudo apt-get install build-essential pkg-config python3
rustc --version
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all -- --check
bash scripts/test-mount-recovery.sh
bash scripts/build-release.sh
```

Install Rust through your approved toolchain distribution process if it is absent. Rust 1.89+ is required. On Amazon Linux 2023, the equivalent native prerequisites are GCC, GCC C++, make, pkgconf/pkg-config, and Python 3. The release builder uses Python only on the build host; the daemon itself is a native executable.

`scripts/test-mount-recovery.sh` requires permitted user/mount namespaces and loopback sockets. It creates a private tmpfs mount and does not touch the host's NFS mounts. If your hardened build runner disables namespaces, run it on an isolated Linux test host; do not omit it from release qualification.

The builder creates a native architecture-specific `.tar.gz` and its `.sha256` file under `dist/`. Each bundle contains internal checksums and `build-info.json` recording compiler, libc, architecture, binary digest, and lockfile digest. Archive the build/test output and a current `cargo audit --file Cargo.lock` report with the release. CI also audits the locked dependencies. Checksums are not a release signature: sign or distribute through your organisation's trusted artifact repository.

## 3. Install the NFS client and mount FSx

On Ubuntu:

```sh
sudo apt-get install nfs-common acl
sudo install -d -m 0755 -o root -g root /mnt/fsx
```

On Amazon Linux 2023:

```sh
sudo dnf install nfs-utils acl
sudo install -d -m 0755 -o root -g root /mnt/fsx
```

Use the actual endpoint and export path. This NFSv4.1 example is a starting point for a reviewed `/etc/fstab` entry, not a command containing real infrastructure values:

```fstab
FSX_DNS_NAME:/EXPORT_PATH /mnt/fsx nfs vers=4.1,hard,timeo=600,retrans=2,rsize=1048576,wsize=1048576,noresvport,_netdev,nofail 0 0
```

Then mount this entry and inspect what the kernel actually negotiated:

```sh
sudo mount /mnt/fsx
findmnt --mountpoint /mnt/fsx -o TARGET,SOURCE,FSTYPE,OPTIONS
nfsstat -m
```

The example follows AWS's [FSx EC2 mount guidance](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/storage_fsx.html). OpenZFS may negotiate smaller read/write request sizes on lower-throughput configurations; see its [mounting reference](https://docs.aws.amazon.com/fsx/latest/OpenZFSGuide/mounting-volumes.html). Select NFS version and throughput options against your actual service. `timeo=600` is a retry timeout in deciseconds, not a total operation deadline.

Use `hard` recovery and server-coordinated file locking. The daemon refuses `soft`, `softerr`, `softreval`, `nolock`, `local_lock=all`, and `local_lock=flock`. Keep the bare local mountpoint owned by root and unwritable by the service account. Do not put the service behind `ConditionPathIsMountPoint` or `RequiresMountsFor`: it must start and wait while FSx is unavailable.

The daemon does not retry the mount command itself. Your mount unit, configuration management, or operations procedure must restore a missing mount. A failed `nofail` boot mount does not by itself schedule recurring mount attempts. Avoid relying on an autofs-only mountpoint: logshipper checks for an actual NFS mount before touching the archive.

## 4. Install the release without starting it

Copy the bundle and its externally trusted checksum/signature to the host. Substitute the actual release name:

```sh
sha256sum --check logshipper-0.2.0-linux-x86_64.tar.gz.sha256
tar -xzf logshipper-0.2.0-linux-x86_64.tar.gz
cd logshipper-0.2.0-linux-x86_64
sudo bash install.sh
```

The installer checks bundle contents, creates the service account if needed, installs the binary/unit/docs, and preserves an existing configuration. It never enables or restarts the service. If numeric UID/GID values are centrally assigned, provision the account before running it. For an upgrade, first follow section 10.

Installed paths:

| Path | Purpose |
| --- | --- |
| `/usr/local/bin/logshipper` | Executable |
| `/etc/logshipper/config.toml` | Root-owned, service-readable configuration |
| `/var/lib/logshipper` | Private persistent state, mode 0700 |
| `/etc/systemd/system/logshipper.service` | Unit |
| `/usr/local/share/doc/logshipper/` | README, this guide, build info, alert example |

## 5. Configure identities, permissions, and sources

Inspect the account and tell storage administration its numeric UID/GID:

```sh
id logshipper
```

Have the export administrator create `<export>/logshipper/<namespace>` with appropriate ownership and mode 0700 (or a reviewed shared-group mode). Intermediate directories must be readable/traversable by the service. Do this on the actual NFS export; changing ownership of the bare local mountpoint does not grant remote access. ONTAP security style, export policy, root squash, and OpenZFS POSIX ownership must match the chosen identity.

Give logshipper read/traverse access only to the selected source trees. A group/ACL example for an existing Ubuntu application log tree is:

```sh
sudo setfacl -R -m u:logshipper:rX /var/log/myapp
sudo find /var/log/myapp -type d -exec setfacl -m d:u:logshipper:rX '{}' +
sudo -u logshipper test -r /var/log/myapp/application.log.1
sudo -u logshipper test -w /mnt/fsx/logshipper/prod-app-01
```

Review this against your ACL policy and test files newly created by log rotation; restrictive creation modes can mask inherited permissions. The service does not need source write permission. Filesystem permissions must also let it traverse source parents.

Edit `/etc/logshipper/config.toml`:

- Set the namespace to the stable host/writer identity. Two hosts must never intentionally share it, even during recovery.
- Set `mount_path`, and set `expected_source` to the exact `SOURCE` printed by `findmnt -n -o SOURCE --mountpoint /mnt/fsx`.
- Keep `require_mount = true` and `allowed_filesystems = ["nfs", "nfs4"]`.
- For live shipping, set `mode = "tail"` and include active **and retained rotated** filenames. The example includes `*.log`, `*.json`, `*.jsonl` and their rotated forms. Use rename-and-create rotation; avoid copy-truncate. For independent closed-file archives, use `mode = "archive"` and exclude active names. Validate patterns against the actual application naming convention.
- Set retention/space reserves and budgets. Keep `state_dir` on local persistent storage.
- Keep monitoring on loopback unless an explicitly secured management network is used.

Validate as the service identity:

```sh
sudo -u logshipper /usr/local/bin/logshipper --config /etc/logshipper/config.toml --check-config
```

For live mode, review this section of the example configuration:

```toml
[tail]
poll_seconds = 1
flush_seconds = 5
mem_buf_limit = 8388608
segment_bytes = 1048576
```

`mem_buf_limit` is the bound on unacknowledged source payload, not the entire Rust process RSS. Codec buffers, SQLite, traversal, and monitoring use additional bounded memory. `segment_bytes` must fit the limit. A blocked NFS write holds that segment and stops further source consumption. Small appends are batched up to `flush_seconds`; full segments can send sooner. Polling/scanning/throttling add latency. Increasing flush time reduces the number of small archive objects and metadata operations on FSx. Each segment creates data, a manifest, and a receipt; size the namespace/retention policy for that object count.

## 6. Establish resource and retention budgets

Start with the example's 20 MiB/s read, 10 MiB/s actual write, 100 combined data operations/s, and 100 scanned entries/s. They are ceilings, not promised throughput. Live reads often use the page cache, while old backlog may need actual disk I/O; neither cache residency nor zero application impact is guaranteed. SHA-256 adds CPU cost; resuming rereads the durable archive prefix to validate it. Higher Zstandard levels need more memory and CPU. For archive mode, a 64 MiB checkpoint limits normal uncommitted recopy to that chunk, but loss of local state requires recopying an incomplete file from the beginning. Tail mode retries at most one pending segment, and its committed offset advances only after remote receipt publication. Lost tail state replays retained files from the beginning and can create overlapping ranges, so include the entire state database in backups.

The unit sets low CPU/I/O priority, a 50% CPU quota, `MemoryHigh=256M`, `MemoryMax=512M`, `TasksMax=16`, and `LimitNOFILE=1024`. Measure these values on representative logs. High compression levels may need a larger memory limit; memory-limit termination is recoverable but prevents forward progress if repeated.

For a hard source/state block-device budget, inspect the backing device and use a service drop-in. Replace the device below with your actual one:

```ini
# sudo systemctl edit logshipper
[Service]
IOReadBandwidthMax=/dev/nvme1n1 20M
IOReadIOPSMax=/dev/nvme1n1 100
IOWriteBandwidthMax=/dev/nvme1n1 5M
IOWriteIOPSMax=/dev/nvme1n1 100
```

These controls require the cgroup I/O controller and appropriate device support. Check the service cgroup's `io.max` after applying them. They govern local block I/O; the daemon's output limit governs NFS writes. See the Linux [cgroup v2 I/O documentation](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html). Measure application latency, `iostat -xz`, NFS retransmissions, and disk pressure while tuning. No automatic pressure-based throttling is implemented.

Size source retention for the maximum NFS outage **plus catch-up time plus operational margin**. Archival throughput must exceed the long-term log production rate. For illustration, 2 MiB/s of source logs over a six-hour outage accumulates about 42 GiB before allowing for ongoing writes/catch-up. Compression helps remote capacity, not source retention requirements. Set remote retention through your storage policy; deleting a still-referenced completed archive makes subsequent scans report damage.

## 7. Canary, start, and monitor

Use a dedicated test source and namespace for the first run, containing known text, JSON, binary, empty, and compressed samples. Do not run a second `--once` instance against the live daemon's state/namespace.

```sh
sudo -u logshipper /usr/local/bin/logshipper --config /etc/logshipper/config.toml --once --json
# Select an actual receipt produced by this run:
sudo -u logshipper /usr/local/bin/logshipper --verify /mnt/fsx/logshipper/HOST/SOURCE/PREFIX/ID/receipt.json
sudo systemctl enable --now logshipper
systemctl status logshipper
journalctl -u logshipper -f
curl --fail http://127.0.0.1:9898/healthz
curl --fail http://127.0.0.1:9898/readyz
curl --fail http://127.0.0.1:9898/metrics
```

Scrape through a local Prometheus agent or a reviewed authenticated proxy. The built-in listener has no TLS/authentication; loopback is the example default. A local scrape example:

```yaml
scrape_configs:
  - job_name: logshipper
    scrape_interval: 30s
    static_configs:
      - targets: ["127.0.0.1:9898"]
```

Load [packaging/alerts.yml](packaging/alerts.yml) into your monitoring deployment. Alert on exporter unavailability, sustained unready status, errors, and any integrity failure. Also alert on source/state/remote free space and oldest unarchived log age: `pending_transfers` is not the total backlog. An idle source legitimately produces no archive-success events, so a universal “no recent success” alert can be misleading.

`/healthz` proves that the monitoring thread responds. `/readyz` reflects destination/work status and busy-worker progress. A hard-NFS syscall blocked longer than `stall_seconds` causes 503 while liveness can remain 200. A healthy process during an outage is expected; investigate the mount/client before restarting it.

## 8. Acceptance tests on the actual FSx host

Run on a staging export or a dedicated canary host. Record the release digest, distribution/kernel, NFS version/options, FSx deployment type, settings, fixture checksums, and results.

1. **Data and large files:** archive representative binary, JSON, text, precompressed, and multi-GiB files. Run `--verify`, restore them, and independently compare checksums with the immutable source. Confirm bounded RSS and foreground application latency under realistic load.
2. **Idempotency and live offsets:** repeat scans; completed archives and fully consumed tail files should be skipped. Append to a live file and confirm new segments start at the previous acknowledged offset. Restore a complete stream with `restore-stream.py` and compare its bytes with the source.
3. **Process interruption:** terminate and separately kill the canary during transfer. Restart with the same local state and namespace. Confirm resumed offsets, successful verification, and no receipt for incomplete output.
4. **Missing mount at startup:** use a dedicated canary mountpoint; start the daemon while it is unmounted. Confirm the process stays alive, readiness is 503, and no archive directory appears locally. Restore the mount and confirm progress without restarting the daemon.
5. **Mid-transfer unmount/remount:** detach only the dedicated canary mount, restore the same export, and verify resume. Open descriptors can make a normal unmount busy. Review any lazy-unmount procedure with the host owner; never unmount a shared production volume for this test.
6. **NFS network/server outage and application isolation:** keep a separate application process writing a local canary log while using your staging fault-injection or FSx failover procedure. Confirm the writer continues completing local writes, tail buffered bytes never exceed the configured bound, and source-read counters stop growing while delivery is stalled. Confirm monitoring remains responsive, stalled readiness alerts fire, and completion/checksums recover after the NFS client recovers. Examine kernel logs for lost locks, stale handles, and retransmissions.
7. **Wrong destination/options:** point a canary mount at another export and confirm `expected_source` prevents writes. Confirm unsafe NFS options are rejected. Restore the approved mount.
8. **Permissions and space:** deny access on a test source/object, or raise the configured free-space reserve above available capacity. Confirm errors/alerts and recovery after correction. Do not fill the shared filesystem to test ENOSPC.
9. **Source mutation/rotation:** for tail mode, append and rename/rotate while running; confirm contiguous offsets and correct new-inode handling. Truncation should start a distinct generation and alert. For archive mode, changing a source during copying must defer that version. Validate that rotation retains unread bytes through the outage.
10. **Operational recovery:** test config restart, completed-state pruning, state backup/restore, upgrade, and an archive restore independently of the source host.

ONTAP failover can produce transient errors, extended pauses, and NFSv4 lock reclaim failures; AWS documents [client failover considerations](https://docs.aws.amazon.com/fsx/latest/ONTAPGuide/nfs-failover-issues.html). Evaluate any suggested network/lease tuning in staging with storage/network administrators. This package does not change host sysctls or FSx settings.

## 9. Routine operations and troubleshooting

```sh
sudo systemctl restart logshipper       # Apply a reviewed config change
sudo systemctl stop logshipper
journalctl -u logshipper --since '1 hour ago'
findmnt --mountpoint /mnt/fsx
nfsstat -m
df -h /var/lib/logshipper /var/log/myapp /mnt/fsx
journalctl -k --since '1 hour ago'
```

| Symptom | Action |
| --- | --- |
| Required filesystem not mounted | Restore the actual mount; the daemon will retry |
| Mounted export does not match | Compare `expected_source` with `findmnt`; verify export ownership before changing either |
| Unsafe NFS option | Correct the reviewed mount definition and remount the canary/client appropriately |
| Namespace/state locked | Find the intended writer; do not delete `.lock` to bypass it |
| Access denied | Check numeric UID/GID, parent traversal, export policy, root squash, ACLs, and newly rotated files |
| Free-space reserve reached | Expand/free storage or review reserve policy; inspect source retention as backlog grows |
| Completed archive missing/damaged | Restore from FSx backup or investigate; the daemon does not silently overwrite completed evidence |
| Incomplete checksum mismatch | The daemon restarts from source and increments the integrity-failure counter; investigate storage before trusting recurrence |
| Ready=0, health=200, no progress | Inspect NFS/client/kernel state and quota/space; a hard-NFS syscall may be blocked |
| Source changed during whole-file archive | Use tail mode for active files; retain immutable files for archive mode |
| Tail reset or vanished pending range | Investigate copy-truncate, premature rotation deletion, or state loss; missing source bytes cannot be recovered by the daemon |
| Repeated OOM/service restart | Check compression level and workload memory; adjust service limits after measurement |

Use `RUST_LOG=debug` through a systemd environment drop-in for checkpoint diagnostics, then remove it if the extra logging is unnecessary. The service's 90-second stop grace allows normal cancellation; a kernel-blocked NFS task may remain until the client recovers, even after a kill request. Avoid starting competing writers or moving its state while the old process still exists.

Tail cursors are retained to prevent accidental replay; do not delete their state while the corresponding source files remain relevant. Completed whole-file local records older than `state_retention_days` are pruned in batches; their remote receipts allow rediscovery. Keep the local state directory private and back it up while the service is stopped (or use a SQLite-consistent backup procedure). Do not copy only `state.sqlite` while ignoring a live WAL. SQLite frees pages for reuse; if a deliberate database compaction is needed, schedule it while stopped and budget the extra I/O/free space.

Incomplete state/partial objects are not automatically deleted. Periodically inventory pending objects and investigate sources that disappeared or repeatedly changed. Before removing an abandoned object, stop its writer, retain its local state/diagnostic information, establish that the source/version is no longer needed or is backed up, and use your storage retention procedure. Do not bulk-delete all objects lacking receipts during normal operation. Completed archive retention and FSx backup/snapshot policy are separate from local state retention.

## 10. Upgrade and rollback

1. Qualify the new bundle on a canary with the acceptance tests. Keep the old executable, configuration, build info, and state backup together.
2. Stop the service and verify its process has exited. Take a consistent backup of the entire `/var/lib/logshipper` directory and `/etc/logshipper/config.toml` to your secured backup location.
3. Run the new bundle's `install.sh`. It retains `/usr/local/bin/logshipper.previous` and preserves configuration; it does not restart the service.
4. Review new configuration defaults and migration notes, then run `--check-config` as the service user. Start the service and watch readiness, logs, integrity metrics, and a canary archive verification.
5. If rolling back, stop and confirm exit again. Restore a compatible executable **and its matching state/configuration backup**, then restart. Keep newer archives intact; do not delete them merely to roll back software.

Version 0.2 migrates local state to schema 2 and refuses a newer unknown schema. It uses v2 archive identities and receipts with checksum manifests. Version 0.1 archives remain readable with their original compression tools but lack v2 full-integrity verification. Retained source files will be archived again as v2 on first eligibility, so budget remote space and rollout I/O. A v0.1 rollback must use its pre-upgrade state backup rather than attempting to interpret schema 2.

For configuration-only changes, archive the prior TOML, validate the replacement, and restart. Changing destination mount/directory/namespace requires a separate state directory; state binding intentionally prevents accidental reuse. Changing compression changes whole-file archive identity and may rearchive retained files. Tail mode keeps acknowledged offsets; a pending segment retains its saved codec/level and later segments use the new setting. Do not lower `tail.mem_buf_limit` below an existing pending segment until it has been delivered.

## Deployment completion record

Retain a record of release/build digests; dependency audit/test results; NFS export and mount options; identity/permission checks; capacity and I/O budgets; alert routing; canary/outage/restore results; and the rollback backup location. Sign off production rollout only after these refer to the actual deployed host and FSx service.
