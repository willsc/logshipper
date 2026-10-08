use crate::{
    config::{Codec, Config, Source},
    destination::{Destination, fd_path},
    integrity::{self, Chunk, HashWriter},
    rate::{Limits, check_stop},
    state::{Progress, State},
};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, write::GzEncoder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path},
    sync::atomic::Ordering,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: i64,
    pub ctime: i64,
    pub ctime_nsec: i64,
}
impl Fingerprint {
    pub fn of(m: &Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
        }
    }
    pub fn settled(&self, seconds: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i128;
        let newest = i128::from(self.mtime.max(self.ctime));
        now - newest >= i128::from(seconds)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub version: u32,
    pub id: String,
    pub source: String,
    pub relative_path: String,
    pub relative_path_hex: String,
    pub source_root: String,
    pub fingerprint: Fingerprint,
    pub compression: Codec,
    pub stored_bytes: u64,
    pub completed_unix_seconds: u64,
    #[serde(default)]
    pub manifest_sha256: String,
    #[serde(default)]
    pub chunks: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<crate::tail::Segment>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Shipped {
        input: u64,
        output: u64,
        resumed: u64,
    },
    Skipped,
}

fn identifier(c: &Config, source: &Source, path: &Path, fp: &Fingerprint) -> Result<String> {
    let mut hash = Sha256::new();
    // Length-prefixed components prevent path/field concatenation collisions.
    for field in [
        b"logshipper-v2".to_vec(),
        source.path.as_os_str().as_bytes().to_vec(),
        source.name.as_bytes().to_vec(),
        path.as_os_str().as_bytes().to_vec(),
        serde_json::to_vec(fp)?,
        serde_json::to_vec(&(
            c.compression.format,
            c.compression.level,
            c.compression.skip_compressed,
        ))?,
    ] {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field);
    }
    Ok(hex::encode(hash.finalize()))
}

// Pin each source directory; never follow a symlink, including raced parent components.
pub(crate) fn open_source(source: &Source, relative: &Path) -> Result<File> {
    let mut dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&source.path)?;
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            anyhow::bail!("invalid source-relative path");
        };
        let flags = libc::O_NOFOLLOW
            | if parts.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                libc::O_NONBLOCK
            };
        dir = OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(fd_path(&dir).join(name))?;
    }
    ensure!(dir.metadata()?.is_file(), "source is not a regular file");
    Ok(dir)
}

pub(crate) fn compressed(path: &Path, bytes: &[u8]) -> bool {
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    [
        "gz", "gzip", "tgz", "zst", "zstd", "bz2", "xz", "zip", "7z", "lz4", "lz", "lzma", "br",
        "snappy", "rar",
    ]
    .contains(&ext.as_str())
        || [
            b"\x1f\x8b".as_slice(),
            b"\x28\xb5\x2f\xfd",
            b"BZh",
            b"\xfd7zXZ\0",
            b"PK\x03\x04",
            b"PK\x05\x06",
            b"7z\xbc\xaf\x27\x1c",
            b"\x04\x22\x4d\x18",
            b"Rar!",
        ]
        .iter()
        .any(|m| bytes.starts_with(m))
}

pub(crate) fn frame(
    input: &mut impl Read,
    output: &mut File,
    count: u64,
    codec: Codec,
    c: &Config,
    limits: &mut Limits,
    input_already_read: bool,
) -> Result<(String, String)> {
    // Both directions share one operations limiter. RefCell is local to this single worker.
    let limits = std::cell::RefCell::new(limits);
    let stored_hash = std::cell::RefCell::new(Sha256::new());
    let mut source_hash = Sha256::new();
    struct SharedWriter<'a, 'b> {
        file: &'a mut File,
        limits: &'a std::cell::RefCell<&'b mut Limits>,
        max: usize,
        hash: &'a std::cell::RefCell<Sha256>,
    }
    impl Write for SharedWriter<'_, '_> {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let size = b.len().min(self.max);
            self.limits.borrow_mut().writing(size)?;
            let n = self.file.write(&b[..size])?;
            self.hash.borrow_mut().update(&b[..n]);
            self.limits
                .borrow()
                .metrics
                .written_bytes
                .fetch_add(n as u64, Ordering::Relaxed);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }
    let writer = BufWriter::with_capacity(
        c.buffer_bytes,
        SharedWriter {
            file: output,
            limits: &limits,
            max: c.buffer_bytes,
            hash: &stored_hash,
        },
    );
    let copy = |writer: &mut dyn Write| -> Result<()> {
        let mut buffer = vec![0u8; c.buffer_bytes];
        let mut remaining = count;
        while remaining > 0 {
            let amount = remaining.min(buffer.len() as u64) as usize;
            if !input_already_read {
                limits.borrow_mut().reading(amount)?;
            }
            let n = input.read(&mut buffer[..amount])?;
            ensure!(n > 0, "source truncated during transfer");
            source_hash.update(&buffer[..n]);
            if !input_already_read {
                limits
                    .borrow()
                    .metrics
                    .read_bytes
                    .fetch_add(n as u64, Ordering::Relaxed);
            }
            writer.write_all(&buffer[..n])?;
            remaining -= n as u64;
        }
        Ok(())
    };
    let mut copy = copy;
    let mut writer = match codec {
        Codec::None => {
            let mut w = writer;
            copy(&mut w)?;
            w
        }
        Codec::Gzip => {
            let mut w = GzEncoder::new(writer, Compression::new(c.compression.level as u32));
            copy(&mut w)?;
            w.finish()?
        }
        Codec::Zstd => {
            let mut w = zstd::stream::write::Encoder::new(writer, c.compression.level)?;
            w.include_checksum(true)?;
            copy(&mut w)?;
            w.finish()?
        }
    };
    writer.flush()?;
    drop(writer);
    Ok((
        hex::encode(source_hash.finalize()),
        hex::encode(stored_hash.into_inner().finalize()),
    ))
}

