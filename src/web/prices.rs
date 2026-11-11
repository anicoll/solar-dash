//! `GET /api/prices`: stored electricity prices (see `amber::store`).

use std::collections::{BTreeMap, HashMap};

use axum::Json;
use axum::extract::{Query, State};
use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;
use super::api::{ApiError, ApiResult, day_bounds_or_error, parse_date, today};
use super::day_cache;
use crate::amber::store::{self, StoredPrice};
use crate::cost::{Interval, PriceSpan};

/// The longest range one request may ask for. Five-minute prices on three
/// channels are about 900 rows a day, so this is under 30,000 rows.
const MAX_PRICES_SECS: i64 = 31 * 24 * 3600;

#[derive(Debug, Deserialize)]
pub struct PricesParams {
    from: Option<String>,
    to: Option<String>,
    /// Only this source's prices, e.g. `amber:01F5A5CRKMZ5BCX9P1S4V990AM`.
    source: Option<String>,
}

fn parse_unix(name: &str, text: Option<&str>) -> Result<i64, ApiError> {
    text.and_then(|t| t.parse().ok())
        .ok_or_else(|| ApiError::bad_request(format!("{name} must be a unix time in seconds")))
}

/// Groups rows (already sorted by channel) into `{channel: [..]}`.
fn by_channel(rows: Vec<StoredPrice>) -> BTreeMap<String, Vec<StoredPrice>> {
    let mut grouped: BTreeMap<String, Vec<StoredPrice>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.channel.clone()).or_default().push(row);
    }
    grouped
}

/// `{"spike_cents":100,"channels":{"general":[{start,end,cents,spot_cents,kind},...],"feed_in":[...]},"now":{"general":{...}}}`.
///
/// `from` and `to` are unix seconds, at most 31 days apart; intervals that
/// start in `[from, to)` are returned, oldest first, grouped by channel
/// (`general`, `feed_in`, `controlled_load`). `kind` is `actual`, `current`
/// or `forecast`. `now` has the interval each channel is in right now, whatever
/// the range, and is `{}` when there is none. Prices are c/kWh including GST,
/// as the source gave them: the feed-in sign is not changed
/// (negative means the customer is paid; the UI flips it for display).
/// Rows also carry Amber's `descriptor` and `renewables` when known.
pub async fn prices(
    State(state): State<AppState>,
    Query(params): Query<PricesParams>,
) -> ApiResult {
    let from = parse_unix("from", params.from.as_deref())?;
    let to = parse_unix("to", params.to.as_deref())?;
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if to - from > MAX_PRICES_SECS {
        return Err(ApiError::bad_request("the range can be at most 31 days"));
    }
    let source = params.source.as_deref();

    let rows = store::prices_between(&state.pool, source, from, to).await?;
    let current = store::prices_at(&state.pool, source, Utc::now().timestamp()).await?;
    // A channel can have rows from several sources; the query lists the
    // newest start first, so the first one per channel is the one to show.
    let mut now = BTreeMap::new();
    for row in current {
        now.entry(row.channel.clone()).or_insert(row);
    }
    // The Today page colours the live price with this (the `price_spike_cents` setting).
    let spike_cents = state.admin.get().price_spike_cents;
    Ok(Json(json!({
        "channels": by_channel(rows),
        "now": now,
        "spike_cents": spike_cents,
    })))
}

/// The most days one `/api/prices/daily` request may ask for.
const MAX_DAILY_DAYS: u64 = 400;

/// The source prefix of Amber prices (see `amber::store::source_name`).
const AMBER_PREFIX: &str = "amber:";

#[derive(Debug, Deserialize)]
pub struct DailyParams {
    from: Option<String>,
    to: Option<String>,
}

/// What the buy price did on one local day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayPriceStats {
    /// The local day, `YYYY-MM-DD`.
    pub date: String,
    /// Lowest and highest buy price (c/kWh incl. GST) and the unix time of
    /// the interval that start each was seen in.
    pub min_cents: f64,
    pub min_start: i64,
    pub max_cents: f64,
    pub max_start: i64,
    /// Plain average of the day's interval prices.
    pub mean_cents: f64,
    /// Average price weighted by the grid import (kWh) in each interval:
    /// what the home actually paid per kWh. `None` without usage data.
    pub weighted_cents: Option<f64>,
    /// Minutes priced at or below 0 c/kWh.
    pub free_minutes: f64,
    /// Intervals priced above the spike setting.
    pub spike_intervals: u32,
    /// How many price intervals the day has.
    pub intervals: u32,
    /// Share of the day (0 to 1) that has a price. A 23 or 25 hour DST day
    /// is measured against its own length.
    pub coverage: f64,
}

