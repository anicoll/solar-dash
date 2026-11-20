//! Nightly database backups: a task inside `serve` that copies the database to
//! gzip-compressed `solar-YYYY-MM-DD.db.gz` files and keeps only the newest few.
//! (Older uncompressed `solar-YYYY-MM-DD.db` files are still recognised, so
//! they are pruned like the rest.)
//!
//! The copy is made with SQLite's `VACUUM INTO`, which is safe while the
//! database is in use (the database uses WAL) and gives a compact, complete
//! file. That raw copy goes to a `.tmp` file, which is then gzipped to another
//! `.tmp` file and renamed when finished, so a crash never leaves a
//! half-written file that looks like a good backup. The raw copy is always
//! deleted afterwards.
//!
//! Before writing, it checks that the backup disk has room for the copy plus
//! `backup_min_free_mb` of headroom; if not, that night's backup is skipped
//! with a warning (old backups are still pruned).
//!
//! When it runs: once at startup if today's backup is missing, then daily at
//! 03:30 local time. Errors are logged and never stop `serve`.
//!
//! With `[database.offsite]` configured, each new backup is also uploaded to
//! the bucket (see `offsite.rs`), and at startup the newest local backup is
//! uploaded if the bucket does not have it yet.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::globird::sync::next_daily;
use crate::offsite::Offsite;

/// The local time of the daily backup.
const BACKUP_TIME: NaiveTime = NaiveTime::from_hms_opt(3, 30, 0).unwrap();

/// The file name of the backup for `date`, e.g. `solar-2026-10-26.db.gz`.
fn file_name(date: NaiveDate) -> String {
    format!("solar-{}.db.gz", date.format("%Y-%m-%d"))
}

/// The name older versions used for the same backup (not compressed).
fn legacy_file_name(date: NaiveDate) -> String {
    format!("solar-{}.db", date.format("%Y-%m-%d"))
}

/// The date in a backup file name (`solar-YYYY-MM-DD.db.gz`, or the older
/// `solar-YYYY-MM-DD.db`), or `None` for any other file (such as a
/// `pre-update-*.db` made by update.sh), which pruning must leave alone.
pub(crate) fn date_of(name: &str) -> Option<NaiveDate> {
    let text = name.strip_prefix("solar-")?;
    let text = text
        .strip_suffix(".db.gz")
        .or_else(|| text.strip_suffix(".db"))?;
    let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    // Reject lookalikes such as `solar-2026-1-2.db`.
    (file_name(date) == name || legacy_file_name(date) == name).then_some(date)
}

/// Whether the disk has room for a backup: `free` bytes must cover the raw
/// copy (`db_bytes`), the gzip output written while it exists (taken as half
/// of that, a safe over-estimate), and `min_free_mb` of headroom left over.
fn enough_space(free: u64, db_bytes: u64, min_free_mb: u64) -> bool {
    let needed = db_bytes
        .saturating_add(db_bytes / 2)
        .saturating_add(min_free_mb.saturating_mul(1024 * 1024));
    free >= needed
}

/// The database size in bytes, from SQLite's page count and page size.
async fn db_size(pool: &SqlitePool) -> Result<u64> {
    let (pages,): (i64,) = sqlx::query_as("PRAGMA page_count").fetch_one(pool).await?;
    let (page_size,): (i64,) = sqlx::query_as("PRAGMA page_size").fetch_one(pool).await?;
    Ok(u64::try_from(pages).unwrap_or(0) * u64::try_from(page_size).unwrap_or(0))
}

