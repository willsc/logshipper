//! Live tailing with pull-based backpressure. The application's local log is the durable queue.
//! Only one bounded segment can be unacknowledged; no application code or extra disk spool is involved.
use crate::{
    config::{Codec, Config, Source},
    destination::{Destination, fd_path},
    integrity::{self, Chunk},
    monitoring::{self, Metrics},
    rate::{Limits, check_stop},
    state::State,
    transfer::{self, Fingerprint, Outcome, Receipt},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Seek, SeekFrom, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::Path,
    sync::{Arc, atomic::Ordering},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Segment {
    pub id: String,
    pub stream_id: String,
    pub generation: u64,
    pub offset: u64,
    pub bytes: u64,
    pub source_root: String,
    pub relative_path: String,
    pub relative_path_hex: String,
    pub payload_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    segment: Segment,
    fingerprint: Fingerprint,
    codec: Codec,
    level: i32,
    anchor_bytes: usize,
    anchor_sha256: String,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct TailCursor {
    generation: u64,
    offset: u64,
    anchor_bytes: usize,
    anchor_sha256: String,
    pending: Option<Pending>,
    #[serde(default)]
    last_fingerprint: Option<Fingerprint>,
    #[serde(default)]
    last_ack: u64,
}
impl TailCursor {
    fn save(&self, state: &State, key: &str) -> Result<()> {
        state.tail_save(key, &serde_json::to_string(self)?, self.pending.is_some())
    }
    fn acknowledge(&mut self, state: &State, key: &str, pending: &Pending) -> Result<()> {
        self.offset = pending
            .segment
            .offset
            .checked_add(pending.segment.bytes)
            .context("tail offset overflow")?;
        self.anchor_bytes = pending.anchor_bytes;
        self.anchor_sha256 = pending.anchor_sha256.clone();
        self.pending = None;
        self.last_fingerprint = Some(pending.fingerprint.clone());
        self.last_ack = monitoring::now();
        self.save(state, key)
    }
    fn reset(&mut self, state: &State, key: &str, limits: &Limits) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .context("tail generation overflow")?;
        self.offset = 0;
        self.anchor_bytes = 0;
        self.anchor_sha256.clear();
        self.pending = None;
        self.last_fingerprint = None;
        self.last_ack = 0;
        limits.metrics.tail_resets.fetch_add(1, Ordering::Relaxed);
        self.save(state, key)
    }
}
struct MemoryGuard(Arc<Metrics>);
impl Drop for MemoryGuard {
    fn drop(&mut self) {
        self.0.buffered_bytes.store(0, Ordering::Relaxed);
    }
}
fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn read_bytes(
    file: &mut File,
    offset: u64,
    bytes: usize,
    c: &Config,
    limits: &mut Limits,
) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut data = vec![0u8; bytes];
    let mut read = 0;
    while read < bytes {
        let amount = (bytes - read).min(c.buffer_bytes);
        limits.reading(amount)?;
        let n = file.read(&mut data[read..read + amount])?;
        ensure!(n > 0, "tail source truncated while reading");
        limits
            .metrics
            .read_bytes
            .fetch_add(n as u64, Ordering::Relaxed);
        read += n;
    }
    Ok(data)
}
fn atomic_metadata(dir: &File, name: &str, data: &[u8]) -> Result<()> {
    let tmp = fd_path(dir).join(format!("{name}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&tmp)?;
    ensure!(
        file.metadata()?.is_file(),
        "metadata target is not a regular file"
    );
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(tmp, fd_path(dir).join(name))?;
    dir.sync_all()?;
    Ok(())
}

pub fn ship(
    c: &Config,
    source: &Source,
    relative: &Path,
    destination: &Destination,
    state: &State,
    limits: &mut Limits,
) -> Result<Outcome> {
    check_stop(&limits.stop)?;
    state.check_space(c.min_state_free_bytes)?;
    destination.check(0)?;
    let mut input = transfer::open_source(source, relative)?;
    let fingerprint = Fingerprint::of(&input.metadata()?);
    let key = hash(&serde_json::to_vec(&(
        "tail-v1",
        &source.name,
        hex::encode(source.path.as_os_str().as_bytes()),
        fingerprint.device,
        fingerprint.inode,
    ))?);
    let mut cursor: TailCursor = state
        .tail_get(&key)?
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default();

    // Recover a crash after remote commit but before cursor acknowledgement, without rereading the source.
    if let Some(pending) = cursor.pending.clone() {
        let object = destination.object(&source.name, &pending.segment.id)?;
        let path = fd_path(&object).join("receipt.json");
        if let Some(receipt) = transfer::read_receipt(&path)? {
            ensure!(
                receipt.stream.as_ref() == Some(&pending.segment),
                "tail completion receipt does not match pending range"
            );
            integrity::verify(&path, limits, c.buffer_bytes)?;
            object.sync_all()?;
            cursor.acknowledge(state, &key, &pending)?;
            return Ok(Outcome::Shipped {
                input: pending.segment.bytes,
                output: receipt.stored_bytes,
                resumed: pending.segment.bytes,
            });
        }
    }

    if cursor.pending.is_none() {
        if fingerprint.size == cursor.offset
            && cursor.last_fingerprint.as_ref() == Some(&fingerprint)
        {
            return Ok(Outcome::Skipped);
        }
        let mut replaced = fingerprint.size < cursor.offset;
        if !replaced && cursor.anchor_bytes > 0 {
            ensure!(
                cursor.anchor_bytes <= 128 && cursor.anchor_bytes as u64 <= cursor.offset,
                "invalid tail cursor anchor"
            );
            let anchor = read_bytes(
                &mut input,
                cursor.offset - cursor.anchor_bytes as u64,
                cursor.anchor_bytes,
                c,
                limits,
            )?;
            replaced = hash(&anchor) != cursor.anchor_sha256;
        }
        if replaced {
            tracing::warn!(stream_id=%key, offset=cursor.offset, "tail source truncated or overwritten; starting a new generation; use rename rotation to avoid losing unread bytes");
            cursor.reset(state, &key, limits)?;
        }
        if fingerprint.size <= cursor.offset {
            cursor.last_fingerprint = Some(fingerprint);
            cursor.save(state, &key)?;
            return Ok(Outcome::Skipped);
        }
        // Batch small live appends, limiting per-segment NFS metadata cost while preserving a short latency target.
        if fingerprint.size - cursor.offset < c.tail.segment_bytes as u64
            && monitoring::now().saturating_sub(cursor.last_ack) < c.tail.flush_seconds
        {
            return Ok(Outcome::Skipped);
        }
    }

    let count = if let Some(p) = &cursor.pending {
        ensure!(
            p.segment.bytes <= c.tail.mem_buf_limit as u64,
            "pending tail segment exceeds mem_buf_limit; restore the previous limit until it is delivered"
        );
        p.segment.bytes as usize
    } else {
        (fingerprint.size - cursor.offset).min(c.tail.segment_bytes as u64) as usize
    };
    ensure!(count <= c.tail.mem_buf_limit, "tail memory bound exceeded");
    let _memory = MemoryGuard(limits.metrics.clone());
    limits
        .metrics
        .buffered_bytes
        .store(count as u64, Ordering::Relaxed);
    limits
        .metrics
        .peak_buffered_bytes
        .fetch_max(count as u64, Ordering::Relaxed);
    if cursor.offset.saturating_add(count as u64) > fingerprint.size {
        let old_offset = cursor.offset;
        cursor.reset(state, &key, limits)?;
        anyhow::bail!(
            "unacknowledged tail range at {old_offset} disappeared; source retention was insufficient"
        );
    }
    let data = read_bytes(&mut input, cursor.offset, count, c, limits)?;
    let payload_hash = hash(&data);
    let pending = if let Some(pending) = cursor.pending.clone() {
        if payload_hash != pending.segment.payload_sha256 {
            cursor.reset(state, &key, limits)?;
            anyhow::bail!(
                "unacknowledged tail range was overwritten; original bytes are unavailable"
            );
        }
        pending
    } else {
        let id = hash(&serde_json::to_vec(&(
            "tail-segment-v1",
            &key,
            cursor.generation,
            cursor.offset,
            count,
            &payload_hash,
        ))?);
        let segment = Segment {
            id,
            stream_id: key.clone(),
            generation: cursor.generation,
            offset: cursor.offset,
            bytes: count as u64,
            source_root: source.path.to_string_lossy().into(),
            relative_path: relative.to_string_lossy().into(),
            relative_path_hex: hex::encode(relative.as_os_str().as_bytes()),
            payload_sha256: payload_hash,
        };
        let codec = if c.compression.skip_compressed
            && transfer::compressed(relative, &data[..data.len().min(8)])
        {
            Codec::None
        } else {
            c.compression.format
        };
        let anchor_bytes = count.min(128);
        let pending = Pending {
            segment,
            fingerprint,
            codec,
            level: c.compression.level,
            anchor_bytes,
            anchor_sha256: hash(&data[count - anchor_bytes..]),
        };
        cursor.pending = Some(pending.clone());
        // Persist this exact range before any remote publication, so a restart never widens an unacknowledged range.
        cursor.save(state, &key)?;
        pending
    };

    let object = destination.object(&source.name, &pending.segment.id)?;
    let dir = fd_path(&object);
    // Source state loss can rediscover an identical already-published segment.
    if let Some(receipt) = transfer::read_receipt(&dir.join("receipt.json"))? {
        ensure!(
            receipt.stream.as_ref() == Some(&pending.segment),
            "existing segment receipt mismatch"
        );
        integrity::verify(&dir.join("receipt.json"), limits, c.buffer_bytes)?;
        object.sync_all()?;
        cursor.acknowledge(state, &key, &pending)?;
        return Ok(Outcome::Shipped {
            input: count as u64,
            output: receipt.stored_bytes,
            resumed: count as u64,
        });
    }
    destination.check(
        (count as u64)
            .saturating_add(count as u64 / 8)
            .saturating_add(1024 * 1024),
    )?;
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("data.partial"))?;
    ensure!(
        output.metadata()?.is_file(),
        "tail output is not a regular file"
    );
    let mut frame_config = c.clone();
    frame_config.compression.level = pending.level;
    let (source_sha256, stored_sha256) = transfer::frame(
        &mut Cursor::new(&data),
        &mut output,
        count as u64,
        pending.codec,
        &frame_config,
        limits,
        true,
    )?;
    ensure!(
        source_sha256 == pending.segment.payload_sha256,
        "tail buffer integrity failure"
    );
    limits.ops.acquire(1, &limits.stop)?;
    output.sync_all()?;
    let output_bytes = output.metadata()?.len();
    drop(output);
    destination.check(0)?;
    fs::rename(dir.join("data.partial"), dir.join(pending.codec.filename()))?;
    object.sync_all()?;
    let chunk = Chunk {
        input_offset: 0,
        input_bytes: count as u64,
        output_offset: 0,
        output_bytes,
        source_sha256,
        stored_sha256,
    };
    let mut manifest = serde_json::to_vec(&chunk)?;
    manifest.push(b'\n');
    atomic_metadata(&object, "checksums.jsonl", &manifest)?;
    let mut fp = pending.fingerprint.clone();
    fp.size = count as u64;
    let receipt = Receipt {
        version: 2,
        id: pending.segment.id.clone(),
        source: source.name.clone(),
        relative_path: pending.segment.relative_path.clone(),
        relative_path_hex: pending.segment.relative_path_hex.clone(),
        source_root: pending.segment.source_root.clone(),
        fingerprint: fp,
        compression: pending.codec,
        stored_bytes: output_bytes,
        completed_unix_seconds: monitoring::now(),
        manifest_sha256: hash(&manifest),
        chunks: 1,
        stream: Some(pending.segment.clone()),
    };
    atomic_metadata(
        &object,
        "receipt.json",
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    cursor.acknowledge(state, &key, &pending)?;
    limits.metrics.checkpoints.fetch_add(1, Ordering::Relaxed);
    tracing::info!(stream_id=%key, generation=pending.segment.generation, offset=pending.segment.offset, bytes=count, "tail segment acknowledged by destination");
    Ok(Outcome::Shipped {
        input: count as u64,
        output: output_bytes,
        resumed: 0,
    })
}
