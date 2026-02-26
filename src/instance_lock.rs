//! Single-instance session lock using OS file locking (flock).
//!
//! Prevents multiple bridge processes from using the same WhatsApp session,
//! which would cause an endless StreamReplaced ping-pong loop.
//! The lock auto-releases on crash — no stale lock cleanup needed.

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// Holds an exclusive OS file lock for the bridge's lifetime.
/// Drop releases the lock automatically.
pub struct InstanceLock {
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Try to acquire the instance lock, retrying for up to `wait` duration.
    /// On failure, reports the PID/host/timestamp of the current owner.
    pub fn acquire(db_path: &Path, wait: Duration) -> Result<Self> {
        let lock_path = db_path.with_extension("db.instance.lock");

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open lock file {}", lock_path.display()))?;

        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    write_owner(&mut file)?;
                    return Ok(Self {
                        _file: file,
                        path: lock_path,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        let owner =
                            read_owner(&mut file).unwrap_or_else(|_| "unknown".to_string());
                        bail!(
                            "another bridge instance is running (lock: {})\n  owner: {}",
                            lock_path.display(),
                            owner.trim()
                        );
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("acquire lock {}", lock_path.display()));
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn write_owner(file: &mut File) -> Result<()> {
    let host = hostname();
    let body = format!(
        "pid={}\nhost={}\nstarted_at={}\n",
        std::process::id(),
        host,
        chrono::Utc::now().to_rfc3339()
    );

    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(body.as_bytes())?;
    file.sync_data()?;
    Ok(())
}

fn read_owner(file: &mut File) -> std::io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut s = String::new();
    file.read_to_string(&mut s)?;
    Ok(s)
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}
