//! The GloBird sync: a task inside `serve` that copies the portal's daily cost
//! and usage into the database.
//!
//! One run logs in fresh (no saved cookies), lists the account's services and,
//! for each electricity service with a smart meter:
//!
//! - **backfills** history once: windows of 31 days, walking back from today
//!   until the portal has nothing older (or three years). Progress is kept in
//!   `sync_state`, so a restart carries on where it stopped and a finished
//!   backfill is never repeated.
//! - **refreshes** the recent days: from a week before the newest complete day
//!   up to today, because GloBird can revise days after posting them.
//!
//! When it runs: shortly after startup, then daily at `sync_time`. GloBird's
//! data lags by a day or more, so if yesterday is still incomplete after a run
//! it tries again every 2 hours until 23:00. A run never logs in more than
//! once, and a captcha demand stops the retries for the day.

use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Days, LocalResult, Months, NaiveDate, NaiveTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use sqlx::SqlitePool;
use tracing::{debug, error, info, warn};

use super::client::{CaptchaRequired, Client};
use super::data::{
    CostRow, DayRecord, MAX_WINDOW_DAYS, UsageRow, group_days, parse_cost_rows, parse_usage_rows,
    split_windows,
};
use super::service::{
    DateRange, Service, cost_body, is_basic_meter, meter_list, parse_accounts, pick_meter,
    smart_read_body, text_of,
};
use crate::config::Globird;
use crate::db;

/// How long after startup the first run happens.
const STARTUP_DELAY: Duration = Duration::from_secs(30);
/// Pause before every portal call, to be polite to the portal.
const CALL_PAUSE: Duration = Duration::from_secs(1);
/// How long to wait before trying again when yesterday is still incomplete.
const RETRY_AFTER: TimeDelta = TimeDelta::hours(2);
/// Retries stop for the day once the next one would be after this local time.
const RETRY_UNTIL: NaiveTime = NaiveTime::from_hms_opt(23, 0, 0).unwrap();
/// A refresh starts this many days before the newest complete day.
const RECENT_OVERLAP_DAYS: u64 = 7;
/// How far back a refresh goes when nothing is stored yet.
const FIRST_RECENT_DAYS: u64 = 30;
/// The backfill never goes back further than this many months.
const BACKFILL_MONTHS: u32 = 36;

/// The names of the `sync_state` jobs that keep one service's backfill
/// progress. `cursor_ts` is the oldest day reached (midnight UTC of that
/// date), and the second job exists once the backfill has finished.
fn oldest_job(service_id: &str) -> String {
    format!("globird_oldest:{service_id}")
}

fn done_job(service_id: &str) -> String {
    format!("globird_backfill_done:{service_id}")
}

/// A date as the `sync_state` cursor: midnight UTC of that date, as unix seconds.
fn date_to_cursor(date: NaiveDate) -> i64 {
    date.and_time(NaiveTime::MIN).and_utc().timestamp()
}

/// The inverse of [`date_to_cursor`].
fn cursor_to_date(cursor: i64) -> NaiveDate {
    DateTime::from_timestamp(cursor, 0)
        .expect("cursor out of range")
        .date_naive()
}

/// The oldest day the backfill will ask for.
fn backfill_floor(today: NaiveDate) -> NaiveDate {
    today
        .checked_sub_months(Months::new(BACKFILL_MONTHS))
        .unwrap_or(today)
}

/// The first day a refresh asks for: a week before the newest complete day, or
/// a month back when nothing is stored. Never after `today`.
fn recent_start(latest_complete: Option<NaiveDate>, today: NaiveDate) -> NaiveDate {
    let start = match latest_complete {
        Some(date) => date - Days::new(RECENT_OVERLAP_DAYS),
        None => today - Days::new(FIRST_RECENT_DAYS),
    };
    start.min(today)
}

// ---------------------------------------------------------------------------
// When to run
// ---------------------------------------------------------------------------

