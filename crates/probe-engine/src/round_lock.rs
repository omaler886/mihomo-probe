//! Cross-process exclusion for the one thing two processes must never do at
//! the same time: rewrite `<root>/core/config.yaml` and reload the kernel.
//!
//! The API already refuses a second round in-process (`in_flight`), but that
//! flag cannot see a `probe-cli round` started by hand while the service is
//! running its scheduled one -- both would write the same config file and
//! reload the same controller, and whichever reload lands second wins.
//!
//! `create_new` is the atomic part (`O_EXCL` on POSIX, `CREATE_NEW` on NTFS).
//! Staleness is decided by age, not by probing the holder: a round cannot
//! legitimately outlive its own watchdog budget, so a lock file older than
//! that budget belongs to a process that is gone. Probing the pid would be
//! nicer on Linux and impossible to do portably in the same crate.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The lock file name under `<root>/data`.
pub const LOCK_NAME: &str = "round.lock";

/// An exclusive claim on the round's kernel config.
#[derive(Debug)]
pub struct RoundLock {
    path: PathBuf,
    pid: u32,
}

impl RoundLock {
    /// Take the round lock, stealing it only when it is older than `max_age`.
    ///
    /// Returns the reason nobody else can run when the lock is held: the
    /// caller reports it instead of pretending the round started.
    pub fn acquire(root: &Path, max_age: Duration) -> Result<Self, String> {
        let path = root.join("data").join(LOCK_NAME);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        match Self::create(&path) {
            Ok(lock) => Ok(lock),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                let holder = holder.trim().to_string();
                let age = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok());
                let stale = age.map(|age| age > max_age).unwrap_or(false);
                if !stale {
                    return Err(format!("another round is already running ({holder})"));
                }
                let _ = std::fs::remove_file(&path);
                Self::create(&path)
                    .map_err(|err| format!("cannot take the round lock {}: {err}", path.display()))
            }
            Err(err) => Err(format!(
                "cannot take the round lock {}: {err}",
                path.display()
            )),
        }
    }

    fn create(path: &Path) -> std::io::Result<Self> {
        let pid = std::process::id();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let started = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        // A body is written immediately: an empty file would make the holder
        // unidentifiable in the very message this exists to produce.
        let _ = writeln!(file, "pid={pid} started={started}");
        Ok(Self {
            path: path.to_path_buf(),
            pid,
        })
    }

    /// The file this lock owns.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RoundLock {
    fn drop(&mut self) {
        // Delete only our own lock: if it was stolen while we ran (the age
        // heuristic fired on a slow round), removing it would release someone
        // else's claim.
        let ours = std::fs::read_to_string(&self.path)
            .map(|body| body.contains(&format!("pid={}", self.pid)))
            .unwrap_or(false);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second claimant is refused and told who holds the lock.
    #[test]
    fn a_second_round_is_refused_while_the_first_holds_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let first = RoundLock::acquire(tmp.path(), Duration::from_secs(60)).expect("first lock");
        let second = RoundLock::acquire(tmp.path(), Duration::from_secs(60));
        let reason = second.expect_err("second lock must be refused");
        assert!(reason.contains("another round"), "{reason}");
        assert!(
            reason.contains(&format!("pid={}", std::process::id())),
            "{reason}"
        );
        drop(first);
        // Released: the next taker succeeds.
        assert!(RoundLock::acquire(tmp.path(), Duration::from_secs(60)).is_ok());
    }

    /// A lock older than the watchdog budget belongs to a process that is gone.
    #[test]
    fn a_stale_lock_is_stolen_and_the_fresh_one_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data").join(LOCK_NAME);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "pid=999999 started=0\n").unwrap();
        let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(two_hours_ago).unwrap();
        drop(file);

        let stolen = RoundLock::acquire(tmp.path(), Duration::from_secs(60)).expect("stale lock");
        let body = std::fs::read_to_string(stolen.path()).unwrap();
        assert!(
            body.contains(&format!("pid={}", std::process::id())),
            "{body}"
        );
        drop(stolen);
        assert!(!path.exists(), "dropping the lock removes the file");
    }
}
