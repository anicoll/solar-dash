//! The `globird-probe` command: logs in, calls every endpoint we might use and
//! writes what came back to a JSON file, with personal details redacted. The
//! point is to learn the real response shapes before designing database tables.
//!
//! Each call is independent: if one fails, its error is recorded under its key
//! and the probe carries on.

use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Days, NaiveDate, Utc};
use chrono_tz::Tz;
use serde_json::{Map, Value, json};

use super::client::Client;

/// Pause before every request, to be polite to the portal.
const PAUSE: Duration = Duration::from_millis(500);

/// Object keys (lowercase) whose values are replaced with "REDACTED".
const REDACTED_KEYS: &[&str] = &[
    "accesstoken",
    "accountaddress",
    "accountname",
    "accountnumber",
    "address",
    "concessionaddress",
    "documentid",
    "email",
    "emailaddress",
    "identifier",
    "invoicenumber",
    "nmi",
    "password",
    "serial",
    "serialnumber",
    "siteaddress",
    "siteidentifier",
    "streetaddress",
    "siteidentifiernmi",
    "firstname",
    "lastname",
    "phone",
    "mobile",
    "phonenumber",
    "dateofbirth",
    "bsb",
    "bankaccount",
    "cardnumber",
];

/// Account-level endpoints: output key, path, and whether it takes `accountId`.
const ACCOUNT_ENDPOINTS: &[(&str, &str, bool)] = &[
    ("dashboard", "/api/account/dashboard", true),
    ("balance", "/api/transaction/balance", true),
    ("signup_info", "/api/account/getSignupInfo", true),
    ("service_status", "/api/site/accountservicestatus", false),
    (
        "weather_impacted_days",
        "/api/weather/calculateweatherimpacteddays",
        true,
    ),
];

/// How many days back the history-depth checks start.
const HISTORY_STARTS: [u64; 2] = [365, 730];

/// How long each history-depth window is, in days.
const HISTORY_WINDOW_DAYS: u64 = 31;

/// An inclusive range of local dates.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DateRange {
    from: NaiveDate,
    to: NaiveDate,
}

impl DateRange {
    /// `days` before `end`, through `end`.
    fn ending_on(end: NaiveDate, days: u64) -> Self {
        Self {
            from: end - Days::new(days),
            to: end,
        }
    }

    /// `days` after `start`, starting on `start`.
    fn starting_on(start: NaiveDate, days: u64) -> Self {
        Self {
            from: start,
            to: start + Days::new(days),
        }
    }

    /// "2026/09/21", the form the usage endpoints want.
    fn slash(date: NaiveDate) -> String {
        date.format("%Y/%m/%d").to_string()
    }

    /// "2026-09-21", the form the cost endpoint wants.
    fn dash(date: NaiveDate) -> String {
        date.format("%Y-%m-%d").to_string()
    }

    /// The weather endpoint's start: midnight UTC on the first day.
    fn weather_from(&self) -> String {
        format!("{}T00:00:00Z", Self::dash(self.from))
    }

    /// The weather endpoint's end: the last moment of the last day.
    fn weather_to(&self) -> String {
        format!("{}T23:59:59.999999Z", Self::dash(self.to))
    }
}

/// Turns a text or number JSON value into text. Ids may be either.
fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Whether a string field equals `expected`, ignoring case.
fn field_is(value: &Value, field: &str, expected: &str) -> bool {
    value
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|s| s.trim().eq_ignore_ascii_case(expected))
}

/// One account from `currentuser`.
#[derive(Debug)]
struct Account {
    id: String,
    services: Vec<Service>,
}

/// One service (a power or gas connection) on an account.
#[derive(Debug)]
struct Service {
    id: String,
    /// The NMI (or gas MIRN). Sent to the portal but redacted from the dump.
    nmi: Option<String>,
    post_code: Option<String>,
    is_gas: bool,
    is_closed: bool,
    /// The service exactly as the portal sent it.
    raw: Value,
}

impl Service {
    fn from_value(raw: &Value) -> Option<Self> {
        // Without an id nothing else can be asked about the service.
        let id = text_of(raw.get("accountServiceId")?)?;
        let has_closed_date = raw.get("closedDate").is_some_and(|d| !d.is_null());
        Some(Self {
            id,
            nmi: raw.get("siteIdentifier").and_then(text_of),
            post_code: raw.get("postCode").and_then(text_of),
            is_gas: field_is(raw, "serviceType", "gas"),
            is_closed: has_closed_date || field_is(raw, "status", "closed"),
            raw: raw.clone(),
        })
    }
}

