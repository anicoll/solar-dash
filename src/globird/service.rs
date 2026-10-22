//! Helpers shared by the probe and the sync: picking a power service and its
//! meter out of the account data, and the request bodies and date formats the
//! usage and cost endpoints want.

use chrono::{Days, NaiveDate};
use serde_json::{Value, json};

/// An inclusive range of local dates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DateRange {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

impl DateRange {
    /// `days` before `end`, through `end`.
    pub fn ending_on(end: NaiveDate, days: u64) -> Self {
        Self {
            from: end - Days::new(days),
            to: end,
        }
    }

    /// `days` after `start`, starting on `start`.
    pub fn starting_on(start: NaiveDate, days: u64) -> Self {
        Self {
            from: start,
            to: start + Days::new(days),
        }
    }

    /// "2026/09/21", the form the usage endpoints want.
    pub fn slash(date: NaiveDate) -> String {
        date.format("%Y/%m/%d").to_string()
    }

    /// "2026-09-21", the form the cost endpoint wants.
    pub fn dash(date: NaiveDate) -> String {
        date.format("%Y-%m-%d").to_string()
    }

    /// The weather endpoint's start: midnight UTC on the first day.
    pub fn weather_from(&self) -> String {
        format!("{}T00:00:00Z", Self::dash(self.from))
    }

    /// The weather endpoint's end: the last moment of the last day.
    pub fn weather_to(&self) -> String {
        format!("{}T23:59:59.999999Z", Self::dash(self.to))
    }
}

/// Turns a text or number JSON value into text. Ids may be either.
pub fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Whether a string field equals `expected`, ignoring case.
pub fn field_is(value: &Value, field: &str, expected: &str) -> bool {
    value
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|s| s.trim().eq_ignore_ascii_case(expected))
}

/// One account from `currentuser`.
#[derive(Debug)]
pub struct Account {
    pub id: String,
    pub services: Vec<Service>,
}

/// One service (a power or gas connection) on an account.
#[derive(Debug)]
pub struct Service {
    pub id: String,
    /// The NMI (or gas MIRN). Sent to the portal but redacted from the dump.
    pub nmi: Option<String>,
    pub post_code: Option<String>,
    pub is_gas: bool,
    /// Whether `serviceType` says electricity ("power" or "electric"). The
    /// sync only handles these.
    pub is_power: bool,
    pub is_closed: bool,
    /// The service exactly as the portal sent it.
    pub raw: Value,
}

impl Service {
    pub fn from_value(raw: &Value) -> Option<Self> {
        // Without an id nothing else can be asked about the service.
        let id = text_of(raw.get("accountServiceId")?)?;
        let has_closed_date = raw.get("closedDate").is_some_and(|d| !d.is_null());
        Some(Self {
            id,
            nmi: raw.get("siteIdentifier").and_then(text_of),
            post_code: raw.get("postCode").and_then(text_of),
            is_gas: field_is(raw, "serviceType", "gas"),
            is_power: is_power_type(raw),
            is_closed: has_closed_date || field_is(raw, "status", "closed"),
            raw: raw.clone(),
        })
    }
}

/// Whether the service's `serviceType` mentions power or electricity.
fn is_power_type(raw: &Value) -> bool {
    raw.get("serviceType")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .is_some_and(|kind| kind.contains("power") || kind.contains("electric"))
}

/// Reads the accounts and their services out of the `currentuser` data.
pub fn parse_accounts(data: &Value) -> Vec<Account> {
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
pub fn meter_list(data: &Value) -> Vec<Value> {
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
pub fn pick_meter<'a>(meters: &'a [Value], nmi: Option<&str>) -> Option<&'a Value> {
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

/// The JSON body for a `CostDetail` call.
pub fn cost_body(service: &Service, range: DateRange, is_smart: bool) -> Value {
    json!({
        "accountServiceId": service.id,
        "identifier": service.nmi,
        "from": DateRange::dash(range.from),
        "to": DateRange::dash(range.to),
        "isSmart": is_smart,
    })
}

/// The JSON body for a smart meter read (`accountservicetimezonesmartmeterread`).
pub fn smart_read_body(service: &Service, serial: &str, range: DateRange) -> Value {
    json!({
        "identifier": service.nmi,
        "serialNumber": serial,
        "fromDate": DateRange::slash(range.from),
        "toDate": DateRange::slash(range.to),
        "isSmart": true,
        "isAcrossAccount": false,
    })
}

/// Whether a meter is a "Basic" (accumulation) meter, which has no 5-minute data.
pub fn is_basic_meter(meter: &Value) -> bool {
    field_is(meter, "meterReadType", "basic")
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
        assert!(!services[0].is_closed && !services[0].is_gas && services[0].is_power);
        assert!(!services[1].is_power);
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
}