/// Bytes available to us on the filesystem that holds `dir`.
fn free_bytes(dir: &Path) -> Result<u64> {
    let stat = rustix::fs::statvfs(dir)
        .with_context(|| format!("could not check free space in {}", dir.display()))?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// Gzips `src` into `dst` and syncs it to disk.
fn gzip_file(src: &Path, dst: &Path) -> Result<()> {
    let mut input =
        std::fs::File::open(src).with_context(|| format!("could not open {}", src.display()))?;
    let output = std::fs::File::create(dst)
        .with_context(|| format!("could not create {}", dst.display()))?;
    let mut encoder = flate2::write::GzEncoder::new(
        std::io::BufWriter::new(output),
        flate2::Compression::default(),
    );
    std::io::copy(&mut input, &mut encoder).context("could not compress the backup")?;
    let writer = encoder.finish().context("could not finish the backup")?;
    let file = writer.into_inner().map_err(|err| err.into_error())?;
    file.sync_all().context("could not sync the backup")?;
    Ok(())
}

/// Deletes `path`, ignoring a file that is already gone.
fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("could not remove {}", path.display())),
    }
}

/// Writes the backup for `date` into `dir` (created if missing) and returns
/// its path. An existing backup for the same date is replaced.
pub async fn backup_now(pool: &SqlitePool, dir: &Path, date: NaiveDate) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("could not create backup directory {}", dir.display()))?;
    let done = dir.join(file_name(date));
    let raw = dir.join(format!("{}.tmp", legacy_file_name(date)));
    let zipped = dir.join(format!("{}.tmp", file_name(date)));
    // `VACUUM INTO` refuses to write over a file, so clear a leftover first.
    remove_if_exists(&raw)?;
    let result = write_gz(pool, &raw, &zipped, &done).await;
    // The raw copy is never kept, and neither is a half-written gzip.
    let _ = remove_if_exists(&raw);
    if result.is_err() {
        let _ = remove_if_exists(&zipped);
    }
    result?;
    // A backup of the same day from an older version is now superseded.
    remove_if_exists(&dir.join(legacy_file_name(date)))?;
    Ok(done)
}

/// `VACUUM INTO` `raw`, gzip that to `zipped`, then rename it to `done`.
async fn write_gz(pool: &SqlitePool, raw: &Path, zipped: &Path, done: &Path) -> Result<()> {
    let raw_text = raw.to_str().context("backup path is not valid UTF-8")?;
    sqlx::query("VACUUM INTO ?")
        .bind(raw_text)
        .execute(pool)
        .await
        .with_context(|| format!("could not write backup {}", raw.display()))?;
    let (src, dst) = (raw.to_path_buf(), zipped.to_path_buf());
    tokio::task::spawn_blocking(move || gzip_file(&src, &dst))
        .await
        .context("the compression task failed")??;
    std::fs::rename(zipped, done)
        .with_context(|| format!("could not move the backup to {}", done.display()))?;
    Ok(())
}

/// The newest backup in `dir` (by the date in its name) and its path, or
/// `None` when there is none or the directory does not exist yet. Only files
/// named like a backup (see [`date_of`]) count, like in [`prune`].
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
            && newest.as_ref().is_none_or(|(best_date, best_path)| {
                // Same day (an old `.db` next to a `.db.gz`): the larger name wins.
                (date, entry.path()) > (*best_date, best_path.clone())
            })
        {
            newest = Some((date, entry.path()));
        }
    }
    Ok(newest)
}

/// Deletes the oldest backups in `dir` so that only `keep` remain, and returns
/// the paths it deleted. Only files named like a backup (see [`date_of`]) count.
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

/// Logs what an offsite upload did (or why it failed).
fn log_offsite(result: Result<crate::offsite::Synced>) {
    match result {
        Ok(synced) => {
            info!(key = %synced.key, copies = synced.count, "offsite backup uploaded");
            for key in synced.deleted {
                info!(%key, "old offsite backup deleted");
            }
        }
        Err(err) => warn!("offsite backup failed: {err:#}"),
    }
}