/// The local time `time` on `date`. A time that does not exist that day (the
/// hour skipped when the clocks go forward) moves an hour later, and one that
/// happens twice (when they go back) means the first.
fn local_at(tz: Tz, date: NaiveDate, time: NaiveTime) -> DateTime<Tz> {
    let naive = date.and_time(time);
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(t) => t,
        LocalResult::Ambiguous(first, _second) => first,
        LocalResult::None => tz
            .from_local_datetime(&(naive + TimeDelta::hours(1)))
            .earliest()
            // Cannot happen with real zones; this only keeps the code total.
            .unwrap_or_else(|| tz.from_utc_datetime(&naive)),
    }
}

/// The first `sync_time` strictly after `now`.
pub(crate) fn next_daily(now: DateTime<Tz>, sync_time: NaiveTime) -> DateTime<Tz> {
    let tz = now.timezone();
    let today = local_at(tz, now.date_naive(), sync_time);
    if today > now {
        today
    } else {
        next_day(now, sync_time)
    }
}

/// `sync_time` on the day after `now`'s local date.
fn next_day(now: DateTime<Tz>, sync_time: NaiveTime) -> DateTime<Tz> {
    let tomorrow = now.date_naive() + Days::new(1);
    local_at(now.timezone(), tomorrow, sync_time)
}

/// When the next run should start, after a run that ended at `now`.
///
/// If yesterday is complete, that is the next `sync_time`. If not, it is 2
/// hours from now, unless that is after 23:00 (or on another day), in which
/// case it is the next `sync_time`. The 2 hours are real elapsed time, so on
/// a day the clocks change the local time moves by 1 or 3 hours.
pub fn next_run(now: DateTime<Tz>, sync_time: NaiveTime, yesterday_complete: bool) -> DateTime<Tz> {
    let daily = next_daily(now, sync_time);
    if yesterday_complete {
        return daily;
    }
    let retry = now + RETRY_AFTER;
    let retry_today = retry.date_naive() == now.date_naive() && retry.time() <= RETRY_UNTIL;
    if retry_today { retry.min(daily) } else { daily }
}

// ---------------------------------------------------------------------------
// One run
// ---------------------------------------------------------------------------

/// What one run did, for the log line.
#[derive(Debug, Default)]
struct Summary {
    /// Services that were synced (electricity with a smart meter).
    services: usize,
    /// Days saved this run.
    written: BTreeSet<NaiveDate>,
    /// The saved days that GloBird has not finished posting.
    incomplete: BTreeSet<NaiveDate>,
}

impl Summary {
    fn record(&mut self, day: &DayRecord) {
        self.written.insert(day.date);
        if day.is_complete() {
            self.incomplete.remove(&day.date);
        } else {
            self.incomplete.insert(day.date);
        }
    }

    /// The one-line summary. `newest_complete` is the newest complete day in
    /// the database, not only among this run's days.
    fn describe(&self, newest_complete: Option<NaiveDate>) -> String {
        let newest = newest_complete.map_or("none".to_string(), |d| d.to_string());
        let incomplete = if self.incomplete.is_empty() {
            "none".to_string()
        } else {
            let dates: Vec<String> = self.incomplete.iter().map(NaiveDate::to_string).collect();
            dates.join(", ")
        };
        format!(
            "GloBird sync: {} days written, newest complete day {newest}, incomplete days: {incomplete}",
            self.written.len()
        )
    }
}

/// A service to sync: the portal connection, the service and its meter.
struct Target<'a> {
    client: &'a Client,
    service: &'a Service,
    serial: String,
}

