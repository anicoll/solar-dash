//! The `amber-probe` command: prints what Amber's API returns for the
//! configured site, so we can check it against our own meter before building
//! on it. Everything is read-only.
//!
//! The sections are independent: if one call fails (a closed site may answer
//! 404, say) the error is printed and the next section still runs.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{Days, NaiveDate, Utc};
use chrono_tz::Tz;
use serde_json::{Map, Value, json};

use super::client::Client;
use super::models::{ChannelType, PriceInterval, Site, Usage};
use crate::config::Amber;

/// How many past and future intervals of prices to ask for.
const PRICE_WINDOW: u32 = 12;

/// How many days of usage to look for. Amber allows at most 7 between the
/// start and end date.
const USAGE_DAYS: u64 = 7;

/// What to do about the site after reading `/sites`.
#[derive(Debug, PartialEq, Eq)]
enum SiteChoice {
    /// Use this site id.
    Use(String),
    /// Several sites and none chosen in the config: the user has to pick.
    NeedsChoice,
    /// No sites at all.
    NoSites,
}

/// Picks the site: the configured `site_id` if there is one (even when it is
/// not in the list, because a closed site may be missing from `/sites` but
/// still worth asking about), otherwise the only site.
fn choose_site(sites: &[Site], configured: Option<&str>) -> SiteChoice {
    if let Some(id) = configured {
        return SiteChoice::Use(id.to_string());
    }
    match sites {
        [] => SiteChoice::NoSites,
        [only] => SiteChoice::Use(only.id.clone()),
        _ => SiteChoice::NeedsChoice,
    }
}

/// One line per site, then its channels.
fn site_lines(sites: &[Site]) -> Vec<String> {
    let mut lines = Vec::new();
    for site in sites {
        let mut line = format!(
            "{}  NMI {}  status {:?}  network {}",
            site.id,
            site.nmi,
            site.status,
            site.network.as_deref().unwrap_or("?"),
        );
        if let Some(from) = &site.active_from {
            line.push_str(&format!("  active from {from}"));
        }
        if let Some(on) = &site.closed_on {
            line.push_str(&format!("  closed on {on}"));
        }
        if let Some(minutes) = site.interval_length {
            line.push_str(&format!("  {minutes}-minute intervals"));
        }
        lines.push(line);
        let channels: Vec<String> = site
            .channels
            .iter()
            .map(|c| format!("{} ({})", c.identifier, c.kind.as_str()))
            .collect();
        lines.push(format!("    channels: {}", channels.join(", ")));
    }
    lines
}

/// The price table: local time, kind, channel, perKwh, spotPerKwh, duration.
/// Rows are sorted by time, then channel, so each half hour reads together.
fn price_lines(prices: &[PriceInterval], tz: Tz) -> Vec<String> {
    let mut rows: Vec<&PriceInterval> = prices.iter().collect();
    rows.sort_by_key(|p| (p.start_time, p.channel_type));
    let mut lines = vec![format!(
        "{:<17} {:<9} {:<15} {:>9} {:>11} {:>8}",
        "local time", "kind", "channel", "perKwh", "spotPerKwh", "minutes"
    )];
    for p in rows {
        lines.push(format!(
            "{:<17} {:<9} {:<15} {:>9} {:>11} {:>8}",
            p.start_time
                .with_timezone(&tz)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            p.kind.short_name(),
            p.channel_type.as_str(),
            p.per_kwh,
            p.spot_per_kwh,
            p.duration,
        ));
    }
    lines
}

/// What the probe can say about the sign of feed-in prices from one reply.
/// It states what was returned and does not decide what it means.
fn feed_in_note(prices: &[PriceInterval]) -> String {
    let feed_in: Vec<f64> = prices
        .iter()
        .filter(|p| p.channel_type == ChannelType::FeedIn)
        .map(|p| p.per_kwh)
        .collect();
    if feed_in.is_empty() {
        return "No feedIn rows came back, so the feed-in sign cannot be checked.".to_string();
    }
    let negative = feed_in.iter().filter(|v| **v < 0.0).count();
    let positive = feed_in.iter().filter(|v| **v > 0.0).count();
    format!(
        "feedIn perKwh is printed exactly as the API returned it: {} rows, {negative} negative, \
         {positive} positive. Compare with the Amber app to confirm which sign means \"you are \
         paid\"; the code does not assume one yet.",
        feed_in.len(),
    )
}

/// Usage rows per channel, as "E1 (general): 336 intervals".
fn usage_lines(usage: &[Usage]) -> Vec<String> {
    let mut counts: BTreeMap<(String, ChannelType), usize> = BTreeMap::new();
    for row in usage {
        *counts
            .entry((row.channel_identifier.clone(), row.channel_type))
            .or_default() += 1;
    }
    counts
        .into_iter()
        .map(|((id, kind), n)| format!("  {id} ({}): {n} intervals", kind.as_str()))
        .collect()
}

