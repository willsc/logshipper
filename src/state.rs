use crate::{integrity::Chunk, monitoring::now};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::Path,
};

pub struct State {
    db: Connection,
    _lock: File,
    directory: File,
}
impl Drop for State {
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}
#[derive(Debug, Default, Clone, Copy)]
pub struct Progress {
    pub input: u64,
    pub output: u64,
    pub done: bool,
}
impl State {
    pub fn open(dir: &Path, binding: &str) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let directory = File::open(dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("lock"))?;
        lock.try_lock()
            .context("another logshipper is using this state directory")?;
        let db = Connection::open(dir.join("state.sqlite"))?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version <= 2,
            "state schema is newer than this daemon; refusing to downgrade"
        );
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            PRAGMA cache_size=-4096; PRAGMA journal_size_limit=16777216;
            CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS progress (id TEXT PRIMARY KEY, input INTEGER NOT NULL,
                output INTEGER NOT NULL, done INTEGER NOT NULL);",
        )?;
        if version < 2 {
            db.execute_batch("BEGIN IMMEDIATE;
                ALTER TABLE progress ADD COLUMN updated INTEGER NOT NULL DEFAULT 0;
                CREATE TABLE chunks (id TEXT NOT NULL, input_offset INTEGER NOT NULL, record TEXT NOT NULL,
                    PRIMARY KEY(id,input_offset));
                CREATE INDEX progress_updated ON progress(updated);
                PRAGMA user_version=2; COMMIT;")?;
        }
        db.execute(
            "INSERT OR IGNORE INTO settings VALUES ('destination', ?1)",
            [binding],
        )?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS tail_streams (
            id TEXT PRIMARY KEY, state_json TEXT NOT NULL, pending INTEGER NOT NULL, updated INTEGER NOT NULL);")?;
        let saved: String = db.query_row(
            "SELECT value FROM settings WHERE key='destination'",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            saved == binding,
            "state belongs to a different destination; use a separate state_dir"
        );
        directory.sync_all()?;
        Ok(Self {
            db,
            _lock: lock,
            directory,
        })
    }
    pub fn get(&self, id: &str) -> Result<Progress> {
        let row = self
            .db
            .query_row(
                "SELECT input,output,done FROM progress WHERE id=?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?;
        match row {
            Some((input, output, done)) => Ok(Progress {
                input: input.try_into()?,
                output: output.try_into()?,
                done,
            }),
            None => Ok(Progress::default()),
        }
    }
    pub fn save(&self, id: &str, p: Progress) -> Result<()> {
        self.db.execute("INSERT INTO progress (id,input,output,done,updated) VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(id) DO UPDATE SET input=excluded.input,output=excluded.output,done=excluded.done,updated=excluded.updated",
            params![id, i64::try_from(p.input)?, i64::try_from(p.output)?, p.done, i64::try_from(now())?])?;
        Ok(())
    }

    pub fn checkpoint(&self, id: &str, p: Progress, chunk: &Chunk) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        self.save(id, p)?;
        tx.execute(
            "INSERT INTO chunks VALUES (?1,?2,?3)",
            params![
                id,
                i64::try_from(chunk.input_offset)?,
                serde_json::to_string(chunk)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn chunks(&self, id: &str, mut visit: impl FnMut(Chunk) -> Result<()>) -> Result<u64> {
        let mut statement = self
            .db
            .prepare("SELECT record FROM chunks WHERE id=?1 ORDER BY input_offset")?;
        let mut rows = statement.query([id])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            visit(serde_json::from_str(&row.get::<_, String>(0)?)?)?;
            count += 1;
        }
        Ok(count)
    }
    pub fn reset(&self, id: &str) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM chunks WHERE id=?1", [id])?;
        self.save(id, Progress::default())?;
        tx.commit()?;
        Ok(())
    }
    pub fn prune(&self, days: u64) -> Result<usize> {
        let cutoff = i64::try_from(now().saturating_sub(days * 86400))?;
        let tx = self.db.unchecked_transaction()?;
        tx.execute("DELETE FROM chunks WHERE id IN (SELECT id FROM progress WHERE updated < ?1 AND done=1 ORDER BY updated,id LIMIT 1000)", [cutoff])?;
        let count = tx.execute("DELETE FROM progress WHERE id IN (SELECT id FROM progress WHERE updated < ?1 AND done=1 ORDER BY updated,id LIMIT 1000)", [cutoff])?;
        tx.commit()?;
        Ok(count)
    }
    pub fn pending(&self) -> Result<u64> {
        Ok(self
            .db
            .query_row("SELECT (SELECT count(*) FROM progress WHERE done=0) + (SELECT count(*) FROM tail_streams WHERE pending=1)", [], |r| {
                r.get::<_, i64>(0)
            })?
            .try_into()?)
    }
    pub fn check_space(&self, minimum: u64) -> Result<()> {
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: live descriptor and valid output pointer; initialized only on success.
        let rc = unsafe { libc::fstatvfs(self.directory.as_raw_fd(), stat.as_mut_ptr()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stat = unsafe { stat.assume_init() };
        ensure!(
            stat.f_bavail.saturating_mul(stat.f_frsize) >= minimum,
            "local state disk free-space reserve reached"
        );
        Ok(())
    }
    pub fn tail_get(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT state_json FROM tail_streams WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn tail_save(&self, id: &str, state_json: &str, pending: bool) -> Result<()> {
        self.db.execute("INSERT INTO tail_streams VALUES (?1,?2,?3,?4)
            ON CONFLICT(id) DO UPDATE SET state_json=excluded.state_json,pending=excluded.pending,updated=excluded.updated",
            params![id, state_json, pending, i64::try_from(now())?])?;
        Ok(())
    }
}
