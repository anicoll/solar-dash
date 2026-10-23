//! The `amber-probe` command: asks the Amber API what it will give us, before
//! we design an import of the 30-minute usage history. It lists the sites,
//! fetches a sample window of usage, tries longer windows to find the most one
//! call allows, checks how far back history goes and prints the rate limit
//! headers.
//!
//! Each call is independent: if one fails, the probe notes it and carries on.
//! The raw replies are saved in the output directory. The token is only ever
//! sent as a header; it is never printed or written.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Days, NaiveDate};
use serde_json::Value;

use super::client::{Client, Reply, Site, Usage, parse_sites, parse_usage};
use crate::globird::probe::write_private;

/// Pause before every request, to be polite to the API.
const PAUSE: Duration = Duration::from_millis(500);

/// Window lengths, in days, tried to find the most one call allows.
const RANGE_DAYS: [u64; 3] = [7, 31, 90];

/// The site's first and last day, from `activeFrom` and `closedOn`.
fn site_span(site: &Site) -> (Option<NaiveDate>, Option<NaiveDate>) {
    let parse = |text: &Option<String>| text.as_deref().and_then(|t| t.parse().ok());
    (parse(&site.active_from), parse(&site.closed_on))
}

/// Single days asked for to see how far back history goes: just before and on
/// the site's first day, half a year in, and either side of its last day (or
/// a week ago, for a site that is still open).
fn history_dates(site: &Site, today: NaiveDate) -> Vec<NaiveDate> {
    let (from, closed) = site_span(site);
    let mut dates = Vec::new();
    if let Some(from) = from {
        dates.extend([from - Days::new(14), from, from + Days::new(180)]);
    }
    match closed {
        Some(closed) => dates.extend([closed - Days::new(5), closed + Days::new(6)]),
        None => dates.push(today - Days::new(7)),
    }
    dates
}

/// Totals for one channel on one day.
#[derive(Default)]
struct DaySummary {
    intervals: usize,
    kwh: f64,
    cost: f64,
    min_price: Option<f64>,
    max_price: Option<f64>,
    /// How many intervals have each quality ("billable", "estimated", ...).
    quality: BTreeMap<String, usize>,
}

/// Groups usage rows by local date and channel, and totals each group.
fn summarise(rows: &[Usage]) -> BTreeMap<(String, String), DaySummary> {
    let mut days: BTreeMap<(String, String), DaySummary> = BTreeMap::new();
    for row in rows {
        let date = row.date.clone().unwrap_or_else(|| "?".to_string());
        let channel = format!(
            "{}/{}",
            row.channel_identifier.as_deref().unwrap_or("?"),
            row.channel_type.as_deref().unwrap_or("?")
        );
        let day = days.entry((date, channel)).or_default();
        day.intervals += 1;
        day.kwh += row.kwh.unwrap_or(0.0);
        day.cost += row.cost.unwrap_or(0.0);
        if let Some(price) = row.per_kwh {
            day.min_price = Some(day.min_price.map_or(price, |m| m.min(price)));
            day.max_price = Some(day.max_price.map_or(price, |m| m.max(price)));
        }
        let quality = row.quality.as_deref().unwrap_or("?").to_string();
        *day.quality.entry(quality).or_insert(0) += 1;
    }
    days
}

/// The per-day, per-channel lines of the summary.
fn day_lines(rows: &[Usage]) -> Vec<String> {
    let mut lines = Vec::new();
    for ((date, channel), day) in summarise(rows) {
        let quality: Vec<String> = day
            .quality
            .iter()
            .map(|(q, n)| format!("{q} {n}"))
            .collect();
        lines.push(format!(
            "  {date} {channel}: {} intervals, {:.3} kWh, cost {:.2} c, perKwh {} to {} c, quality: {}",
            day.intervals,
            day.kwh,
            day.cost,
            price_text(day.min_price),
            price_text(day.max_price),
            quality.join(", ")
        ));
    }
    lines
}

fn price_text(price: Option<f64>) -> String {
    price.map_or_else(|| "?".to_string(), |p| format!("{p:.2}"))
}