/// Turns a raw reply body into JSON for the `--json` file. A body that is not
/// JSON is kept as text, so nothing is lost.
fn raw_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Runs the probe and prints the report. Only a failure to write the `--json`
/// file is returned as an error; API failures are printed as part of the report.
pub async fn run(config: &Amber, tz: Tz, json_out: Option<&std::path::Path>) -> Result<()> {
    let client = Client::new(config)?;
    let mut saved = Map::new();

    println!("== Sites ==");
    let sites = match client.sites().await {
        Ok(reply) => {
            saved.insert("sites".into(), raw_value(&reply.raw));
            if reply.items.is_empty() {
                println!("The account has no sites.");
            }
            for line in site_lines(&reply.items) {
                println!("{line}");
            }
            reply.items
        }
        Err(err) => {
            println!("Could not list sites: {err:#}");
            saved.insert("sites".into(), json!({ "error": format!("{err:#}") }));
            Vec::new()
        }
    };

    let site_id = match choose_site(&sites, config.site_id.as_deref()) {
        SiteChoice::Use(id) => {
            if config.site_id.is_some() && !sites.iter().any(|s| s.id == id) && !sites.is_empty() {
                println!("\nNote: site_id {id} is not in the list above; trying it anyway.");
            }
            Some(id)
        }
        SiteChoice::NeedsChoice => {
            println!(
                "\nThere are {} sites. Set site_id in [amber] to one of the ids above to see \
                 prices and usage.",
                sites.len()
            );
            None
        }
        SiteChoice::NoSites => {
            println!("\nNo site to look at. Set site_id in [amber] if you know it.");
            None
        }
    };

    if let Some(site_id) = site_id {
        println!(
            "\n== Current prices for {site_id} (previous {PRICE_WINDOW}, next {PRICE_WINDOW}) =="
        );
        match client
            .current_prices(&site_id, PRICE_WINDOW, PRICE_WINDOW)
            .await
        {
            Ok(reply) => {
                saved.insert("prices".into(), raw_value(&reply.raw));
                if reply.items.is_empty() {
                    println!("Amber returned no price intervals.");
                } else {
                    for line in price_lines(&reply.items, tz) {
                        println!("{line}");
                    }
                }
                println!("\n{}", feed_in_note(&reply.items));
            }
            Err(err) => {
                println!("Could not get prices: {err:#}");
                saved.insert("prices".into(), json!({ "error": format!("{err:#}") }));
            }
        }

        let end: NaiveDate = Utc::now().with_timezone(&tz).date_naive();
        let start = end - Days::new(USAGE_DAYS - 1);
        println!("\n== Usage for {site_id}, {start} to {end} ==");
        match client.usage(&site_id, start, end).await {
            Ok(reply) => {
                saved.insert("usage".into(), raw_value(&reply.raw));
                if reply.items.is_empty() {
                    println!("No usage rows came back for these days.");
                } else {
                    println!("Usage exists. Intervals per channel:");
                    for line in usage_lines(&reply.items) {
                        println!("{line}");
                    }
                }
            }
            Err(err) => {
                println!("Could not get usage: {err:#}");
                saved.insert("usage".into(), json!({ "error": format!("{err:#}") }));
            }
        }
    }

    if let Some(path) = json_out {
        let text = serde_json::to_string_pretty(&Value::Object(saved))?;
        // Raw replies hold the NMI and address-like details: owner-only file.
        crate::globird::probe::write_private(path, &text)?;
        println!(
            "\nSaved the raw replies to {} (contains your NMI; do not share it as is).",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amber::models::SiteStatus;

    fn sites() -> Vec<Site> {
        serde_json::from_str(include_str!("testdata/sites.json")).unwrap()
    }

    fn prices() -> Vec<PriceInterval> {
        serde_json::from_str(include_str!("testdata/prices_current.json")).unwrap()
    }

    #[test]
    fn site_choice_rules() {
        let all = sites();
        assert_eq!(
            choose_site(&all[..1], None),
            SiteChoice::Use(all[0].id.clone())
        );
        assert_eq!(choose_site(&all, None), SiteChoice::NeedsChoice);
        assert_eq!(choose_site(&[], None), SiteChoice::NoSites);
        // A configured id wins, even for a site that is not listed.
        assert_eq!(
            choose_site(&all, Some("other")),
            SiteChoice::Use("other".into())
        );
    }

    #[test]
    fn site_lines_show_dates_and_status() {
        let text = site_lines(&sites()).join("\n");
        assert!(text.contains("NMI 3052282872"), "{text}");
        assert!(text.contains("active from 2022-01-01"), "{text}");
        assert!(text.contains("closed on 2022-05-01"), "{text}");
        assert!(
            text.contains(&format!("{:?}", SiteStatus::Closed)),
            "{text}"
        );
        assert!(text.contains("B1 (feedIn)"), "{text}");
    }

    #[test]
    fn price_table_uses_local_time() {
        let tz: Tz = "Australia/Adelaide".parse().unwrap();
        let lines = price_lines(&prices(), tz);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("spotPerKwh"));
        // 02:00 UTC on 5 May is 11:30 in Adelaide (UTC+9:30 in winter).
        assert!(lines[1].starts_with("2021-05-05 11:30"), "{}", lines[1]);
        assert!(lines[1].contains("actual"));
        assert!(lines[1].contains("24.33"));
        assert!(lines[2].contains("current"));
        assert!(lines[2].contains("feedIn"));
        assert!(lines[2].contains("-4.2"));
        assert!(lines[3].contains("forecast"));
    }

    #[test]
    fn feed_in_note_reports_what_was_returned() {
        let note = feed_in_note(&prices());
        assert!(note.contains("1 rows, 1 negative, 0 positive"), "{note}");
        let none: Vec<PriceInterval> = prices().into_iter().take(1).collect();
        assert!(feed_in_note(&none).contains("cannot be checked"));
    }

    #[test]
    fn usage_is_counted_per_channel() {
        let usage: Vec<Usage> = serde_json::from_str(include_str!("testdata/usage.json")).unwrap();
        let lines = usage_lines(&usage);
        assert_eq!(
            lines,
            vec!["  B1 (feedIn): 1 intervals", "  E1 (general): 1 intervals"]
        );
    }
}
