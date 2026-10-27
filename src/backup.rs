//! Nightly database backups: a task inside `serve` that copies the database to
//! `solar-YYYY-MM-DD.db` files and keeps only the newest few.
//!
//! The copy is made with SQLite's `VACUUM INTO`, which is safe while the
//! database is in use (the database uses WAL) and gives a compact, complete
//! file. It is written under a `.tmp` name first and renamed when finished, so
//! a crash never leaves a half-written `.db` that looks like a good backup.
//!
//! When it runs: once at startup if today's backup is missing, then daily at
//! 03:30 local time. Errors are logged and never stop `serve`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::globird::sync::next_daily;

/// The local time of the daily backup.
const BACKUP_TIME: NaiveTime = NaiveTime::from_hms_opt(3, 30, 0).unwrap();

/// The file name of the backup for `date`, e.g. `solar-2026-10-26.db`.
fn file_name(date: NaiveDate) -> String {
    format!("solar-{}.db", date.format("%Y-%m-%d"))
}

/// The date in a backup file name, or `None` for any other file (such as a
/// `pre-update-*.db` made by update.sh), which pruning must leave alone.
fn date_of(name: &str) -> Option<NaiveDate> {
    let text = name.strip_prefix("solar-")?.strip_suffix(".db")?;
    let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    // Reject lookalikes such as `solar-2026-1-2.db`.
    (file_name(date) == name).then_some(date)
}

/// Writes the backup for `date` into `dir` (created if missing) and returns
/// its path. An existing backup for the same date is replaced.
pub async fn backup_now(pool: &SqlitePool, dir: &Path, date: NaiveDate) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("could not create backup directory {}", dir.display()))?;
    let done = dir.join(file_name(date));
    let tmp = dir.join(format!("{}.tmp", file_name(date)));
    // `VACUUM INTO` refuses to write over a file, so clear a leftover first.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context(format!("could not remove {}", tmp.display())),
    }
    let tmp_text = tmp.to_str().context("backup path is not valid UTF-8")?;
    sqlx::query("VACUUM INTO ?")
        .bind(tmp_text)
        .execute(pool)
        .await
        .with_context(|| format!("could not write backup {}", tmp.display()))?;
    std::fs::rename(&tmp, &done)
        .with_context(|| format!("could not move the backup to {}", done.display()))?;
    Ok(done)
}

/// The newest backup in `dir` (by the date in its name) and its path, or
/// `None` when there is none or the directory does not exist yet. Only files
/// named `solar-YYYY-MM-DD.db` count, like in [`prune`].
pub fn latest(dir: &Path) -> Result<Option<(NaiveDate, PathBuf)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("could not read backup directory {}", dir.display()));
        }
    };
    let mut newest: Option<(NaiveDate, PathBuf)> = None;
    for entry in entries {
        let entry = entry?;
        if let Some(date) = entry.file_name().to_str().and_then(date_of)
            && newest.as_ref().is_none_or(|(best, _)| date > *best)
        {
            newest = Some((date, entry.path()));
        }
    }
    Ok(newest)
}

/// Deletes the oldest backups in `dir` so that only `keep` remain, and returns
/// the paths it deleted. Only files named `solar-YYYY-MM-DD.db` count.
pub fn prune(dir: &Path, keep: usize) -> Result<Vec<PathBuf>> {
    let mut backups = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("could not read backup directory {}", dir.display()))?
    {
        let entry = entry?;
        if let Some(date) = entry.file_name().to_str().and_then(date_of) {
            backups.push((date, entry.path()));
        }
    }
    // Oldest first, so the ones to delete are at the front.
    backups.sort();
    let surplus = backups.len().saturating_sub(keep);
    let mut deleted = Vec::new();
    for (_, path) in backups.into_iter().take(surplus) {
        std::fs::remove_file(&path)
            .with_context(|| format!("could not delete old backup {}", path.display()))?;
        deleted.push(path);
    }
    Ok(deleted)
}

/// One backup plus pruning. Errors are logged, not returned.
async fn run_once(pool: &SqlitePool, dir: &Path, keep: usize, tz: Tz) {
    let today = Utc::now().with_timezone(&tz).date_naive();
    match backup_now(pool, dir, today).await {
        Ok(path) => info!(path = %path.display(), "database backup written"),
        Err(err) => {
            error!("database backup failed: {err:#}");
            return;
        }
    }
    match prune(dir, keep) {
        Ok(deleted) => {
            for path in deleted {
                info!(path = %path.display(), "old database backup deleted");
            }
        }
        Err(err) => warn!("could not delete old backups: {err:#}"),
    }
}