/// Reads the accounts and their services out of the `currentuser` data.
fn parse_accounts(data: &Value) -> Vec<Account> {
    let Some(accounts) = data.get("accounts").and_then(Value::as_array) else {
        return Vec::new();
    };
    accounts
        .iter()
        .filter_map(|account| {
            let id = text_of(account.get("accountId")?)?;
            let services = account
                .get("services")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(Service::from_value).collect())
                .unwrap_or_default();
            Some(Account { id, services })
        })
        .collect()
}

/// The meters in a `readmeters` reply. It should be a list, but may be an
/// object holding the list under `data`, `meters`, `items` or `readMeters`.
fn meter_list(data: &Value) -> Vec<Value> {
    if let Some(list) = data.as_array() {
        return list.clone();
    }
    ["data", "meters", "items", "readMeters"]
        .iter()
        .filter_map(|key| data.get(key))
        .find_map(|inner| inner.as_array().cloned())
        .unwrap_or_default()
}

/// Picks the meter to ask for usage. Prefers one whose NMI matches the
/// service, then an active one, then one that is not a "Basic" meter.
fn pick_meter<'a>(meters: &'a [Value], nmi: Option<&str>) -> Option<&'a Value> {
    let score = |meter: &Value| {
        let nmi_matches = nmi.is_some_and(|nmi| {
            ["siteIdentifier", "nmi"]
                .iter()
                .any(|key| field_is(meter, key, nmi))
        });
        let active = ["active", "current"]
            .iter()
            .any(|word| field_is(meter, "serialStatus", word));
        let smart = !field_is(meter, "meterReadType", "basic");
        // Tuples compare left to right, so the first item matters most.
        (nmi_matches, active, smart)
    };
    // `max_by_key` returns the last of equal items. Reversing first makes
    // ties go to the meter listed first.
    meters.iter().rev().max_by_key(|meter| score(meter))
}

/// Replaces the value of every sensitive key with "REDACTED", all the way down.
fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if is_sensitive_key(key) && !inner.is_null() {
                    *inner = Value::String("REDACTED".to_string());
                } else {
                    redact(inner);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        Value::String(text) if looks_like_secret(text) => {
            *text = "REDACTED".to_string();
        }
        _ => {}
    }
}

/// Whether an object key holds personal or secret data.
fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    // Anything cookie- or token-like goes too, whatever it is called exactly.
    REDACTED_KEYS.contains(&key.as_str()) || key.contains("cookie") || key.contains("token")
}

/// Whether a text value looks like a JWT or an affinity cookie.
fn looks_like_secret(text: &str) -> bool {
    (text.starts_with("eyJ") && text.matches('.').count() == 2)
        || text.to_ascii_lowercase().contains("arraffinity")
}

/// Runs one probe call: waits a moment, then turns the result into JSON. A
/// failure becomes `{"error": "..."}` so it cannot stop the other calls.
async fn probed(call: impl Future<Output = Result<Value>>) -> Value {
    tokio::time::sleep(PAUSE).await;
    match call.await {
        Ok(data) => data,
        Err(err) => json!({ "error": format!("{err:#}") }),
    }
}

/// Probes the endpoints of one service.
async fn probe_service(
    client: &Client,
    service: &Service,
    range: DateRange,
    today: NaiveDate,
) -> Value {
    let asid = service.id.as_str();
    let by_service = [("accountServiceId", asid)];

    let meters = probed(client.get_json("/api/site/readmeters", &by_service)).await;
    let meter_values = meter_list(&meters);
    let meter = pick_meter(&meter_values, service.nmi.as_deref());
    let serial = meter.and_then(|m| m.get("serialNumber")).and_then(text_of);
    let is_smart = !service.is_gas && !meter.is_some_and(|m| field_is(m, "meterReadType", "basic"));

    let usage = match &serial {
        None => json!({ "error": "no meter with a serial number was found" }),
        Some(serial) => {
            let path = if is_smart {
                "/api/site/accountservicetimezonesmartmeterread"
            } else {
                "/api/site/basicmeterread"
            };
            let body = json!({
                "identifier": service.nmi,
                "serialNumber": serial,
                "fromDate": DateRange::slash(range.from),
                "toDate": DateRange::slash(range.to),
                "isSmart": is_smart,
                "isAcrossAccount": false,
            });
            probed(client.post_json(path, &by_service, &body)).await
        }
    };

    let cost = probed(client.post_json(
        "/api/transaction/CostDetail",
        &[],
        &cost_body(service, range, is_smart),
    ))
    .await;

    let weather = match &service.post_code {
        None => Value::Null,
        Some(post_code) => {
            let body = json!({
                "accountServiceId": asid,
                "dateFrom": range.weather_from(),
                "dateTo": range.weather_to(),
                "postCode": post_code,
            });
            probed(client.post_json("/api/weather/getWeatherData", &[], &body)).await
        }
    };

    // How far back can we go? Ask for a window a year and two years ago, and
    // keep only a summary of each.
    let mut cost_history = Vec::new();
    for days_ago in HISTORY_STARTS {
        let window = DateRange::starting_on(today - Days::new(days_ago), HISTORY_WINDOW_DAYS);
        let reply = probed(client.post_json(
            "/api/transaction/CostDetail",
            &[],
            &cost_body(service, window, is_smart),
        ))
        .await;
        cost_history.push(summarise_window(window, &reply));
    }

    json!({
        "service": service.raw,
        "meters": meters,
        "meter": meter,
        "usage": usage,
        "cost": cost,
        "weather": weather,
        "cost_history": cost_history,
    })
}