/// One backup, local pruning, then the offsite upload. Errors are logged, not
/// returned. If the disk is too full the backup is skipped, but pruning and the
/// offsite catch-up still happen.
async fn run_once(
    pool: &SqlitePool,
    dir: &Path,
    keep: usize,
    min_free_mb: u64,
    tz: Tz,
    offsite: Option<&Offsite>,
) {
    let today = Utc::now().with_timezone(&tz).date_naive();
    let written = match check_space(pool, dir, min_free_mb).await {
        Ok(true) => match backup_now(pool, dir, today).await {
            Ok(path) => {
                info!(path = %path.display(), "database backup written");
                Some(path)
            }
            Err(err) => {
                error!("database backup failed: {err:#}");
                return;
            }
        },
        Ok(false) => None,
        Err(err) => {
            error!("database backup failed: {err:#}");
            return;
        }
    };
    match prune(dir, keep) {
        Ok(deleted) => {
            for path in deleted {
                info!(path = %path.display(), "old database backup deleted");
            }
        }
        Err(err) => warn!("could not delete old backups: {err:#}"),
    }
    if let Some(offsite) = offsite {
        match written {
            Some(path) => log_offsite(offsite.upload_and_prune(&path).await),
            None => catch_up(dir, offsite).await,
        }
    }
}

/// Whether there is room to write a backup into `dir`; logs a warning when not.
async fn check_space(pool: &SqlitePool, dir: &Path, min_free_mb: u64) -> Result<bool> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("could not create backup directory {}", dir.display()))?;
    let db_bytes = db_size(pool).await.context("could not size the database")?;
    let free = free_bytes(dir)?;
    if enough_space(free, db_bytes, min_free_mb) {
        return Ok(true);
    }
    let mb = |bytes: u64| bytes / (1024 * 1024);
    let needed = db_bytes
        .saturating_add(db_bytes / 2)
        .saturating_add(min_free_mb.saturating_mul(1024 * 1024));
    warn!(
        free_mb = mb(free),
        needed_mb = mb(needed),
        "not enough free disk space, skipping the database backup"
    );
    Ok(false)
}

/// At startup: uploads the newest local backup if the bucket lacks it, for
/// example because the last upload failed.
async fn catch_up(dir: &Path, offsite: &Offsite) {
    match latest(dir) {
        Ok(Some((_, path))) => match offsite.upload_if_missing(&path).await {
            Ok(Some(synced)) => log_offsite(Ok(synced)),
            Ok(None) => info!("the newest backup is already offsite"),
            Err(err) => warn!("offsite backup failed: {err:#}"),
        },
        Ok(None) => {}
        Err(err) => warn!("could not look for the newest backup: {err:#}"),
    }
}