/// Which way the signs of the feedIn rows go. We believe perKwh and cost are
/// negative when Amber pays the customer, but this reports what the data says.
fn feed_in_signs(rows: &[Usage]) -> String {
    let feed_in: Vec<&Usage> = rows
        .iter()
        .filter(|r| r.channel_type.as_deref() == Some("feedIn"))
        .collect();
    if feed_in.is_empty() {
        return "no feedIn rows in the sample, so the sign convention is unknown".to_string();
    }
    let count = |value: fn(&Usage) -> Option<f64>, negative: bool| {
        feed_in
            .iter()
            .filter_map(|r| value(r))
            .filter(|v| if negative { *v < 0.0 } else { *v > 0.0 })
            .count()
    };
    let (price_neg, price_pos) = (count(|r| r.per_kwh, true), count(|r| r.per_kwh, false));
    let (cost_neg, cost_pos) = (count(|r| r.cost, true), count(|r| r.cost, false));
    let verdict = match (cost_neg, cost_pos) {
        (0, 0) => "all costs are zero, so unclear",
        (_, 0) => "cost is negative when exporting (customer is paid)",
        (0, _) => "cost is positive when exporting",
        _ => "cost has mixed signs",
    };
    format!(
        "feedIn rows {}: perKwh negative {price_neg}, positive {price_pos}; \
         cost negative {cost_neg}, positive {cost_pos}; {verdict}",
        feed_in.len()
    )
}

/// Keeps only the last 3 digits of an NMI.
fn mask_nmi(nmi: &str) -> String {
    let keep = nmi.chars().count().saturating_sub(3);
    let tail: String = nmi.chars().skip(keep).collect();
    format!("{}{tail}", "*".repeat(keep))
}

/// The saved copy of the sites reply: raw, except the NMIs are masked.
fn mask_sites_body(body: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    for site in value.as_array_mut().into_iter().flatten() {
        if let Some(nmi) = site.get("nmi").and_then(Value::as_str) {
            site["nmi"] = Value::String(mask_nmi(nmi));
        }
    }
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.to_string())
}

/// One line describing a site.
fn site_line(site: &Site) -> String {
    let channels: Vec<String> = site
        .channels
        .iter()
        .map(|c| {
            format!(
                "{} {} {}",
                c.identifier.as_deref().unwrap_or("?"),
                c.kind.as_deref().unwrap_or("?"),
                c.tariff.as_deref().unwrap_or("?")
            )
        })
        .collect();
    format!(
        "  {} nmi {} status {} active from {} closed on {} channels [{}]",
        site.id,
        mask_nmi(site.nmi.as_deref().unwrap_or("")),
        site.status.as_deref().unwrap_or("?"),
        site.active_from.as_deref().unwrap_or("?"),
        site.closed_on.as_deref().unwrap_or("-"),
        channels.join("; ")
    )
}

/// Picks the site to probe: the configured one, or the only one there is.
fn pick_site<'a>(sites: &'a [Site], wanted: Option<&str>) -> Result<&'a Site> {
    let ids: Vec<&str> = sites.iter().map(|s| s.id.as_str()).collect();
    match (wanted, sites) {
        (Some(id), _) => sites
            .iter()
            .find(|s| s.id == id)
            .with_context(|| format!("site_id {id} is not one of your sites: {ids:?}")),
        (None, [only]) => Ok(only),
        (None, []) => bail!("the account has no sites"),
        (None, _) => bail!("several sites found, set site_id in [amber] to one of {ids:?}"),
    }
}

/// Runs one call after a pause and saves the raw body as `out/file`. A failure
/// to get any reply comes back as `Err(message)`.
async fn probed(
    out: &Path,
    file: &str,
    call: impl Future<Output = Result<Reply>>,
) -> Result<Reply, String> {
    tokio::time::sleep(PAUSE).await;
    let reply = call.await.map_err(|err| format!("{err:#}"))?;
    write_private(&out.join(file), &reply.body).map_err(|err| format!("{err:#}"))?;
    Ok(reply)
}

/// What a usage call came back with, on one line.
fn outcome_line(result: &Result<Reply, String>) -> String {
    match result {
        Err(err) => format!("no reply: {err}"),
        Ok(reply) if !reply.status.is_success() => {
            format!("HTTP {}: {}", reply.status, reply.snippet())
        }
        Ok(reply) => match parse_usage(&reply.body) {
            Ok(rows) => format!("HTTP {}, {} rows", reply.status, rows.len()),
            Err(err) => format!("HTTP {}, but {err:#}", reply.status),
        },
    }
}

/// The rate limit headers of a call, if it got a reply.
fn headers_of(result: &Result<Reply, String>) -> &[(String, String)] {
    result.as_ref().map_or(&[], |reply| &reply.rate_limit)
}