impl Target<'_> {
    /// Asks the portal for the cost rows and the smart meter reads of a date
    /// range (at most 31 days).
    async fn fetch(&self, range: DateRange) -> Result<(Vec<CostRow>, Vec<UsageRow>)> {
        tokio::time::sleep(CALL_PAUSE).await;
        let cost = self
            .client
            .post_json(
                "/api/transaction/CostDetail",
                &[],
                &cost_body(self.service, range, true),
            )
            .await?;
        tokio::time::sleep(CALL_PAUSE).await;
        let usage = self
            .client
            .post_json(
                "/api/site/accountservicetimezonesmartmeterread",
                &[("accountServiceId", self.service.id.as_str())],
                &smart_read_body(self.service, &self.serial, range),
            )
            .await?;
        Ok((parse_cost_rows(cost)?, parse_usage_rows(usage)?))
    }

    /// Fetches a range and saves its days.
    async fn fetch_and_save(
        &self,
        pool: &SqlitePool,
        range: DateRange,
        summary: &mut Summary,
    ) -> Result<()> {
        let (cost, usage) = self.fetch(range).await?;
        save_days(pool, &group_days(&cost, &usage), summary).await
    }
}

/// Saves days, one transaction each, and notes them in the summary.
async fn save_days(pool: &SqlitePool, days: &[DayRecord], summary: &mut Summary) -> Result<()> {
    let fetched_at = db::now();
    for day in days {
        db::save_globird_day(pool, day, fetched_at).await?;
        summary.record(day);
    }
    Ok(())
}

/// Walks back from today in 31-day windows until the portal returns no cost
/// rows, or three years are covered. Does nothing once it has finished.
///
/// The oldest day reached is saved after every window, so an interrupted
/// backfill (an error, a restart) resumes instead of starting over.
async fn backfill(
    target: &Target<'_>,
    pool: &SqlitePool,
    today: NaiveDate,
    summary: &mut Summary,
) -> Result<()> {
    let id = &target.service.id;
    if db::get_cursor(pool, &done_job(id)).await?.is_some() {
        return Ok(());
    }
    let floor = backfill_floor(today);
    let mut end = match db::get_cursor(pool, &oldest_job(id)).await? {
        Some(cursor) => cursor_to_date(cursor) - Days::new(1),
        None => today,
    };
    info!(until = %end, "GloBird backfill started or resumed");

    loop {
        if end < floor {
            info!("GloBird backfill finished: reached the {BACKFILL_MONTHS}-month limit");
            break;
        }
        let from = (end - Days::new(MAX_WINDOW_DAYS - 1)).max(floor);
        let range = DateRange { from, to: end };
        let (cost, usage) = target.fetch(range).await?;
        if cost.is_empty() {
            info!("GloBird backfill finished: no history before {end}");
            break;
        }
        save_days(pool, &group_days(&cost, &usage), summary).await?;
        db::set_cursor(pool, &oldest_job(id), date_to_cursor(from)).await?;
        debug!(%from, to = %end, "GloBird backfill window saved");
        end = from - Days::new(1);
    }
    // `1` only has to exist; the row's presence is the marker.
    db::set_cursor(pool, &done_job(id), 1).await
}

/// Fetches the recent days again and saves them over the stored ones.
async fn refresh_recent(
    target: &Target<'_>,
    pool: &SqlitePool,
    today: NaiveDate,
    summary: &mut Summary,
) -> Result<()> {
    let latest = db::latest_complete_globird_date(pool).await?;
    for range in split_windows(recent_start(latest, today), today) {
        target.fetch_and_save(pool, range, summary).await?;
    }
    Ok(())
}

/// Finds the serial of the service's smart meter. `None` for a service with
/// no meter or only a Basic one (no five-minute data, so nothing to sync).
async fn smart_meter_serial(client: &Client, service: &Service) -> Result<Option<String>> {
    tokio::time::sleep(CALL_PAUSE).await;
    let meters = client
        .get_json(
            "/api/site/readmeters",
            &[("accountServiceId", service.id.as_str())],
        )
        .await
        .context("could not list the meters")?;
    let meters = meter_list(&meters);
    Ok(pick_meter(&meters, service.nmi.as_deref())
        .filter(|meter| !is_basic_meter(meter))
        .and_then(|meter| meter.get("serialNumber"))
        .and_then(text_of))
}