/// Grid import (kWh) that falls inside `[start, end)`. Each usage interval
/// counts in proportion to how much of it overlaps. `usage` must be sorted
/// by start; `from` is where to start looking (it moves forward as the
/// price spans, which are also sorted, move on).
fn import_in_span(usage: &[Interval], from: &mut usize, start: i64, end: i64) -> f64 {
    // Skip usage that ended before this span; later spans start later still.
    while *from < usage.len() && usage[*from].end_ts <= start {
        *from += 1;
    }
    let mut kwh = 0.0;
    for iv in &usage[*from..] {
        if iv.start_ts >= end {
            break;
        }
        let length = (iv.end_ts - iv.start_ts).max(1) as f64;
        let overlap = (iv.end_ts.min(end) - iv.start_ts.max(start)).max(0) as f64;
        kwh += iv.import_kwh * overlap / length;
    }
    kwh
}

/// The stats of one local day `[day_start, day_end)`, or `None` when it has
/// no price rows. `prices` and `usage` are sorted by start time and may hold
/// other days too: only rows that start inside the day are used.
pub fn day_stats(
    date: NaiveDate,
    day_start: i64,
    day_end: i64,
    prices: &[PriceSpan],
    usage: &[Interval],
    spike_cents: f64,
) -> Option<DayPriceStats> {
    let first = prices.partition_point(|p| p.start_ts < day_start);
    let last = prices.partition_point(|p| p.start_ts < day_end);
    let today = &prices[first..last];
    let (&head, _) = today.split_first()?;

    let (mut min, mut max) = (head, head);
    let (mut sum, mut free_secs, mut covered_secs) = (0.0, 0_i64, 0_i64);
    let (mut spikes, mut weighted_sum, mut import_total) = (0, 0.0, 0.0);
    let mut cursor = 0;
    for span in today {
        if span.cents < min.cents {
            min = *span;
        }
        if span.cents > max.cents {
            max = *span;
        }
        sum += span.cents;
        // Clip to the day so a row that runs past midnight is not counted twice.
        let secs = span.end_ts.min(day_end) - span.start_ts;
        covered_secs += secs;
        if span.cents <= 0.0 {
            free_secs += secs;
        }
        if span.cents > spike_cents {
            spikes += 1;
        }
        let import = import_in_span(usage, &mut cursor, span.start_ts, span.end_ts);
        weighted_sum += span.cents * import;
        import_total += import;
    }
    let length = (day_end - day_start).max(1) as f64;
    Some(DayPriceStats {
        date: date.to_string(),
        min_cents: min.cents,
        min_start: min.start_ts,
        max_cents: max.cents,
        max_start: max.start_ts,
        mean_cents: sum / today.len() as f64,
        weighted_cents: (import_total > 1e-9).then(|| weighted_sum / import_total),
        free_minutes: free_secs as f64 / 60.0,
        spike_intervals: spikes,
        intervals: today.len() as u32,
        coverage: (covered_secs as f64 / length).min(1.0),
    })
}

/// Totals over all the days that have prices.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DailySummary {
    /// Days that have price rows.
    pub days: u32,
    /// Average of every interval price in the range.
    pub mean_cents: f64,
    /// Import-weighted average over the range, `None` without usage.
    pub weighted_cents: Option<f64>,
    pub min_cents: f64,
    pub min_start: i64,
    pub max_cents: f64,
    pub max_start: i64,
    pub free_minutes: f64,
    /// Days with at least one interval at or below 0 c/kWh.
    pub free_days: u32,
    pub spike_intervals: u32,
}