/// Backs up the database at startup (if today's backup is missing), then every
/// day at 03:30 in `tz`, until `shutdown` completes. With `keep` 0 it does
/// nothing. Failures are logged and the loop carries on.
pub async fn run(
    pool: SqlitePool,
    dir: PathBuf,
    keep: usize,
    tz: Tz,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    if keep == 0 {
        info!("database backups are off (backup_keep = 0)");
        return Ok(());
    }
    // Pinning lets `select!` poll the same future in every loop round.
    tokio::pin!(shutdown);
    info!(dir = %dir.display(), keep, "database backups started");

    let today = Utc::now().with_timezone(&tz).date_naive();
    if !dir.join(file_name(today)).exists() {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = run_once(&pool, &dir, keep, tz) => {}
        }
    }
    loop {
        let now: DateTime<Tz> = Utc::now().with_timezone(&tz);
        let next = next_daily(now, BACKUP_TIME);
        info!(at = %next.format("%Y-%m-%d %H:%M %Z"), "next database backup");
        let wait = (next - now).to_std().unwrap_or(Duration::ZERO);
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(wait) => {}
        }
        tokio::select! {
            _ = &mut shutdown => break,
            _ = run_once(&pool, &dir, keep, tz) => {}
        }
    }
    info!("database backups stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn names_round_trip_and_odd_files_are_ignored() {
        let d = date(2026, 10, 6);
        assert_eq!(file_name(d), "solar-2026-10-06.db");
        assert_eq!(date_of("solar-2026-10-06.db"), Some(d));
        assert_eq!(date_of("solar-2026-10-06.db.tmp"), None);
        assert_eq!(date_of("solar-2026-1-6.db"), None);
        assert_eq!(date_of("pre-update-20261006-010203.db"), None);
        assert_eq!(date_of("solar.db"), None);
    }

    #[test]
    fn latest_finds_the_newest_valid_backup() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(latest(&dir.path().join("missing")).unwrap(), None);
        assert_eq!(latest(dir.path()).unwrap(), None);
        for name in [
            "solar-2026-10-03.db",
            "solar-2026-10-05.db",
            "solar-2026-10-04.db",
            "solar-2026-10-09.db.tmp",
            "pre-update-20261020-000000.db",
        ] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        assert_eq!(
            latest(dir.path()).unwrap(),
            Some((date(2026, 10, 5), dir.path().join("solar-2026-10-05.db")))
        );
    }

    #[test]
    fn prune_keeps_the_newest_and_leaves_other_files() {
        let dir = tempfile::tempdir().unwrap();
        for day in 1..=5 {
            std::fs::write(dir.path().join(file_name(date(2026, 10, day))), "x").unwrap();
        }
        std::fs::write(dir.path().join("pre-update-20260101-000000.db"), "x").unwrap();

        let deleted = prune(dir.path(), 2).unwrap();
        assert_eq!(deleted.len(), 3);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "pre-update-20260101-000000.db",
                "solar-2026-10-04.db",
                "solar-2026-10-05.db"
            ]
        );
        // Nothing more to delete.
        assert!(prune(dir.path(), 2).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_backup_opens_and_has_the_tables() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("solar.db").to_str().unwrap())
            .await
            .unwrap();
        sqlx::query("INSERT INTO point (point_id, code) VALUES (7, 'p7')")
            .execute(&pool)
            .await
            .unwrap();

        // The directory does not exist yet: backup_now creates it.
        let backups = dir.path().join("backups");
        let path = backup_now(&pool, &backups, date(2026, 10, 26))
            .await
            .unwrap();
        assert_eq!(path, backups.join("solar-2026-10-26.db"));
        // No temporary file is left behind.
        assert!(!backups.join("solar-2026-10-26.db.tmp").exists());

        let copy = db::open(path.to_str().unwrap()).await.unwrap();
        let (code,): (String,) = sqlx::query_as("SELECT code FROM point WHERE point_id = 7")
            .fetch_one(&copy)
            .await
            .unwrap();
        assert_eq!(code, "p7");

        // A second backup on the same day replaces the first.
        backup_now(&pool, &backups, date(2026, 10, 26))
            .await
            .unwrap();
    }
}