/// One whole run: login, then every syncable service. Days saved before an
/// error stay saved, and `summary` shows them either way.
async fn sync_all(
    config: &Globird,
    pool: &SqlitePool,
    tz: Tz,
    summary: &mut Summary,
) -> Result<()> {
    let today = Utc::now().with_timezone(&tz).date_naive();
    // A new client for every run, so there are no old cookies to go stale.
    let client = Client::new(config.clone())?;
    client.login().await?;

    tokio::time::sleep(CALL_PAUSE).await;
    let current_user = client
        .get_json("/api/account/currentuser", &[])
        .await
        .context("could not read the account list after logging in")?;
    let accounts = parse_accounts(&current_user);

    for service in accounts.iter().flat_map(|account| &account.services) {
        if service.is_closed || !service.is_power {
            debug!(service = %service.id, "skipping a closed or non-electricity service");
            continue;
        }
        let Some(serial) = smart_meter_serial(&client, service).await? else {
            info!(service = %service.id, "skipping a service with no smart meter");
            continue;
        };
        let target = Target {
            client: &client,
            service,
            serial,
        };
        backfill(&target, pool, today, summary).await?;
        refresh_recent(&target, pool, today, summary).await?;
        summary.services += 1;
    }
    Ok(())
}

/// How a run ended, which decides when the next one is.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// The run worked. `yesterday_complete` says whether to stop retrying.
    Done { yesterday_complete: bool },
    /// The portal wants a captcha. Retrying cannot help.
    Captcha,
    /// A network or portal error. Try again later.
    Failed,
}

/// Runs the sync once and logs how it went.
async fn run_once(config: &Globird, pool: &SqlitePool, tz: Tz) -> Outcome {
    let mut summary = Summary::default();
    let result = sync_all(config, pool, tz, &mut summary).await;

    let newest = db::latest_complete_globird_date(pool)
        .await
        .unwrap_or_else(|err| {
            warn!("could not read the newest complete GloBird day: {err:#}");
            None
        });
    if let Err(err) = result {
        // `downcast_ref` finds the captcha error even under added context.
        if err.downcast_ref::<CaptchaRequired>().is_some() {
            error!(
                "GloBird wants a captcha, so the sync cannot log in. Log in once in a browser \
                 at myaccount.globirdenergy.com.au, then it will work again. Not trying again \
                 until tomorrow."
            );
            return Outcome::Captcha;
        }
        warn!("GloBird sync failed: {err:#}; {}", summary.describe(newest));
        return Outcome::Failed;
    }

    info!("{}", summary.describe(newest));
    if summary.services == 0 {
        // Retrying cannot create a service; wait for the next daily run.
        warn!("GloBird sync found no electricity service with a smart meter");
        return Outcome::Done {
            yesterday_complete: true,
        };
    }
    let yesterday = Utc::now().with_timezone(&tz).date_naive() - Days::new(1);
    let yesterday_complete = db::globird_day_is_complete(pool, yesterday)
        .await
        .unwrap_or_else(|err| {
            warn!("could not check yesterday's GloBird data: {err:#}");
            false
        });
    Outcome::Done { yesterday_complete }
}

