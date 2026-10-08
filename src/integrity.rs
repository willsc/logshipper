use crate::{config::Codec, rate::Limits, transfer::Receipt};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::atomic::Ordering,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    pub input_offset: u64,
    pub input_bytes: u64,
    pub output_offset: u64,
    pub output_bytes: u64,
    pub source_sha256: String,
    pub stored_sha256: String,
}

pub struct HashWriter<W> {
    pub inner: W,
    pub hash: Sha256,
    pub bytes: u64,
}
impl<W> HashWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            bytes: 0,
        }
    }
}
impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(b)?;
        self.hash.update(&b[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
struct HashReader<'a, R> {
    inner: R,
    hash: Sha256,
    limits: &'a mut Limits,
    buffer_size: usize,
}
impl<R: Read> Read for HashReader<'_, R> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let size = b.len().min(self.buffer_size);
        self.limits.reading(size)?;
        let n = self.inner.read(&mut b[..size])?;
        self.hash.update(&b[..n]);
        self.limits
            .metrics
            .read_bytes
            .fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

pub fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "not a regular file: {}",
        path.display()
    );
    Ok(file)
}

/// Verify persisted bytes at the current file offset without unbounded allocation.
pub fn stored_hash(
    file: &mut File,
    size: u64,
    buffer_size: usize,
    limits: &mut Limits,
) -> Result<String> {
    let mut hash = Sha256::new();
    let mut left = size;
    let mut buffer = vec![0u8; buffer_size];
    while left > 0 {
        let amount = left.min(buffer.len() as u64) as usize;
        limits.reading(amount)?;
        let n = file.read(&mut buffer[..amount])?;
        ensure!(n > 0, "archive truncated during integrity verification");
        hash.update(&buffer[..n]);
        left -= n as u64;
        limits
            .metrics
            .read_bytes
            .fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(hex::encode(hash.finalize()))
}

pub fn validate_chunk(chunk: &Chunk, input: u64, output: u64) -> Result<()> {
    ensure!(
        chunk.input_offset == input && chunk.output_offset == output,
        "noncontiguous checksum manifest"
    );
    ensure!(
        chunk.input_bytes <= 1024 * 1024 * 1024,
        "invalid chunk input size"
    );
    ensure!(
        chunk.output_bytes <= 2 * 1024 * 1024 * 1024,
        "invalid chunk output size"
    );
    for hash in [&chunk.source_sha256, &chunk.stored_sha256] {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid SHA-256 digest"
        );
    }
    Ok(())
}

/// Read-only full verification, including decompression and checksums of the original bytes.
pub fn verify(receipt_path: &Path, limits: &mut Limits, buffer_size: usize) -> Result<Receipt> {
    let receipt: Receipt = serde_json::from_reader(open_regular(receipt_path)?.take(1024 * 1024))?;
    ensure!(
        receipt.version == 2,
        "this archive has no v2 integrity manifest; legacy metadata is insufficient for verification"
    );
    if let Some(stream) = &receipt.stream {
        ensure!(
            stream.id == receipt.id
                && stream.bytes == receipt.fingerprint.size
                && receipt.chunks == 1,
            "tail receipt range does not match its archived payload"
        );
    }
    let parent = receipt_path.parent().context("receipt has no parent")?;
    let mut data = open_regular(&parent.join(receipt.compression.filename()))?;
    ensure!(
        data.metadata()?.len() == receipt.stored_bytes,
        "archive size mismatch"
    );
    let mut manifest = BufReader::new(open_regular(&parent.join("checksums.jsonl"))?);
    let mut manifest_hash = Sha256::new();
    let mut input_offset = 0u64;
    let mut output_offset = 0u64;
    let mut chunks = 0u64;
    let mut buffer = vec![0u8; buffer_size];
    loop {
        let mut line = Vec::new();
        let n = manifest.by_ref().take(4097).read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        ensure!(
            n <= 4096 && line.last() == Some(&b'\n'),
            "oversized or incomplete checksum record"
        );
        manifest_hash.update(&line);
        let chunk: Chunk = serde_json::from_slice(&line)?;
        if let Some(stream) = &receipt.stream {
            ensure!(
                stream.payload_sha256 == chunk.source_sha256,
                "tail payload digest differs from checksum manifest"
            );
        }
        validate_chunk(&chunk, input_offset, output_offset)?;
        let mut reader = HashReader {
            inner: (&mut data).take(chunk.output_bytes),
            hash: Sha256::new(),
            limits,
            buffer_size,
        };
        let mut source_hash = Sha256::new();
        let mut source_bytes = 0u64;
        {
            let mut decoded: Box<dyn Read + '_> = match receipt.compression {
                Codec::None => Box::new(&mut reader),
                Codec::Gzip => Box::new(flate2::read::MultiGzDecoder::new(&mut reader)),
                Codec::Zstd => Box::new(zstd::stream::read::Decoder::new(&mut reader)?),
            };
            loop {
                let n = decoded.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                source_bytes += n as u64;
                ensure!(
                    source_bytes <= chunk.input_bytes,
                    "decoded chunk exceeds declared size"
                );
                source_hash.update(&buffer[..n]);
            }
        }
        ensure!(
            reader.inner.limit() == 0,
            "compressed chunk has trailing unread bytes"
        );
        ensure!(
            hex::encode(reader.hash.finalize()) == chunk.stored_sha256,
            "stored chunk checksum mismatch at offset {}",
            output_offset
        );
        ensure!(
            source_bytes == chunk.input_bytes
                && hex::encode(source_hash.finalize()) == chunk.source_sha256,
            "source checksum mismatch at offset {}",
            input_offset
        );
        input_offset = input_offset
            .checked_add(chunk.input_bytes)
            .context("input size overflow")?;
        output_offset = output_offset
            .checked_add(chunk.output_bytes)
            .context("output size overflow")?;
        chunks += 1;
        ensure!(chunks <= receipt.chunks, "too many checksum records");
    }
    ensure!(
        hex::encode(manifest_hash.finalize()) == receipt.manifest_sha256,
        "checksum manifest digest mismatch"
    );
    ensure!(
        input_offset == receipt.fingerprint.size
            && output_offset == receipt.stored_bytes
            && chunks == receipt.chunks
            && chunks > 0,
        "archive totals do not match receipt"
    );
    Ok(receipt)
}