/// The JSON body for a `CostDetail` call.
fn cost_body(service: &Service, range: DateRange, is_smart: bool) -> Value {
    json!({
        "accountServiceId": service.id,
        "identifier": service.nmi,
        "from": DateRange::dash(range.from),
        "to": DateRange::dash(range.to),
        "isSmart": is_smart,
    })
}

/// Boils a `CostDetail` reply down to its row count and first and last dates.
fn summarise_window(window: DateRange, reply: &Value) -> Value {
    let mut summary = Map::new();
    summary.insert("from".into(), json!(DateRange::dash(window.from)));
    summary.insert("to".into(), json!(DateRange::dash(window.to)));
    if reply.get("error").is_some() {
        summary.insert("error".into(), reply["error"].clone());
    } else {
        let mut dates = Vec::new();
        collect_strings(reply, "date", &mut dates);
        summary.insert("rows".into(), json!(row_count(reply)));
        summary.insert("first_date".into(), json!(dates.first()));
        summary.insert("last_date".into(), json!(dates.last()));
    }
    Value::Object(summary)
}

/// How many rows a reply holds: its length if it is a list, otherwise the
/// length of the longest list directly inside it. `None` if there is none.
fn row_count(value: &Value) -> Option<usize> {
    match value {
        Value::Array(list) => Some(list.len()),
        Value::Object(map) => map
            .values()
            .filter_map(|inner| inner.as_array().map(Vec::len))
            .max(),
        _ => None,
    }
}

/// Collects, in document order, every text value stored under `key`.
fn collect_strings(value: &Value, key: &str, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (name, inner) in map {
                match inner {
                    Value::String(text) if name == key => found.push(text.clone()),
                    _ => collect_strings(inner, key, found),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_strings(i, key, found)),
        _ => {}
    }
}

/// The length of the first `usageArray` found anywhere in `value`.
fn first_usage_array_len(value: &Value) -> Option<usize> {
    match value {
        Value::Object(map) => {
            if let Some(list) = map.get("usageArray").and_then(Value::as_array) {
                return Some(list.len());
            }
            map.values().find_map(first_usage_array_len)
        }
        Value::Array(items) => items.iter().find_map(first_usage_array_len),
        _ => None,
    }
}

/// Probes everything and builds the dump (not yet redacted).
async fn build_dump(client: &Client, tz: Tz, days: u32) -> Result<Value> {
    let today = Utc::now().with_timezone(&tz).date_naive();
    let range = DateRange::ending_on(today, u64::from(days));

    let current_user = client
        .get_json("/api/account/currentuser", &[])
        .await
        .context("could not read the account list after logging in")?;
    let accounts = parse_accounts(&current_user);
    let Some(first) = accounts.first() else {
        bail!("the portal returned no accounts for this login");
    };

    // The raw `currentuser` data goes in too: it shows every field.
    let mut account = Map::new();
    account.insert("currentuser".into(), current_user);
    for (key, path, takes_account_id) in ACCOUNT_ENDPOINTS {
        let query: Vec<(&str, &str)> = if *takes_account_id {
            vec![("accountId", first.id.as_str())]
        } else {
            Vec::new()
        };
        account.insert((*key).into(), probed(client.get_json(path, &query)).await);
    }

    let mut services = Vec::new();
    for service in accounts.iter().flat_map(|a| &a.services) {
        if service.is_closed {
            continue;
        }
        println!("Probing service {} ...", service.id);
        services.push(probe_service(client, service, range, today).await);
    }

    Ok(json!({
        "probed_at": Utc::now().to_rfc3339(),
        "solar_dash_version": env!("CARGO_PKG_VERSION"),
        "days": days,
        "account": account,
        "services": services,
    }))
}