pub(crate) fn read_receipt(path: &Path) -> Result<Option<Receipt>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => Ok(Some(serde_json::from_reader(file.take(1024 * 1024))?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn archive(
    c: &Config,
    source: &Source,
    relative: &Path,
    expected: &Fingerprint,
    destination: &Destination,
    state: &State,
    limits: &mut Limits,
) -> Result<Outcome> {
    check_stop(&limits.stop)?;
    let mut input = open_source(source, relative)?;
    ensure!(
        &Fingerprint::of(&input.metadata()?) == expected,
        "source changed before transfer"
    );
    let id = identifier(c, source, relative, expected)?;
    let object = destination.object(&source.name, &id)?;
    let dir = fd_path(&object);
    let mut progress = state.get(&id)?;
    if let Some(receipt) = read_receipt(&dir.join("receipt.json"))? {
        ensure!(
            receipt.version == 2 && receipt.id == id && &receipt.fingerprint == expected,
            "invalid archive receipt"
        );
        let data = fs::symlink_metadata(dir.join(receipt.compression.filename()))?;
        ensure!(
            data.is_file() && data.len() == receipt.stored_bytes,
            "completed archive is missing or damaged"
        );
        let manifest = integrity::open_regular(&dir.join("checksums.jsonl"))?;
        ensure!(
            manifest.metadata()?.len() > 0
                && receipt.chunks > 0
                && receipt.manifest_sha256.len() == 64,
            "completed integrity manifest is missing or invalid"
        );
        if !progress.done {
            // A prior attempt may have published the receipt but failed the directory sync.
            // Retry that durability boundary before recording local completion.
            object.sync_all()?;
            state.save(
                &id,
                Progress {
                    input: expected.size,
                    output: data.len(),
                    done: true,
                },
            )?;
        }
        return Ok(Outcome::Skipped);
    }
    ensure!(
        !progress.done,
        "completed archive receipt is missing; restore it before retrying"
    );
    let mut magic = [0u8; 8];
    let sniff = expected.size.min(magic.len() as u64) as usize;
    limits.reading(sniff)?;
    input.read_exact(&mut magic[..sniff])?;
    limits
        .metrics
        .read_bytes
        .fetch_add(sniff as u64, Ordering::Relaxed);
    let codec = if c.compression.skip_compressed && compressed(relative, &magic[..sniff]) {
        Codec::None
    } else {
        c.compression.format
    };
    let partial = dir.join("data.partial");
    // Recover a crash between the durable data rename and receipt publication without recopying.
    if !partial.try_exists()? && progress.input == expected.size {
        let final_path = dir.join(codec.filename());
        match fs::symlink_metadata(&final_path) {
            Ok(m) if m.is_file() && m.len() == progress.output => fs::rename(final_path, &partial)?,
            Ok(_) => anyhow::bail!("uncommitted archive has unexpected type or size"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    let existed = partial.try_exists()?;
    let mut output = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&partial)?;
    ensure!(
        output.metadata()?.is_file(),
        "partial is not a regular file"
    );
    if !existed || output.metadata()?.len() < progress.output || progress.input > expected.size {
        progress = Progress::default();
        state.reset(&id)?;
    }
    // A durable offset alone cannot prove that a remote filesystem retained the right bytes.
    // Validate every persisted chunk before trusting a resumed prefix.
    let mut chunk_count = 0;
    if progress.input > 0 || progress.output > 0 || expected.size == 0 {
        output.seek(SeekFrom::Start(0))?;
        let mut in_offset = 0;
        let mut out_offset = 0;
        let mut valid = true;
        chunk_count = state.chunks(&id, |chunk| {
            integrity::validate_chunk(&chunk, in_offset, out_offset)?;
            if valid
                && integrity::stored_hash(&mut output, chunk.output_bytes, c.buffer_bytes, limits)?
                    != chunk.stored_sha256
            {
                valid = false;
            }
            in_offset += chunk.input_bytes;
            out_offset += chunk.output_bytes;
            Ok(())
        })?;
        if !valid
            || in_offset != progress.input
            || out_offset != progress.output
            || (chunk_count == 0 && progress.input > 0)
        {
            limits
                .metrics
                .integrity_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(%id, "incomplete archive failed integrity validation; restarting from source");
            state.reset(&id)?;
            progress = Progress::default();
            chunk_count = 0;
        }
    }
    output.set_len(progress.output)?; // Drop any uncommitted tail left by an interruption.
    output.seek(SeekFrom::Start(progress.output))?;
    input.seek(SeekFrom::Start(progress.input))?;
    let resumed = progress.input;
    tracing::info!(path = %source.path.join(relative).display(), bytes = expected.size, resumed, ?codec, "transfer started");
    // Emit a valid empty compressed stream for a zero-byte input too.
    let mut empty = expected.size == 0 && chunk_count == 0;
    while progress.input < expected.size || empty {
        check_stop(&limits.stop)?;
        state.check_space(c.min_state_free_bytes)?;
        let count = (expected.size - progress.input).min(c.checkpoint_bytes);
        // Reserve for worst-case codec overhead, not just the expected compression ratio.
        destination.check(count.saturating_add(count / 8).saturating_add(1024 * 1024))?;
        let (source_sha256, stored_sha256) =
            frame(&mut input, &mut output, count, codec, c, limits, false)?;
        ensure!(
            &Fingerprint::of(&input.metadata()?) == expected,
            "source changed during transfer; version left incomplete"
        );
        limits.ops.acquire(1, &limits.stop)?;
        output.sync_all()?;
        object.sync_all()?;
        let next_output = output.stream_position()?;
        let chunk = Chunk {
            input_offset: progress.input,
            input_bytes: count,
            output_offset: progress.output,
            output_bytes: next_output - progress.output,
            source_sha256,
            stored_sha256,
        };
        progress.input += count;
        progress.output = next_output;
        state.checkpoint(&id, progress, &chunk)?; // Atomically persist offsets and integrity metadata after fsync.
        limits.metrics.checkpoints.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(%id, input_bytes = progress.input, output_bytes = progress.output, "checkpoint saved");
        empty = false;
    }
    ensure!(
        &Fingerprint::of(&input.metadata()?) == expected,
        "source changed before publication"
    );
    destination.check(0)?;
    output.sync_all()?;
    drop(output);
    fs::rename(&partial, dir.join(codec.filename()))?;
    object.sync_all()?;
    let manifest_tmp = dir.join("checksums.tmp");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&manifest_tmp)?;
    let mut manifest = HashWriter::new(BufWriter::new(file));
    let chunks = state.chunks(&id, |chunk| {
        check_stop(&limits.stop)?;
        serde_json::to_writer(&mut manifest, &chunk)?;
        manifest.write_all(b"\n")?;
        Ok(())
    })?;
    ensure!(
        chunks > 0,
        "missing integrity metadata for completed transfer"
    );
    manifest.flush()?;
    manifest.inner.get_ref().sync_all()?;
    fs::rename(manifest_tmp, dir.join("checksums.jsonl"))?;
    object.sync_all()?;
    let receipt = Receipt {
        version: 2,
        id: id.clone(),
        source: source.name.clone(),
        relative_path: relative.to_string_lossy().into(),
        relative_path_hex: hex::encode(relative.as_os_str().as_bytes()),
        source_root: source.path.to_string_lossy().into(),
        fingerprint: expected.clone(),
        compression: codec,
        stored_bytes: progress.output,
        completed_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        manifest_sha256: hex::encode(manifest.hash.finalize()),
        chunks,
        stream: None,
    };
    let receipt_tmp = dir.join("receipt.tmp");
    let mut receipt_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&receipt_tmp)?;
    serde_json::to_writer_pretty(&mut receipt_file, &receipt)?;
    receipt_file.sync_all()?;
    fs::rename(receipt_tmp, dir.join("receipt.json"))?;
    object.sync_all()?;
    progress.done = true;
    state
        .save(&id, progress)
        .context("archive published, but local completion update failed; next scan will recover")?;
    Ok(Outcome::Shipped {
        input: expected.size,
        output: progress.output,
        resumed,
    })
}