/// Remembers a call's rate limit headers, if it had any.
fn note_rate_limit(
    seen: &mut Vec<(String, Vec<(String, String)>)>,
    label: &str,
    headers: &[(String, String)],
) {
    if !headers.is_empty() {
        seen.push((label.to_string(), headers.to_vec()));
    }
}

/// Probes the API and prints what it learned. Files go into `out`.
pub async fn run(
    client: Client,
    site_id: Option<&str>,
    date: Option<NaiveDate>,
    days: u32,
    out: &Path,
) -> Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("could not create {}", out.display()))?;
    // Rate limit headers seen, with the call they came from.
    let mut rate_limits: Vec<(String, Vec<(String, String)>)> = Vec::new();

    println!("Listing sites ...");
    let reply = client.sites().await?;
    note_rate_limit(&mut rate_limits, "GET /sites", &reply.rate_limit);
    // Saved with the NMIs masked, like everything we print.
    write_private(&out.join("sites.json"), &mask_sites_body(&reply.body))?;
    if !reply.status.is_success() {
        bail!("GET /sites gave HTTP {}: {}", reply.status, reply.snippet());
    }
    let sites = parse_sites(&reply.body)?;
    let site = pick_site(&sites, site_id)?;

    let mut report = vec!["Sites:".to_string()];
    report.extend(sites.iter().map(site_line));
    report.push(format!("Probing site {}", site.id));

    // The sample window starts on the site's first day unless --date says
    // otherwise, or 30 days ago if the site does not say when it started.
    let today = chrono::Local::now().date_naive();
    let date = date.or(site_span(site).0).unwrap_or(today - Days::new(30));
    let last = date + Days::new(u64::from(days) - 1);
    println!("Fetching the sample window {date} to {last} ...");
    let sample = probed(out, "usage-sample.json", client.usage(&site.id, date, last)).await;
    note_rate_limit(&mut rate_limits, "sample usage", headers_of(&sample));
    report.push(format!(
        "Sample {date} to {last}: {}",
        outcome_line(&sample)
    ));
    if let Ok(reply) = &sample
        && reply.status.is_success()
        && let Ok(rows) = parse_usage(&reply.body)
    {
        report.extend(day_lines(&rows));
        report.push(format!("Sign convention: {}", feed_in_signs(&rows)));
    }

    // Longer windows, all starting on the same day.
    report.push("Longer windows (to find the most one call allows):".to_string());
    for length in RANGE_DAYS {
        let end = date + Days::new(length - 1);
        println!("Fetching {length} days from {date} ...");
        let file = format!("usage-range-{length}d.json");
        let result = probed(out, &file, client.usage(&site.id, date, end)).await;
        note_rate_limit(
            &mut rate_limits,
            &format!("{length}-day window"),
            headers_of(&result),
        );
        report.push(format!(
            "  {length} days ({date} to {end}): {}",
            outcome_line(&result)
        ));
    }

    // One day each, to see how far back history goes.
    report.push("History depth (one day each):".to_string());
    for day in history_dates(site, today) {
        println!("Fetching {day} ...");
        let result = probed(
            out,
            &format!("usage-day-{day}.json"),
            client.usage(&site.id, day, day),
        )
        .await;
        note_rate_limit(&mut rate_limits, &format!("day {day}"), headers_of(&result));
        report.push(format!("  {day}: {}", outcome_line(&result)));
    }

    report.push("Rate limit headers:".to_string());
    // The first and last calls show how the remaining count moves.
    let shown = [rate_limits.first(), rate_limits.last()];
    if rate_limits.is_empty() {
        report.push("  none seen".to_string());
    }
    for (i, entry) in shown.into_iter().flatten().enumerate() {
        if i == 1 && rate_limits.len() == 1 {
            break;
        }
        let (label, headers) = entry;
        let pairs: Vec<String> = headers.iter().map(|(k, v)| format!("{k}={v}")).collect();
        report.push(format!("  after {label}: {}", pairs.join(", ")));
    }

    println!();
    println!("Wrote raw replies to {}", out.display());
    println!("Paste everything below this line back:");
    println!();
    for line in report {
        println!("{line}");
    }
    println!();
    println!(
        "The files have your NMI masked and no token, but they hold your usage \
         history: look before sharing them."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amber::client::parse_usage;
    use reqwest::StatusCode;

    const USAGE_JSON: &str = include_str!("testdata/usage.json");
    const SITES_JSON: &str = include_str!("testdata/sites.json");

    fn reply(status: StatusCode, body: &str) -> Result<Reply, String> {
        Ok(Reply {
            status,
            rate_limit: Vec::new(),
            body: body.to_string(),
        })
    }

    #[test]
    fn summarises_per_day_and_channel() {
        let rows = parse_usage(USAGE_JSON).unwrap();
        let days = summarise(&rows);
        assert_eq!(days.len(), 3);
        let general = &days[&("2024-01-15".to_string(), "E1/general".to_string())];
        assert_eq!(general.intervals, 2);
        assert_eq!(general.kwh, 1.5);
        assert_eq!(general.cost, 40.0);
        assert_eq!(general.min_price, Some(20.0));
        assert_eq!(general.max_price, Some(30.0));
        assert_eq!(general.quality["billable"], 1);
        assert_eq!(general.quality["estimated"], 1);
        // A row with no quality is counted under "?".
        let later = &days[&("2024-07-15".to_string(), "B1/feedIn".to_string())];
        assert_eq!(later.quality["?"], 1);
        assert!(day_lines(&rows)[1].contains("2 intervals, 1.500 kWh"));
    }

    #[test]
    fn reports_the_feed_in_sign_convention() {
        let rows = parse_usage(USAGE_JSON).unwrap();
        let text = feed_in_signs(&rows);
        assert!(text.contains("perKwh negative 2, positive 0"), "{text}");
        assert!(text.contains("customer is paid"), "{text}");

        let general_only: Vec<Usage> = rows
            .into_iter()
            .filter(|r| r.channel_type.as_deref() == Some("general"))
            .collect();
        assert!(feed_in_signs(&general_only).contains("unknown"));
    }

    #[test]
    fn masks_all_but_the_last_three_digits_of_an_nmi() {
        assert_eq!(mask_nmi("4102345678"), "*******678");
        assert_eq!(mask_nmi("12"), "12");
        let masked = mask_sites_body(SITES_JSON);
        assert!(masked.contains("*******678"));
        assert!(!masked.contains("4102345678"));
        // Not JSON: left alone.
        assert_eq!(mask_sites_body("oops"), "oops");
    }

    #[test]
    fn picks_the_site() {
        let sites = parse_sites(SITES_JSON).unwrap();
        // Two sites and no site_id: the user has to choose.
        assert!(pick_site(&sites, None).is_err());
        assert_eq!(
            pick_site(&sites, Some("01ABCDEFGHJKMNPQRSTVWXYZ01"))
                .unwrap()
                .id,
            "01ABCDEFGHJKMNPQRSTVWXYZ01"
        );
        assert!(pick_site(&sites, Some("nope")).is_err());
        assert!(pick_site(&sites[..1], None).is_ok());
        assert!(pick_site(&[], None).is_err());
    }

    #[test]
    fn history_dates_bracket_the_site_span() {
        let sites = parse_sites(SITES_JSON).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 1, 20).unwrap();
        let dates: Vec<String> = history_dates(&sites[0], today)
            .iter()
            .map(|d| d.to_string())
            .collect();
        assert_eq!(
            dates,
            [
                "2024-01-01",
                "2024-01-15",
                "2024-07-13",
                "2025-12-26",
                "2026-01-06"
            ]
        );
        // An open site with no dates: just a week ago.
        assert_eq!(history_dates(&sites[1], today), [today - Days::new(7)]);
    }

    #[test]
    fn site_line_shows_dates_and_channels() {
        let sites = parse_sites(SITES_JSON).unwrap();
        let line = site_line(&sites[0]);
        assert!(line.contains("closed on 2025-12-31"), "{line}");
        assert!(line.contains("B1 feedIn EA116"), "{line}");
        assert!(!line.contains("4102345678"), "{line}");
    }

    #[test]
    fn outcome_lines() {
        assert_eq!(
            outcome_line(&reply(StatusCode::OK, USAGE_JSON)),
            "HTTP 200 OK, 4 rows"
        );
        let too_big = outcome_line(&reply(StatusCode::BAD_REQUEST, r#"{"message":"too long"}"#));
        assert!(
            too_big.contains("400") && too_big.contains("too long"),
            "{too_big}"
        );
        assert!(outcome_line(&reply(StatusCode::OK, "{}")).contains("not the expected list"));
        assert_eq!(
            outcome_line(&Err("timed out".to_string())),
            "no reply: timed out"
        );
    }
}