/// The lines printed after a probe: one block per service.
fn summary_lines(dump: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let services = dump["services"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for (i, service) in services.iter().enumerate() {
        let count = |key: &str| {
            row_count(&service[key]).map_or_else(|| "none".to_string(), |n| n.to_string())
        };
        let mut categories = Vec::new();
        collect_strings(&service["cost"], "chargeCategory", &mut categories);
        categories.sort();
        categories.dedup();
        let intervals = first_usage_array_len(&service["usage"])
            .map_or_else(|| "none found".to_string(), |n| n.to_string());
        lines.push(format!("Service {}:", i + 1));
        lines.push(format!("  usage rows: {}", count("usage")));
        lines.push(format!("  cost rows: {}", count("cost")));
        lines.push(format!(
            "  chargeCategory values: {}",
            categories.join(", ")
        ));
        lines.push(format!("  first usageArray length: {intervals}"));
        for window in service["cost_history"].as_array().into_iter().flatten() {
            let detail = match window.get("error") {
                Some(err) => format!("error: {err}"),
                None => format!("{} rows", window["rows"]),
            };
            lines.push(format!(
                "  history {} to {}: {detail}",
                window["from"].as_str().unwrap_or("?"),
                window["to"].as_str().unwrap_or("?"),
            ));
        }
    }
    lines
}

/// Writes `text` to `path`, readable only by the owner on unix.
fn write_private(path: &Path, text: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Set the mode when creating, so the file is never briefly readable.
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("could not open {} for writing", path.display()))?;
    // A file that already existed keeps its old mode, so set it again.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not set permissions on {}", path.display()))?;
    }
    file.write_all(text.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))
}