/// Combines the days. Averages are weighted by interval counts (the mean)
/// and by import (the weighted one) so a day with more rows counts more.
/// `weights` is each day's total import in kWh, parallel to `days`.
pub fn summarise(days: &[DayPriceStats], import_kwh: &[f64]) -> Option<DailySummary> {
    let first = days.first()?;
    let (mut min, mut max) = (
        (first.min_cents, first.min_start),
        (first.max_cents, first.max_start),
    );
    let (mut sum, mut count) = (0.0, 0.0);
    let (mut w_sum, mut w_total) = (0.0, 0.0);
    let (mut free_minutes, mut free_days, mut spikes) = (0.0, 0, 0);
    for (day, &kwh) in days.iter().zip(import_kwh) {
        if day.min_cents < min.0 {
            min = (day.min_cents, day.min_start);
        }
        if day.max_cents > max.0 {
            max = (day.max_cents, day.max_start);
        }
        sum += day.mean_cents * f64::from(day.intervals);
        count += f64::from(day.intervals);
        if let Some(w) = day.weighted_cents {
            w_sum += w * kwh;
            w_total += kwh;
        }
        free_minutes += day.free_minutes;
        free_days += u32::from(day.free_minutes > 0.0);
        spikes += day.spike_intervals;
    }
    Some(DailySummary {
        days: days.len() as u32,
        mean_cents: sum / count,
        weighted_cents: (w_total > 1e-9).then(|| w_sum / w_total),
        min_cents: min.0,
        min_start: min.1,
        max_cents: max.0,
        max_start: max.1,
        free_minutes,
        free_days,
        spike_intervals: spikes,
    })
}

/// A day's total import (kWh), counting usage that starts inside it.
fn import_total(usage: &[Interval], day_start: i64, day_end: i64) -> f64 {
    let first = usage.partition_point(|i| i.start_ts < day_start);
    let last = usage.partition_point(|i| i.start_ts < day_end);
    usage[first..last].iter().map(|i| i.import_kwh).sum()
}

/// `GET /api/prices/daily?from=YYYY-MM-DD&to=YYYY-MM-DD`: for each local day
/// (in the site time zone, both ends included, at most 400 days) that has
/// Amber buy prices, the lowest and highest price, the plain and the
/// usage-weighted average, minutes at or below 0 c, intervals above the
/// spike setting and how much of the day has a price. Only final `actual`
/// prices count, plus the interval under way today; forecasts never do.
/// `{"spike_cents":..,"days":[..],"summary":{..}|null}`; days without prices
/// are left out. The weighted average uses the same house intervals as
/// `/api/costs`; a day with no usage data has `weighted_cents: null`.
pub async fn daily(State(state): State<AppState>, Query(params): Query<DailyParams>) -> ApiResult {
    let from = parse_date("from", params.from.as_deref().unwrap_or(""))?;
    let to = parse_date("to", params.to.as_deref().unwrap_or(""))?;
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if (to - from).num_days() as u64 >= MAX_DAILY_DAYS {
        return Err(ApiError::bad_request("the range can be at most 400 days"));
    }
    let tz = state.tz;
    let today_date = today(tz);
    let spike_cents = state.admin.get().price_spike_cents;
    let dates: Vec<NaiveDate> = from.iter_days().take_while(|d| *d <= to).collect();

    // Finished days that are stored for these prices and settings are not
    // worked out again, and their samples are not read.
    let mut keys = Vec::new();
    for &date in dates.iter().filter(|d| **d < today_date) {
        let (start, end) = day_bounds_or_error(date, tz)?;
        let fingerprint =
            store::day_prices_fingerprint(&state.pool, AMBER_PREFIX, start, end, false).await?;
        keys.push((
            date,
            format!(
                "{STATS_VERSION}|{tz}|spike={}|{:?}|prices:{fingerprint}",
                spike_cents.to_bits(),
                state.rules
            ),
        ));
    }
    let mut stored: HashMap<NaiveDate, Option<StoredDay>> =
        day_cache::load(&state.pool, STATS_KIND, &keys).await?;
    let missing: Vec<NaiveDate> = dates
        .iter()
        .copied()
        .filter(|d| !stored.contains_key(d))
        .collect();

    let mut to_store: Vec<(NaiveDate, String, Option<StoredDay>)> = Vec::new();
    let key_of: HashMap<NaiveDate, &String> = keys.iter().map(|(d, k)| (*d, k)).collect();
    let (today_start, _) = day_bounds_or_error(today_date, tz)?;
    // Each run of consecutive missing days is loaded on its own, so a stored
    // day in the middle of the range is not read again.
    for run in missing.chunk_by(|a, b| a.succ_opt() == Some(*b)) {
        // Every interval lies inside its own day (see `live_intervals`), so
        // the days before the run are not needed.
        let (range_start, _) = day_bounds_or_error(run[0], tz)?;
        let (_, range_end) = day_bounds_or_error(run[run.len() - 1], tz)?;
        let prices = store::known_general_prices(
            &state.pool,
            AMBER_PREFIX,
            range_start,
            range_end,
            today_start,
        )
        .await?;
        // Usage is only needed when there is a price to weight.
        let mut usage = Vec::new();
        let mut cacheable_usage: HashMap<NaiveDate, bool> = HashMap::new();
        if !prices.is_empty() {
            let by_day = super::costs::house_intervals_by_day(
                &state.pool,
                state.plant_id,
                tz,
                range_start,
                range_end,
                Utc::now().timestamp(),
                &state.rules,
            )
            .await?;
            for day in by_day {
                cacheable_usage.insert(day.date, day.cacheable);
                usage.extend(day.intervals);
            }
        }
        usage.sort_by_key(|i| i.start_ts);
        for &date in run {
            let (start, end) = day_bounds_or_error(date, tz)?;
            let day =
                day_stats(date, start, end, &prices, &usage, spike_cents).map(|stats| StoredDay {
                    stats,
                    import_kwh: import_total(&usage, start, end),
                });
            // A day without prices stays without them; a priced day is only
            // final once its usage is (a later backfill could add to it).
            let final_usage = day.is_none() || cacheable_usage.get(&date).copied().unwrap_or(false);
            if let (Some(key), true) = (key_of.get(&date), final_usage) {
                to_store.push((date, (*key).clone(), day.clone()));
            }
            stored.insert(date, day);
        }
    }
    let rows: Vec<(NaiveDate, String, &Option<StoredDay>)> = to_store
        .iter()
        .map(|(date, key, day)| (*date, key.clone(), day))
        .collect();
    day_cache::save(&state.pool, STATS_KIND, &rows).await?;

    let (mut days, mut import_kwh) = (Vec::new(), Vec::new());
    for date in &dates {
        if let Some(Some(day)) = stored.remove(date) {
            days.push(day.stats);
            import_kwh.push(day.import_kwh);
        }
    }
    let summary = summarise(&days, &import_kwh);
    Ok(Json(json!({
        "spike_cents": spike_cents,
        "days": days,
        "summary": summary,
    })))
}