/// Backs up the database at startup (if today's backup is missing), then every
/// day at 03:30 in `tz`, until `shutdown` completes. With `keep` 0 it does
/// nothing. Failures are logged and the loop carries on. With `offsite` set,
/// backups are also uploaded. `min_free_mb` is the free space to leave on the
/// disk (see [`enough_space`]).
pub async fn run(
    pool: SqlitePool,
    dir: PathBuf,
    keep: usize,
    min_free_mb: u64,
    tz: Tz,
    offsite: Option<Offsite>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    if keep == 0 {
        info!("database backups are off (backup_keep = 0)");
        return Ok(());
    }
    // Pinning lets `select!` poll the same future in every loop round.
    tokio::pin!(shutdown);
    info!(dir = %dir.display(), keep, offsite = offsite.is_some(), "database backups started");

    let today = Utc::now().with_timezone(&tz).date_naive();
    if !dir.join(file_name(today)).exists() && !dir.join(legacy_file_name(today)).exists() {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = run_once(&pool, &dir, keep, min_free_mb, tz, offsite.as_ref()) => {}
        }
    }
    if let Some(offsite) = &offsite {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = catch_up(&dir, offsite) => {}
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
            _ = run_once(&pool, &dir, keep, min_free_mb, tz, offsite.as_ref()) => {}
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
        assert_eq!(file_name(d), "solar-2026-10-06.db.gz");
        assert_eq!(legacy_file_name(d), "solar-2026-10-06.db");
        assert_eq!(date_of(&file_name(d)), Some(d));
        assert_eq!(date_of(&legacy_file_name(d)), Some(d));
        assert_eq!(date_of("solar-2026-10-06.db.tmp"), None);
        assert_eq!(date_of("solar-2026-10-06.db.gz.tmp"), None);
        assert_eq!(date_of("solar-2026-1-6.db"), None);
        assert_eq!(date_of("solar-2026-1-6.db.gz"), None);
        assert_eq!(date_of("solar-2026-10-06.gz"), None);
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
            "solar-2026-10-04.db.gz",
            "solar-2026-10-09.db.tmp",
            "solar-2026-10-09.db.gz.tmp",
            "pre-update-20261020-000000.db",
        ] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        assert_eq!(
            latest(dir.path()).unwrap(),
            Some((date(2026, 10, 5), dir.path().join("solar-2026-10-05.db")))
        );
        // The same day in both forms: the compressed one wins.
        std::fs::write(dir.path().join("solar-2026-10-05.db.gz"), "x").unwrap();
        assert_eq!(
            latest(dir.path()).unwrap(),
            Some((date(2026, 10, 5), dir.path().join("solar-2026-10-05.db.gz")))
        );
    }

    #[test]
    fn prune_keeps_the_newest_and_leaves_other_files() {
        let dir = tempfile::tempdir().unwrap();
        // Old uncompressed files for days 1-3, compressed ones for 3-5.
        for day in 1..=3 {
            std::fs::write(dir.path().join(legacy_file_name(date(2026, 10, day))), "x").unwrap();
        }
        for day in 3..=5 {
            std::fs::write(dir.path().join(file_name(date(2026, 10, day))), "x").unwrap();
        }
        std::fs::write(dir.path().join("pre-update-20260101-000000.db"), "x").unwrap();

        // Six backup files: day 3 exists in both forms.
        let deleted = prune(dir.path(), 2).unwrap();
        assert_eq!(deleted.len(), 4);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "pre-update-20260101-000000.db",
                "solar-2026-10-04.db.gz",
                "solar-2026-10-05.db.gz"
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
        assert_eq!(path, backups.join("solar-2026-10-26.db.gz"));
        // No temporary file is left behind.
        let names: Vec<String> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["solar-2026-10-26.db.gz"]);

        let unzipped = dir.path().join("restored.db");
        let mut decoder = flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap());
        let mut out = std::fs::File::create(&unzipped).unwrap();
        std::io::copy(&mut decoder, &mut out).unwrap();
        drop(out);
        let copy = db::open(unzipped.to_str().unwrap()).await.unwrap();
        let (code,): (String,) = sqlx::query_as("SELECT code FROM point WHERE point_id = 7")
            .fetch_one(&copy)
            .await
            .unwrap();
        assert_eq!(code, "p7");

        // A second backup on the same day replaces the first, and an old
        // uncompressed file for that day goes away.
        std::fs::write(backups.join("solar-2026-10-26.db"), "old").unwrap();
        backup_now(&pool, &backups, date(2026, 10, 26))
            .await
            .unwrap();
        assert!(!backups.join("solar-2026-10-26.db").exists());
        assert!(backups.join("solar-2026-10-26.db.gz").exists());

        assert!(db_size(&pool).await.unwrap() > 0);
        assert!(free_bytes(&backups).unwrap() > 0);
    }

    #[test]
    fn enough_space_needs_the_copy_and_the_headroom() {
        const MB: u64 = 1024 * 1024;
        // 100 MB database: 150 MB for the copies, plus headroom.
        assert!(enough_space(150 * MB, 100 * MB, 0));
        assert!(!enough_space(150 * MB - 1, 100 * MB, 0));
        assert!(!enough_space(1100 * MB, 100 * MB, 1024));
        assert!(enough_space(1174 * MB, 100 * MB, 1024));
        // Nothing free never fits, and huge inputs do not overflow.
        assert!(!enough_space(0, 1, 0));
        assert!(!enough_space(u64::MAX - 1, u64::MAX, u64::MAX));
    }
}
