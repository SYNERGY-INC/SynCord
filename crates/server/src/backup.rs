//! Daily database backups: once a day (and at start-up if there's none from today yet)
//! the database is copied into a `backups` folder next to it with SQLite's online backup,
//! which works while the server keeps running. The newest [`KEEP`] copies are kept.
//!
//! Uploaded files aren't copied: they can be huge. See the README for backing those up.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use tracing::{info, warn};

use crate::AppState;

/// How many daily copies are kept.
pub const KEEP: usize = 7;
const PREFIX: &str = "celerity-";
const SUFFIX: &str = ".db";

/// The folder backups of `database` go into.
pub fn folder_for(database: &Path) -> PathBuf {
    database.parent().unwrap_or(Path::new(".")).join("backups")
}

/// A backup on disk, for the admin page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupFile {
    pub name: String,
    /// `YYYY-MM-DD` (UTC).
    pub date: String,
    pub size: u64,
}

/// Today's date (UTC) as `YYYY-MM-DD`.
pub fn today() -> String {
    date_of(crate::db::now_ms() / 1000)
}

/// The UTC date of a Unix time, as `YYYY-MM-DD`.
pub fn date_of(unix_secs: i64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let z = unix_secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Backups in `dir`, newest first.
pub fn list(dir: &Path) -> Vec<BackupFile> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<BackupFile> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let date = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?.to_string();
            let valid = date.len() == 10 && date.bytes().enumerate().all(|(i, b)| (i == 4 || i == 7) == (b == b'-'));
            valid.then(|| BackupFile { size: e.metadata().map(|m| m.len()).unwrap_or(0), name, date })
        })
        .collect();
    out.sort_by(|a, b| b.date.cmp(&a.date));
    out
}

/// Copy the database at `database` into `dir` as today's backup (replacing one from earlier today).
pub fn make(database: &Path, dir: &Path, date: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let target = dir.join(format!("{PREFIX}{date}{SUFFIX}"));
    let part = dir.join(format!(".{PREFIX}{date}{SUFFIX}.part"));
    let _ = std::fs::remove_file(&part);
    let result = (|| -> rusqlite::Result<()> {
        // A second, read-only connection: the server's own one keeps working meanwhile.
        let src = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut dst = Connection::open(&part)?;
        let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
        // Everything in one step: in WAL mode that doesn't hold up the server's writes, and
        // a step can't be restarted halfway by a write coming in. If the database is busy
        // for a moment, try again shortly.
        use rusqlite::backup::StepResult;
        for _ in 0..100 {
            match backup.step(-1)? {
                StepResult::Done => return Ok(()),
                StepResult::More => {}
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None))
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(format!("backup failed: {e}"));
    }
    std::fs::rename(&part, &target).map_err(|e| format!("couldn't save the backup: {e}"))?;
    Ok(target)
}

/// Delete all but the newest `keep` backups.
pub fn prune(dir: &Path, keep: usize) {
    for old in list(dir).into_iter().skip(keep) {
        if let Err(e) = std::fs::remove_file(dir.join(&old.name)) {
            warn!("couldn't delete old backup {}: {e}", old.name);
        }
    }
}

/// Make today's backup if there isn't one yet, then tidy up. Returns the new file, if one was made.
pub fn run_once(database: &Path, dir: &Path) -> Result<Option<PathBuf>, String> {
    let date = today();
    if list(dir).iter().any(|b| b.date == date) {
        return Ok(None);
    }
    let made = make(database, dir, &date)?;
    prune(dir, KEEP);
    Ok(Some(made))
}

/// Check every hour whether today's backup has been made (so it also happens at first start).
pub fn start(state: Arc<AppState>) {
    if !state.config.backups {
        info!("daily backups are off (backups = false)");
        return;
    }
    let Some(database) = state.db.path().map(Path::to_path_buf) else { return };
    let dir = folder_for(&database);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let (db, d) = (database.clone(), dir.clone());
            match tokio::task::spawn_blocking(move || run_once(&db, &d)).await {
                Ok(Ok(Some(file))) => info!("backed up the database to {}", file.display()),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => warn!("{e}"),
                Err(e) => warn!("backup task failed: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(date_of(951_782_400), "2000-02-29");
        assert_eq!(date_of(1_794_268_799), "2026-11-09");
        assert_eq!(date_of(-1), "1969-12-31");
    }

    #[test]
    fn backs_up_a_live_database_and_keeps_seven() {
        let dir = std::env::temp_dir().join(format!("celerity-backup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("celerity.db");
        let db = crate::db::Db::open(&path).unwrap();
        let backups = folder_for(&path);
        assert_eq!(backups, dir.join("backups"));

        // The server's connection stays open (and writes) while the backup runs.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(db.call(|c| crate::db::create_user(c, "alice", "x", true))).unwrap();
        let made = run_once(&path, &backups).unwrap().expect("a backup today");
        assert!(run_once(&path, &backups).unwrap().is_none(), "only once a day");
        let copy = Connection::open(&made).unwrap();
        let n: i64 = copy.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);

        // Old ones beyond seven are deleted, oldest first.
        for day in 1..=9 {
            std::fs::write(backups.join(format!("celerity-2020-01-{day:02}.db")), b"x").unwrap();
        }
        std::fs::write(backups.join("notes.txt"), b"left alone").unwrap();
        prune(&backups, KEEP);
        let left = list(&backups);
        assert_eq!(left.len(), KEEP);
        assert_eq!(left[0].date, today());
        assert_eq!(left.last().unwrap().date, "2020-01-04");
        assert!(backups.join("notes.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