/// The `daily_cache` kind for a day's price stats.
const STATS_KIND: &str = "price_stats";

/// Bump when the stats, or how they are worked out, change.
const STATS_VERSION: u32 = 1;

/// A finished day's stats and its total import, as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredDay {
    stats: DayPriceStats,
    import_kwh: f64,
}

/// The stats of every local day from `from` to `to` that has prices, and
/// each one's total import.
#[cfg(test)]
fn days_in_range(
    from: NaiveDate,
    to: NaiveDate,
    tz: chrono_tz::Tz,
    prices: &[PriceSpan],
    usage: &[Interval],
    spike_cents: f64,
) -> Result<(Vec<DayPriceStats>, Vec<f64>), ApiError> {
    let (mut days, mut import_kwh) = (Vec::new(), Vec::new());
    let mut date = from;
    while date <= to {
        let (start, end) = day_bounds_or_error(date, tz)?;
        if let Some(stats) = day_stats(date, start, end, prices, usage, spike_cents) {
            days.push(stats);
            import_kwh.push(import_total(usage, start, end));
        }
        date = date
            .checked_add_days(chrono::Days::new(1))
            .ok_or_else(|| ApiError::bad_request("date is out of range"))?;
    }
    Ok((days, import_kwh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn span(start_ts: i64, cents: f64) -> PriceSpan {
        PriceSpan {
            start_ts,
            end_ts: start_ts + 300,
            cents,
        }
    }

    fn usage(start_ts: i64, secs: i64, import_kwh: f64) -> Interval {
        Interval {
            start_ts,
            end_ts: start_ts + secs,
            import_kwh,
            export_kwh: 0.0,
            load_kwh: 0.0,
        }
    }

    const DAY: i64 = 86_400;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn a_day_is_summarised_with_a_usage_weighted_average() {
        let prices = [
            span(0, -2.0),
            span(300, 10.0),
            span(600, 40.0),
            span(900, 10.0),
        ];
        // 1 kWh at 10 c, 3 kWh at 40 c and nothing at -2 c: (10 + 120) / 4.
        let usage = [usage(300, 300, 1.0), usage(600, 300, 3.0)];
        let stats = day_stats(date("2026-01-01"), 0, DAY, &prices, &usage, 30.0).unwrap();
        assert_eq!((stats.min_cents, stats.min_start), (-2.0, 0));
        assert_eq!((stats.max_cents, stats.max_start), (40.0, 600));
        assert_eq!(stats.mean_cents, 14.5);
        assert_eq!(stats.weighted_cents, Some(32.5));
        assert_eq!(stats.free_minutes, 5.0);
        assert_eq!(stats.spike_intervals, 1);
        assert_eq!(stats.intervals, 4);
        assert!((stats.coverage - 1200.0 / DAY as f64).abs() < 1e-9);
    }

    #[test]
    fn usage_spanning_several_prices_is_shared_by_overlap() {
        let prices = [span(0, 10.0), span(300, 30.0)];
        // One 10 minute interval: half its import in each price.
        let usage = [usage(0, 600, 2.0)];
        let stats = day_stats(date("2026-01-01"), 0, DAY, &prices, &usage, 100.0).unwrap();
        assert_eq!(stats.weighted_cents, Some(20.0));
    }

    #[test]
    fn a_day_without_usage_has_no_weighted_average() {
        let prices = [span(0, 12.0), span(300, 0.0)];
        let stats = day_stats(date("2026-01-01"), 0, DAY, &prices, &[], 100.0).unwrap();
        assert_eq!(stats.weighted_cents, None);
        assert_eq!(stats.mean_cents, 6.0);
        assert_eq!(stats.free_minutes, 5.0, "0 c counts as at or below 0");
        // Usage that is all zero is also "no usage".
        let zero = [usage(0, 600, 0.0)];
        let stats = day_stats(date("2026-01-01"), 0, DAY, &prices, &zero, 100.0).unwrap();
        assert_eq!(stats.weighted_cents, None);
    }

    #[test]
    fn a_day_with_no_prices_is_left_out() {
        let prices = [span(DAY + 10, 5.0)];
        assert!(day_stats(date("2026-01-01"), 0, DAY, &prices, &[], 30.0).is_none());
    }

    #[test]
    fn a_dst_day_is_measured_against_its_own_length() {
        // Sydney springs forward on 2026-10-04: that local day has 23 hours.
        let tz = chrono_tz::Australia::Sydney;
        let (start, end) = day_bounds_or_error(date("2026-10-04"), tz).unwrap();
        assert_eq!(end - start, 23 * 3600);
        // A price for every 5 minutes of the day.
        let prices: Vec<_> = (0..23 * 12).map(|n| span(start + n * 300, 20.0)).collect();
        // The day before has 24 hours and no rows; the day after has one row.
        let after = span(end, 99.0);
        let all: Vec<_> = prices.iter().copied().chain([after]).collect();
        let (days, _) =
            days_in_range(date("2026-10-03"), date("2026-10-05"), tz, &all, &[], 30.0).unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, "2026-10-04");
        assert_eq!(days[0].intervals, 276);
        assert!(
            (days[0].coverage - 1.0).abs() < 1e-9,
            "full 23h day is full coverage"
        );
        assert_eq!(days[1].date, "2026-10-05");
        assert_eq!(days[1].max_cents, 99.0);

        // The fall-back day (2027-04-04) has 25 hours.
        let (start, end) = day_bounds_or_error(date("2027-04-04"), tz).unwrap();
        assert_eq!(end - start, 25 * 3600);
        let local = tz.timestamp_opt(start, 0).unwrap();
        assert_eq!(local.format("%Y-%m-%d").to_string(), "2027-04-04");
        let prices: Vec<_> = (0..25 * 12).map(|n| span(start + n * 300, 1.0)).collect();
        let stats = day_stats(date("2027-04-04"), start, end, &prices, &[], 30.0).unwrap();
        assert_eq!(stats.intervals, 300);
        assert!((stats.coverage - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_summary_adds_up_the_days() {
        let prices = [
            span(0, -1.0),
            span(300, 20.0),
            span(DAY, 50.0),
            span(DAY + 300, 10.0),
        ];
        let usage = [usage(300, 300, 1.0), usage(DAY, 300, 3.0)];
        let d1 = day_stats(date("2026-01-01"), 0, DAY, &prices, &usage, 30.0).unwrap();
        let d2 = day_stats(date("2026-01-02"), DAY, 2 * DAY, &prices, &usage, 30.0).unwrap();
        let summary = summarise(&[d1, d2], &[1.0, 3.0]).unwrap();
        assert_eq!(summary.days, 2);
        assert_eq!(summary.mean_cents, 19.75);
        // (20 * 1 + 50 * 3) / 4
        assert_eq!(summary.weighted_cents, Some(42.5));
        assert_eq!((summary.min_cents, summary.min_start), (-1.0, 0));
        assert_eq!((summary.max_cents, summary.max_start), (50.0, DAY));
        assert_eq!((summary.free_minutes, summary.free_days), (5.0, 1));
        assert_eq!(summary.spike_intervals, 1);
        assert!(summarise(&[], &[]).is_none());
    }
}