/// Keeps the GloBird tables up to date until `shutdown` completes.
///
/// Portal trouble never ends the loop: it is logged and retried later. Only
/// a bad `sync_time` returns `Err`, before anything starts. Stopping in the
/// middle of a run is safe, because each day is saved in one transaction.
pub async fn run(
    config: Globird,
    pool: SqlitePool,
    tz: Tz,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let sync_time = config.sync_time_of_day()?;

    // Pinning lets `select!` poll the same future in every loop round.
    tokio::pin!(shutdown);
    info!(sync_time = %sync_time.format("%H:%M"), "GloBird sync started");
    let mut wait = STARTUP_DELAY;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(wait) => {}
        }
        let outcome = tokio::select! {
            _ = &mut shutdown => break,
            outcome = run_once(&config, &pool, tz) => outcome,
        };

        let now = Utc::now().with_timezone(&tz);
        let next = match outcome {
            Outcome::Done { yesterday_complete } => next_run(now, sync_time, yesterday_complete),
            Outcome::Failed => next_run(now, sync_time, false),
            Outcome::Captcha => next_day(now, sync_time),
        };
        info!(at = %next.format("%Y-%m-%d %H:%M %Z"), "next GloBird sync");
        wait = (next - now).to_std().unwrap_or(Duration::ZERO);
    }
    info!("GloBird sync stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Australia::Adelaide;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn time(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    /// A local Adelaide time.
    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Tz> {
        Adelaide
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap()
    }

    /// "2026-10-04 09:00" style text of a local time, to compare readably.
    fn show(t: DateTime<Tz>) -> String {
        t.format("%Y-%m-%d %H:%M %z").to_string()
    }

    #[test]
    fn complete_yesterday_waits_for_the_next_sync_time() {
        let nine = time(9, 0);
        // Before today's sync time: today. After it: tomorrow.
        assert_eq!(
            show(next_run(at(2026, 10, 22, 8, 0), nine, true)),
            "2026-10-22 09:00 +1030"
        );
        assert_eq!(
            show(next_run(at(2026, 10, 22, 9, 5), nine, true)),
            "2026-10-23 09:00 +1030"
        );
        // Exactly at the sync time counts as already done.
        assert_eq!(
            show(next_run(at(2026, 10, 22, 9, 0), nine, true)),
            "2026-10-23 09:00 +1030"
        );
    }

    #[test]
    fn incomplete_yesterday_retries_every_two_hours_until_23() {
        let nine = time(9, 0);
        assert_eq!(
            show(next_run(at(2026, 10, 22, 9, 1), nine, false)),
            "2026-10-22 11:01 +1030"
        );
        // 21:00 + 2h = 23:00 is still allowed; 21:30 + 2h is not.
        assert_eq!(
            show(next_run(at(2026, 10, 22, 21, 0), nine, false)),
            "2026-10-22 23:00 +1030"
        );
        assert_eq!(
            show(next_run(at(2026, 10, 22, 21, 30), nine, false)),
            "2026-10-23 09:00 +1030"
        );
        // A retry never runs past midnight into the next day.
        assert_eq!(
            show(next_run(at(2026, 10, 22, 23, 30), nine, false)),
            "2026-10-23 09:00 +1030"
        );
    }

    #[test]
    fn a_retry_never_skips_a_sync_time() {
        // The startup run at 08:00 finds yesterday missing: 10:00 would be
        // later than the 09:00 sync, so 09:00 it is.
        assert_eq!(
            show(next_run(at(2026, 10, 22, 8, 0), time(9, 0), false)),
            "2026-10-22 09:00 +1030"
        );
    }

    #[test]
    fn a_captcha_waits_for_the_next_days_sync_time() {
        assert_eq!(
            show(next_day(at(2026, 10, 22, 9, 5), time(9, 0))),
            "2026-10-23 09:00 +1030"
        );
        assert_eq!(
            show(next_day(at(2026, 10, 22, 0, 30), time(9, 0))),
            "2026-10-23 09:00 +1030"
        );
    }

    #[test]
    fn schedule_handles_the_october_clock_change() {
        // Clocks go forward at 02:00 on Sunday 2026-10-04 (+0930 to +1030).
        let nine = time(9, 0);
        let before = at(2026, 10, 3, 22, 0);
        let next = next_run(before, nine, true);
        assert_eq!(show(next), "2026-10-04 09:00 +1030");
        // 22:00 to 09:00 is 11 hours on the clock but only 10 real hours.
        assert_eq!(next - before, TimeDelta::hours(10));

        // Two real hours after 00:30 is 03:30 on the new clock, not 02:30,
        // which does not exist that day.
        assert_eq!(
            show(next_run(at(2026, 10, 4, 0, 30), nine, false)),
            "2026-10-04 03:30 +1030"
        );
        // A sync time inside the skipped hour moves an hour later.
        assert_eq!(
            show(next_run(at(2026, 10, 3, 22, 0), time(2, 30), true)),
            "2026-10-04 03:30 +1030"
        );
        // The day after is back to normal.
        assert_eq!(
            show(next_run(at(2026, 10, 4, 9, 5), nine, true)),
            "2026-10-05 09:00 +1030"
        );
    }

    #[test]
    fn schedule_handles_the_april_clock_change() {
        // Clocks go back at 03:00 on Sunday 2026-04-05 (+1030 to +0930).
        let nine = time(9, 0);
        let before = at(2026, 4, 4, 22, 0);
        let next = next_run(before, nine, true);
        assert_eq!(show(next), "2026-04-05 09:00 +0930");
        // 11 hours on the clock, 12 real hours.
        assert_eq!(next - before, TimeDelta::hours(12));

        // 01:30 (still +1030) plus 2 real hours lands on 02:30 (+0930),
        // after the clocks went back.
        let retry = next_run(at(2026, 4, 5, 1, 30), nine, false);
        assert_eq!(show(retry), "2026-04-05 02:30 +0930");
        assert_eq!(retry - at(2026, 4, 5, 1, 30), TimeDelta::hours(2));
        // A sync time in the repeated hour means its first occurrence.
        assert_eq!(
            show(next_run(at(2026, 4, 4, 22, 0), time(2, 30), true)),
            "2026-04-05 02:30 +1030"
        );
    }

    #[test]
    fn summary_line_lists_days_written_and_incomplete() {
        let mut summary = Summary::default();
        assert_eq!(
            summary.describe(None),
            "GloBird sync: 0 days written, newest complete day none, incomplete days: none"
        );

        let day = |d: u32, categories: &[&str]| {
            let rows: Vec<CostRow> = categories
                .iter()
                .map(|c| CostRow {
                    date: date(2026, 10, d),
                    category: c.to_string(),
                    amount_cents: 1,
                    quantity: 1.0,
                    meter_read_missing: false,
                })
                .collect();
            group_days(&rows, &[]).remove(0)
        };
        summary.record(&day(20, &["SUPPLY", "USAGE"]));
        summary.record(&day(21, &["SUPPLY"]));
        summary.record(&day(22, &["SUPPLY"]));
        // A day saved twice counts once, and its latest state wins.
        summary.record(&day(22, &["SUPPLY", "USAGE"]));
        assert_eq!(
            summary.describe(Some(date(2026, 10, 22))),
            "GloBird sync: 3 days written, newest complete day 2026-10-22, \
             incomplete days: 2026-10-21"
        );
    }

    #[test]
    fn refresh_starts_a_week_before_the_newest_complete_day() {
        let today = date(2026, 10, 22);
        assert_eq!(
            recent_start(Some(date(2026, 10, 21)), today),
            date(2026, 10, 14)
        );
        assert_eq!(recent_start(None, today), date(2026, 9, 22));
        // Never later than today, even if the stored data looks odd.
        assert_eq!(recent_start(Some(date(2026, 12, 1)), today), today);
    }

    #[test]
    fn backfill_state_helpers() {
        let d = date(2026, 5, 26);
        assert_eq!(cursor_to_date(date_to_cursor(d)), d);
        assert_eq!(date_to_cursor(date(1970, 1, 2)), 86_400);
        assert_eq!(backfill_floor(date(2026, 10, 22)), date(2023, 10, 22));
        assert_ne!(oldest_job("1"), done_job("1"));
        assert_ne!(oldest_job("1"), oldest_job("2"));
    }

    #[tokio::test]
    async fn stops_promptly_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let config = Globird {
            email: "a@b.c".into(),
            password: "pw".into(),
            sync_time: "09:00".into(),
        };
        // Shutdown arrives during the 30 s startup wait.
        let shutdown = tokio::time::sleep(Duration::from_millis(200));
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run(config, pool, Adelaide, shutdown),
        )
        .await;
        assert!(result.expect("did not stop on shutdown").is_ok());
    }

    #[tokio::test]
    async fn a_bad_sync_time_fails_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let config = Globird {
            email: "a@b.c".into(),
            password: "pw".into(),
            sync_time: "9am".into(),
        };
        let result = run(config, pool, Adelaide, std::future::pending()).await;
        assert!(result.is_err());
    }
}