/// Logs in, probes the portal, and writes the redacted dump to `out`.
pub async fn run(client: Client, tz: Tz, days: u32, out: &Path) -> Result<()> {
    client.login().await?;
    let mut dump = build_dump(&client, tz, days).await?;
    redact(&mut dump);
    let text = serde_json::to_string_pretty(&dump)?;
    write_private(out, &text)?;

    let service_count = dump["services"].as_array().map_or(0, Vec::len);
    println!("Wrote {}", out.display());
    println!("Services probed: {service_count}");
    for line in summary_lines(&dump) {
        println!("{line}");
    }
    println!();
    println!(
        "Personal details are redacted, but check the file before sharing it: \
         the redaction goes by field name and cannot know every field."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn date_ranges_format_for_each_endpoint() {
        let range = DateRange::ending_on(date(2026, 10, 22), 31);
        assert_eq!(range.from, date(2026, 9, 21));
        assert_eq!(DateRange::slash(range.from), "2026/09/21");
        assert_eq!(DateRange::dash(range.to), "2026-10-22");
        assert_eq!(range.weather_from(), "2026-09-21T00:00:00Z");
        assert_eq!(range.weather_to(), "2026-10-22T23:59:59.999999Z");

        let window = DateRange::starting_on(date(2025, 10, 22), 31);
        assert_eq!(window.to, date(2025, 11, 22));
    }

    #[test]
    fn redacts_sensitive_keys_at_any_depth() {
        let mut value = json!({
            "accountId": 123,
            "accountNumber": "999",
            "postCode": "5000",
            "Email": "a@b.c",
            "nested": [{"siteIdentifier": "NMI1", "accountServiceId": 7, "nmi": null}],
            "address": {"street": "1 Road"},
            "sessionCookie": "x",
            "refreshToken": "y",
            "note": "eyJhbGciOi.payload.sig",
            "plain": "fine",
        });
        redact(&mut value);
        assert_eq!(value["accountId"], 123);
        assert_eq!(value["postCode"], "5000");
        assert_eq!(value["accountNumber"], "REDACTED");
        assert_eq!(value["Email"], "REDACTED");
        assert_eq!(value["nested"][0]["siteIdentifier"], "REDACTED");
        assert_eq!(value["nested"][0]["accountServiceId"], 7);
        // Null stays null: it tells us the field exists but is empty.
        assert!(value["nested"][0]["nmi"].is_null());
        assert_eq!(value["address"], "REDACTED");
        assert_eq!(value["sessionCookie"], "REDACTED");
        assert_eq!(value["refreshToken"], "REDACTED");
        assert_eq!(value["note"], "REDACTED");
        assert_eq!(value["plain"], "fine");
    }

    #[test]
    fn parses_accounts_and_services() {
        let data = json!({"accounts": [{
            "accountId": 11,
            "accountNumber": "A1",
            "services": [
                {"accountServiceId": 1, "siteIdentifier": "N1", "postCode": "5000",
                 "status": "Active", "serviceType": "Power"},
                {"accountServiceId": "2", "serviceType": "Gas", "status": "Closed"},
                {"accountServiceId": 3, "closedDate": "2025-01-01T00:00:00"},
                {"noId": true},
            ],
        }]});
        let accounts = parse_accounts(&data);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, "11");
        let services = &accounts[0].services;
        assert_eq!(services.len(), 3);
        assert_eq!(services[0].nmi.as_deref(), Some("N1"));
        assert_eq!(services[0].post_code.as_deref(), Some("5000"));
        assert!(!services[0].is_closed && !services[0].is_gas);
        assert!(services[1].is_gas && services[1].is_closed);
        assert!(services[2].is_closed);
        assert!(parse_accounts(&json!({})).is_empty());
    }

    #[test]
    fn finds_meters_in_lists_and_wrappers() {
        let list = json!([{"serialNumber": "S1"}]);
        assert_eq!(meter_list(&list).len(), 1);
        for key in ["data", "meters", "items", "readMeters"] {
            let wrapped = json!({ key: [{"serialNumber": "S1"}, {"serialNumber": "S2"}] });
            assert_eq!(meter_list(&wrapped).len(), 2, "{key}");
        }
        assert!(meter_list(&json!({"other": 1})).is_empty());
        assert!(meter_list(&Value::Null).is_empty());
    }

    #[test]
    fn picks_the_best_meter() {
        let meters = vec![
            json!({"serialNumber": "basic", "meterReadType": "Basic", "serialStatus": "Active"}),
            json!({"serialNumber": "old", "meterReadType": "Smart", "serialStatus": "Removed"}),
            json!({"serialNumber": "smart", "meterReadType": "Smart", "serialStatus": "Active"}),
            json!({"serialNumber": "other", "meterReadType": "Smart", "serialStatus": "Active",
                   "siteIdentifier": "N2"}),
            json!({"serialNumber": "mine", "meterReadType": "Basic", "serialStatus": "Removed",
                   "nmi": "n1"}),
        ];
        let serial = |nmi| pick_meter(&meters, nmi).unwrap()["serialNumber"].clone();
        // A matching NMI beats everything else.
        assert_eq!(serial(Some("N1")), "mine");
        // Without a match: active and smart, first listed.
        assert_eq!(serial(Some("N9")), "smart");
        assert_eq!(serial(None), "smart");
        assert!(pick_meter(&[], None).is_none());
        // Only inactive Basic meters: the first one is still picked.
        let weak = vec![
            json!({"serialNumber": "a", "meterReadType": "Basic"}),
            json!({"serialNumber": "b", "meterReadType": "Basic"}),
        ];
        assert_eq!(pick_meter(&weak, None).unwrap()["serialNumber"], "a");
    }

    #[test]
    fn window_summary_has_count_and_dates() {
        let window = DateRange::starting_on(date(2025, 10, 22), 31);
        let reply = json!([{"date": "2025-10-22", "x": 1}, {"date": "2025-10-23"}]);
        let summary = summarise_window(window, &reply);
        assert_eq!(summary["rows"], 2);
        assert_eq!(summary["first_date"], "2025-10-22");
        assert_eq!(summary["last_date"], "2025-10-23");

        let failed = summarise_window(window, &json!({"error": "nope"}));
        assert_eq!(failed["error"], "nope");
        assert!(failed.get("rows").is_none());
    }

    #[test]
    fn summary_reports_shapes() {
        let dump = json!({"services": [{
            "usage": {"days": [{"usageArray": [1, 2, 3]}, {"usageArray": [1]}]},
            "cost": [
                {"chargeCategory": "Peak"}, {"chargeCategory": "Supply"},
                {"chargeCategory": "Peak"},
            ],
            "cost_history": [
                {"from": "2025-10-22", "to": "2025-11-22", "rows": 31},
                {"from": "2024-10-22", "to": "2024-11-22", "error": "nope"},
            ],
        }]});
        let text = summary_lines(&dump).join("\n");
        assert!(text.contains("usage rows: 2"), "{text}");
        assert!(text.contains("cost rows: 3"), "{text}");
        assert!(
            text.contains("chargeCategory values: Peak, Supply"),
            "{text}"
        );
        assert!(text.contains("first usageArray length: 3"), "{text}");
        assert!(
            text.contains("history 2025-10-22 to 2025-11-22: 31 rows"),
            "{text}"
        );
        assert!(text.contains("error"), "{text}");
    }
}
