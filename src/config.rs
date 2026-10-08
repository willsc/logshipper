use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub state_dir: PathBuf,
    pub scan_interval_seconds: u64,
    pub settle_seconds: u64,
    pub buffer_bytes: usize,
    pub checkpoint_bytes: u64,
    pub destination: DestinationConfig,
    pub io: IoConfig,
    pub compression: CompressionConfig,
    pub monitoring: MonitoringConfig,
    pub state_retention_days: u64,
    pub min_state_free_bytes: u64,
    pub tail: TailConfig,
    pub sources: Vec<Source>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            state_dir: "/var/lib/logshipper".into(),
            scan_interval_seconds: 30,
            settle_seconds: 120,
            buffer_bytes: 256 * 1024,
            checkpoint_bytes: 64 * 1024 * 1024,
            destination: DestinationConfig::default(),
            io: IoConfig::default(),
            compression: CompressionConfig::default(),
            monitoring: MonitoringConfig::default(),
            state_retention_days: 30,
            min_state_free_bytes: 64 * 1024 * 1024,
            tail: TailConfig::default(),
            sources: vec![],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DestinationConfig {
    pub mount_path: PathBuf,
    pub directory: PathBuf,
    pub namespace: String,
    pub require_mount: bool,
    pub allowed_filesystems: Vec<String>,
    pub min_free_bytes: u64,
    pub expected_source: Option<String>,
}
impl Default for DestinationConfig {
    fn default() -> Self {
        Self {
            mount_path: "/mnt/fsx".into(),
            directory: "logshipper".into(),
            namespace: "".into(),
            require_mount: true,
            allowed_filesystems: vec!["nfs".into(), "nfs4".into()],
            min_free_bytes: 1024 * 1024 * 1024,
            expected_source: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitoringConfig {
    pub listen: Option<SocketAddr>,
    pub stall_seconds: u64,
}
impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            listen: None,
            stall_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IoConfig {
    pub read_bytes_per_second: u64,
    pub write_bytes_per_second: u64,
    pub operations_per_second: u64,
    pub scan_entries_per_second: u64,
}
impl Default for IoConfig {
    fn default() -> Self {
        Self {
            read_bytes_per_second: 20 * 1024 * 1024,
            write_bytes_per_second: 10 * 1024 * 1024,
            operations_per_second: 100,
            scan_entries_per_second: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    None,
    Gzip,
    Zstd,
}
impl Codec {
    pub fn filename(self) -> &'static str {
        match self {
            Self::None => "data",
            Self::Gzip => "data.gz",
            Self::Zstd => "data.zst",
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressionConfig {
    pub format: Codec,
    pub level: i32,
    pub skip_compressed: bool,
}
impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            format: Codec::Zstd,
            level: 3,
            skip_compressed: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub mode: SourceMode,
    #[serde(default = "all_files")]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}
fn all_files() -> Vec<String> {
    vec!["**/*".into()]
}
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceMode {
    #[default]
    Archive,
    Tail,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TailConfig {
    pub poll_seconds: u64,
    pub flush_seconds: u64,
    pub mem_buf_limit: usize,
    pub segment_bytes: usize,
}
impl Default for TailConfig {
    fn default() -> Self {
        Self {
            poll_seconds: 1,
            flush_seconds: 5,
            mem_buf_limit: 8 * 1024 * 1024,
            segment_bytes: 1024 * 1024,
        }
    }
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

// Resolve existing ancestors too, so a symlink cannot conceal overlapping trees.
fn resolve(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "path must be absolute: {}",
        path.display()
    );
    ensure!(
        !path.components().any(|c| matches!(c, Component::ParentDir)),
        "parent components are forbidden"
    );
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let parent = path.parent().context("path has no parent")?;
    Ok(resolve(parent)?.join(path.file_name().context("path has no filename")?))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut c: Self = toml::from_str(&fs::read_to_string(path)?)
            .with_context(|| format!("invalid configuration {}", path.display()))?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&mut self) -> Result<()> {
        ensure!(!self.sources.is_empty(), "at least one source is required");
        ensure!(
            (1..=3600).contains(&self.tail.poll_seconds),
            "tail.poll_seconds must be 1..3600"
        );
        ensure!(
            self.tail.flush_seconds <= 3600,
            "tail.flush_seconds must be 0..3600"
        );
        ensure!(
            (4096..=64 * 1024 * 1024).contains(&self.tail.mem_buf_limit),
            "tail.mem_buf_limit must be 4 KiB..64 MiB"
        );
        ensure!(
            (4096..=self.tail.mem_buf_limit).contains(&self.tail.segment_bytes),
            "tail.segment_bytes must be 4 KiB..mem_buf_limit"
        );
        ensure!(
            (1..=86400).contains(&self.scan_interval_seconds),
            "scan interval must be 1..86400 seconds"
        );
        ensure!(
            (1..=3650).contains(&self.state_retention_days),
            "state_retention_days must be 1..3650"
        );
        ensure!(
            (1..=86400).contains(&self.monitoring.stall_seconds),
            "monitoring.stall_seconds must be 1..86400"
        );
        ensure!(
            !self.destination.require_mount || !self.destination.allowed_filesystems.is_empty(),
            "allowed_filesystems cannot be empty when requiring a mount"
        );
        ensure!(
            self.destination
                .expected_source
                .as_ref()
                .is_none_or(|s| !s.is_empty()),
            "expected_source must not be empty"
        );
        ensure!(
            (4096..=4 * 1024 * 1024).contains(&self.buffer_bytes),
            "buffer_bytes must be 4 KiB..4 MiB"
        );
        ensure!(
            self.checkpoint_bytes >= self.buffer_bytes as u64
                && self.checkpoint_bytes <= 1024 * 1024 * 1024,
            "checkpoint_bytes must be between buffer_bytes and 1 GiB"
        );
        ensure!(
            valid_name(&self.destination.namespace),
            "set destination.namespace to a unique host name (letters, digits, dot, underscore, hyphen)"
        );
        ensure!(
            !self.destination.directory.as_os_str().is_empty()
                && self
                    .destination
                    .directory
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "destination.directory must be a relative path without . or .."
        );
        ensure!(
            self.io.read_bytes_per_second > 0
                && self.io.write_bytes_per_second > 0
                && self.io.operations_per_second > 0
                && self.io.scan_entries_per_second > 0,
            "all I/O limits must be positive"
        );
        match self.compression.format {
            Codec::Gzip => ensure!(
                (0..=9).contains(&self.compression.level),
                "gzip level must be 0..9"
            ),
            Codec::Zstd => ensure!(
                (1..=19).contains(&self.compression.level),
                "zstd level must be 1..19"
            ),
            Codec::None => (),
        }
        self.destination.mount_path = resolve(&self.destination.mount_path)?;
        self.state_dir = resolve(&self.state_dir)?;
        ensure!(
            !overlap(&self.state_dir, &self.destination.mount_path),
            "state must be on local storage outside destination mount"
        );
        let mut names = HashSet::new();
        let mut paths: Vec<PathBuf> = vec![];
        for source in &mut self.sources {
            ensure!(
                valid_name(&source.name)
                    && source.name != ".lock"
                    && names.insert(source.name.clone()),
                "source names must be valid and unique"
            );
            source.path = resolve(&source.path)?;
            ensure!(
                source.path.is_dir(),
                "source must be a directory: {}",
                source.path.display()
            );
            ensure!(
                !overlap(&source.path, &self.destination.mount_path)
                    && !overlap(&source.path, &self.state_dir),
                "source, state and destination trees must not overlap"
            );
            ensure!(
                !paths.iter().any(|p| overlap(p, &source.path)),
                "source trees must not overlap"
            );
            paths.push(source.path.clone());
            for pattern in source.include.iter().chain(&source.exclude) {
                globset::Glob::new(pattern)?;
            }
        }
        Ok(())
    }
}
fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
