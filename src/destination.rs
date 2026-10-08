use crate::config::DestinationConfig;
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

pub fn fd_path(dir: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
}

fn open_dir(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?)
}

/// Every directory is opened without following symlinks and pinned by its descriptor.
pub fn subdir(parent: &File, name: &OsStr) -> Result<File> {
    let path = fd_path(parent).join(name);
    match fs::create_dir(&path) {
        Ok(()) => parent.sync_all()?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    open_dir(&path).with_context(|| format!("cannot open archive directory {}", path.display()))
}

fn unescape_mount(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            out.push(
                ((bytes[i + 1] - b'0') * 64) + ((bytes[i + 2] - b'0') * 8) + bytes[i + 3] - b'0',
            );
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn parse_mount(info: &str, config: &DestinationConfig) -> Result<Option<String>> {
    if !config.require_mount {
        return Ok(None);
    }
    for line in info.lines().rev() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 7 || unescape_mount(fields[4]) != config.mount_path.as_os_str().as_bytes()
        {
            continue;
        }
        let separator = fields
            .iter()
            .position(|s| *s == "-")
            .context("invalid mountinfo")?;
        let kind = fields
            .get(separator + 1)
            .context("missing filesystem type")?;
        ensure!(
            config.allowed_filesystems.iter().any(|s| s == kind),
            "unexpected filesystem type {kind}"
        );
        let source = fields.get(separator + 2).context("missing mount source")?;
        if let Some(expected) = &config.expected_source {
            ensure!(
                unescape_mount(source) == expected.as_bytes(),
                "mounted NFS export does not match expected_source"
            );
        }
        if *kind == "nfs" || *kind == "nfs4" {
            let options = fields.iter().skip(separator + 3).flat_map(|s| s.split(','));
            for option in options {
                ensure!(
                    !matches!(
                        option,
                        "soft"
                            | "softerr"
                            | "softreval"
                            | "nolock"
                            | "local_lock=all"
                            | "local_lock=flock"
                    ),
                    "unsafe NFS option {option}: use hard recovery and server-coordinated locking"
                );
            }
        }
        return Ok(Some(format!(
            "{}:{}:{}:{}:{}",
            fields[0], fields[2], kind, fields[3], source
        )));
    }
    bail!(
        "required filesystem is not mounted at {}",
        config.mount_path.display()
    )
}
fn mount_identity(config: &DestinationConfig) -> Result<Option<String>> {
    if !config.require_mount {
        return Ok(None);
    }
    parse_mount(&fs::read_to_string("/proc/self/mountinfo")?, config)
}

fn descriptor_mount_id(file: &File) -> Result<String> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    Ok(info
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:").map(str::trim))
        .context("missing descriptor mount ID")?
        .to_string())
}

pub struct Destination {
    config: DestinationConfig,
    mount: File,
    root: File,
    identity: Option<String>,
    _lock: File,
}
impl Drop for Destination {
    fn drop(&mut self) {
        // Explicit unlock also releases locks briefly inherited by a fork-before-exec child.
        let _ = self._lock.unlock();
    }
}
impl Destination {
    pub fn open(config: &DestinationConfig) -> Result<Self> {
        let identity = mount_identity(config)?;
        let mount = open_dir(&config.mount_path)?;
        if let Some(identity) = &identity {
            ensure!(
                identity.split(':').next() == Some(descriptor_mount_id(&mount)?.as_str()),
                "opened directory belongs to a different mount"
            );
        }
        ensure!(
            mount_identity(config)? == identity,
            "mount changed during startup"
        );
        ensure!(
            mount.metadata()?.dev() == fs::metadata(&config.mount_path)?.dev(),
            "mount changed during startup"
        );
        let mut root = mount.try_clone()?;
        for part in config.directory.components() {
            let Component::Normal(name) = part else {
                bail!("invalid archive directory");
            };
            root = subdir(&root, name)?;
        }
        root = subdir(&root, OsStr::new(&config.namespace))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(fd_path(&root).join(".lock"))?;
        lock.try_lock()
            .context("archive namespace is locked, or the filesystem does not support flock")?;
        let result = Self {
            config: config.clone(),
            mount,
            root,
            identity,
            _lock: lock,
        };
        result.check(0)?;
        Ok(result)
    }
    pub fn check(&self, reserve: u64) -> Result<()> {
        ensure!(
            mount_identity(&self.config)? == self.identity,
            "destination mount changed; reopening on next attempt"
        );
        ensure!(
            self.mount.metadata()?.dev() == fs::metadata(&self.config.mount_path)?.dev(),
            "destination device changed"
        );
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: fd is live; fstatvfs initializes stat on success.
        let rc = unsafe { libc::fstatvfs(self.root.as_raw_fd(), stat.as_mut_ptr()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stat = unsafe { stat.assume_init() };
        let available = stat.f_bavail.saturating_mul(stat.f_frsize);
        ensure!(
            available >= self.config.min_free_bytes.saturating_add(reserve),
            "destination free-space reserve would be exceeded"
        );
        Ok(())
    }
    pub fn object(&self, source: &str, id: &str) -> Result<File> {
        self.check(0)?;
        let source = subdir(&self.root, OsStr::new(source))?;
        let shard = subdir(&source, OsStr::new(&id[..2]))?;
        subdir(&shard, OsStr::new(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> DestinationConfig {
        DestinationConfig {
            mount_path: "/mnt/fsx".into(),
            expected_source: Some("fsx.example:/export".into()),
            ..DestinationConfig::default()
        }
    }
    #[test]
    fn accepts_hard_nfs_and_checks_export() {
        let info = "101 1 0:42 / /mnt/fsx rw,relatime - nfs4 fsx.example:/export rw,vers=4.1,hard,local_lock=none";
        assert!(parse_mount(info, &config()).unwrap().is_some());
        assert!(parse_mount(&info.replace("fsx.example", "wrong.example"), &config()).is_err());
    }
    #[test]
    fn rejects_soft_mounts_and_local_only_locks() {
        for option in [
            "soft",
            "softerr",
            "softreval",
            "nolock",
            "local_lock=all",
            "local_lock=flock",
        ] {
            let info = format!("101 1 0:42 / /mnt/fsx rw - nfs fsx.example:/export rw,{option}");
            assert!(parse_mount(&info, &config()).is_err(), "{option}");
        }
    }
    #[test]
    fn escaped_mountpoints_are_compared_as_bytes() {
        let mut c = config();
        c.mount_path = "/mnt/my fsx".into();
        assert!(
            parse_mount(
                "101 1 0:42 / /mnt/my\\040fsx rw - nfs4 fsx.example:/export rw,hard",
                &c
            )
            .is_ok()
        );
    }
}
