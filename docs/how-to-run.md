# How to run solar-dash

These instructions are for Linux (Ubuntu).

## Prerequisites

```sh
sudo apt install build-essential pkg-config git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Restart your shell afterwards so `cargo` is on your `PATH`. The repo's
`rust-toolchain.toml` makes rustup install the right toolchain automatically.

## Get the code and configure

```sh
git clone <your-repo-url> solar-dash
cd solar-dash
cp config.example.toml config.toml
```

Edit `config.toml` and fill in your iSolarCloud developer-portal values
(appkey, access key, RSA public key, gateway for your region) and your normal
iSolarCloud login. Set `encrypt` to match the app's setting in the portal.
`config.toml` is gitignored.

## Run

Check your credentials:

```sh
cargo run -- login-test
```

Success looks like this (your plant id and name will differ):

```
Logged in. Found 1 plant
  1234567 My House
```

Add `RUST_LOG=debug` to also see the raw plant-list data.

### Troubleshooting

If `login-test` fails, it prints the error and exits with status 1. Check:

- `encrypt` in `config.toml` matches the app's encryption setting in the
  developer portal.
- `gateway` is the one for your region.
- The app has been approved in the developer portal.
- The appkey, access key and RSA public key were copied without extra spaces.

Error codes seen so far:

| Code | Meaning | Fix |
| --- | --- | --- |
| `E912` "Illegal x-access-key" | The access key was not accepted | Copy the app's Secret Key into `access_key` again |
| `E919` (no message) | Your iSolarCloud account is not on the app's whitelist | Add your account's email to the app's whitelist in the developer portal |

Keep the database up to date, and fill in older history in the background
(set `history_from` in `config.toml`, see [Backfill history](#backfill-history)):

```sh
cargo run -- serve
```

`serve` logs in. If iSolarCloud is down or the login fails, `serve` keeps running (dashboard and Modbus included) and retries the login after 1 minute, doubling up to every 30 minutes. Once logged in it syncs straight away and every 5 minutes after that
(about a minute after each 5-minute mark, when the data has landed). Stop it
with Ctrl+C. It makes about 12 calls an hour, about 8,900 a month, well inside
the API limits. If it was stopped for a while it catches up from where it left
off; on a new database it starts from midnight today and the background
backfill fills in everything before that. `serve` also runs the
[web dashboard](#web-dashboard).

Use a different config file with `--config path/to/file.toml`.
For more logging:

```sh
RUST_LOG=debug cargo run -- serve
```

## Web dashboard

`serve` also starts a web server (on `bind` in the `[server]` section, by
default `127.0.0.1:8080`). It is protected by a password, which you set once:

```sh
cargo run -- hash-password
```

Type the password twice (nothing is shown as you type). Paste the
`password_hash = "..."` line it prints into the `[server]` section of
`config.toml`. This command needs no config file, so you can run it first.

Also add a `session_secret` (required: `serve` refuses to start without it,
see `config.example.toml`). Make one with:

```sh
openssl rand -base64 64 | tr -d '\n'; echo
```

It prints 88 characters ending in `=`; copy just those. (The `echo` stops
zsh adding a `%` after the output, which isn't part of the secret.)

To call the API from a script without logging in, add an `api_token` to
`[server]` (at least 32 characters; `serve` refuses a shorter one):

```sh
openssl rand -hex 32
```

Scripts then send `Authorization: Bearer <token>` on `/api/` requests. It is
accepted on `/api/` routes only, only for `GET` and `HEAD` (anything that
changes data needs the login cookie, see [Admin API](#admin-api)), never gives
a cookie, and is ignored when `api_token` is not set. Keep `config.toml` private; the token is never logged.

When you open the dashboard at `http://127.0.0.1:8080` (plain http), also set
this in `[server]`, or the browser will drop the login cookie:

```toml
cookie_secure = false
```

`cookie_secure` defaults to `true`, which is right behind Cloudflare Tunnel
(https). Use `false` only for plain-http testing on your own machine or LAN.

Then run `cargo run -- serve` and open <http://127.0.0.1:8080>. After 5 wrong
passwords in 15 minutes from the same address, logins from it are refused for
15 minutes. A login lasts 14 days from your last visit, and logging out ends
it on the server too.

### Dashboard views

The home page (the solar-dash name at the top left) shows Now, with History
below it; the tabs are Money, Payback and Cars (EV vs petrol). History has Day, Week
(Monday to Sunday), Month, Year, Lifetime and Custom tabs, a date pill with a
picker, and four total cards above the chart. The address keeps the period
(`#day=2026-10-20`, `#week=2026-10-19`, `#month=2026-10`, `#year=2026`,
`#lifetime`, `#custom=2026-09-01..2026-10-22`), so refresh and bookmarks
work (the old `#now` and `#history` addresses open home). Clicking a bar opens
that day, month or year. Money has a Month | Year | All time switch
(`#money=month:2026-09`, `#money=year:2025`, `#money=all`; `#money` is this month):
what you saved, a bill-style breakdown, where the saving came from, a bar chart
that opens the day, month or year you click, the car, and, with a `[globird]` section,
a Bill check card that sets GloBird's own bill beside our estimate for the same period
(the last 400 days at most). With a `[globird]` section, the Money tab and Payback use
GloBird's own figures for every day it has billed completely, and our estimate for
the rest; a line under the saving says how many days are which. The old `#savings` and
`#cost` addresses open it. All times are shown in Australia/Adelaide.

- **Now** (top of the home page): solar and load power, battery, and today's energy totals. With
  [Modbus](#local-modbus-optional) running it shows the inverter's live values
  (solar, load, grid importing or exporting, battery charging or discharging,
  and battery %), refreshed every 15 seconds. Without a sample under 5 minutes
  old it falls back to the iSolarCloud readings, refreshed every minute. A line
  at the top says which source it is and how old the reading is.
- **History > Day**: a power chart for one day, today's until you pick another (pick a date or step with the arrows).
  Solar is an area, home a line, and battery and grid are signed areas: battery
  above zero is supplying and below is charging, grid above zero is buying and
  below is selling. All are in kW, and battery % (dashed) is on the right axis.
  Free import windows are shaded green, peak ones red, and the best export
  window (Super Export) a little stronger. A two-row Buy/Sell strip under the
  chart shows the tariff, and the hover tooltip names it for that moment. For
  today the chart ends at the current time. The windows and rates come from
  `GET /api/tariff` for the day shown, so they follow the `[tariff]` (and any
  `[[previous_tariff]]`); without a `[tariff]` the shading and strip are left
  out. Energy totals
  for the day are underneath. The chart uses the inverter's 1-minute data when
  there is at least 30 minutes of it for that day, and iSolarCloud's 5-minute
  data otherwise. If Modbus started part way through the day, iSolarCloud fills
  the time before it. A note under the chart names the source. The totals
  always come from iSolarCloud. While it shows today it refreshes by itself, and a Smooth
  button averages the lines over a few minutes.
- **History > Week, Month, Year, Lifetime, Custom**: grouped columns per bucket
  (a day, month or year): Solar; Grid, with energy bought above zero stacked by
  tariff band (free, shoulder, peak) and energy sold below it (Super Export, then
  the lower rates); Battery, out above and in below; and Home. A bucket with no
  tariff split (an older day without interval data) draws plain bought and sold in
  the grid colours. The tooltip lists each part in kWh, and a click opens the
  bucket. On a phone a chart with many buckets scrolls sideways. The Grid card
  has a thin free/shoulder/peak bar and a line such as "72% bought free, 21%
  shoulder, 7% peak" (hidden without a split).
- **History > Chart / By tariff** (all periods): By tariff is a table of the
  period's grid energy by band: time of day, rate, kWh bought, kWh sold and cost
  (kWh times the band's rate, less what the same band earned from export), then
  the supply charge, the zero-import credit when one was earned, and a total.
  Times and rates come from `/api/tariff`. Each day is costed at its own plan
  when the period spans a `[[previous_tariff]]` change (the times and rates shown
  are the current plan's, and a note says so). The total normally equals the
  Cost card (`/api/costs` `actual`); it differs on days GloBird has billed (the
  card uses its bill) and where a band has more than one rate (costed at the
  average), and a note under the table says by how much.

The charts use [ECharts](https://echarts.apache.org) 5.6.0 (Apache-2.0),
loaded from cdnjs by a single pinned `<script>` tag in `static/index.html`. The
browser therefore needs internet access to cdnjs for the charts; without it
the tiles and totals still work and a banner says the charts could not load.
To serve ECharts yourself instead, put `echarts.min.js` in `static/` and change
that one tag.

The JSON API (all of it needs a login, except `/healthz`; without one `/api/*`
answers 401). Times are unix seconds (UTC), dates are local dates in your
`timezone`:

| Endpoint | Returns |
| --- | --- |
| `GET /healthz` | `{"ok":true}` (public) |
| `GET /api/latest` | The newest value of every point, with its name and unit |
| `GET /api/series?points=83033,83106&from=<unix>&to=<unix>` | Raw `[ts, value]` pairs per point. At most 20 points and 31 days |
| `GET /api/day?date=YYYY-MM-DD` | One local day (default today), with its `start` and `end` as unix seconds: PV, load and battery % plus grid and battery power worked out from the energy counters. Every series has a `key` such as `pv_w` or `grid_import_w` |
| `GET /api/live` | `{"sample": {ts,pv_w,load_w,grid_w,battery_w,soc_pct}, "car": {...}}`. `sample` is from Modbus, or `null` if there is none or it is over 5 minutes old. `car` is the Tesla's charge state from TeslaMate (see [TeslaMate](#teslamate-optional)), or `null` if that is off or nothing has arrived yet |
| `GET /api/live/series?from=<unix>&to=<unix>` | Modbus samples averaged per minute: `{"data":[[ts,pv_w,load_w,grid_w,battery_w,soc_pct],...]}`. At most 2 days |
| `GET /api/ev/sessions?from=<unix>&to=<unix>` | The Tesla's charging sessions that overlap the range, oldest first: `{"sessions":[{id,started_at,ended_at,energy_added_kwh,energy_used_kwh,start_soc_pct,end_soc_pct,max_power_kw,geofence,solar_kwh,battery_kwh,grid_kwh,cost,solar_value,coverage},...]}`. A session still charging has `ended_at` `null` and counts as running until now. `energy_used_kwh` is taken from the wall, `energy_added_kwh` reaches the battery. The last six fields (kWh by source, dollars, and the share 0 to 1 of the session the inverter samples cover) are `null` without a `[tariff]`, away from home (geofence not "Home") or with no energy. At most 400 days. Empty without a `[teslamate]` section |
| `GET /api/ev/summary?from=YYYY-MM-DD&to=YYYY-MM-DD` | The car against petrol, by month and in total: `{"configured":true,"months":[{month,km,home_kwh,home_cost,away_kwh,away_cost,charging_cost,petrol_litres,petrol_cost,saved,ev_cents_per_km,petrol_cents_per_km},...],"total":{...},"petrol_cents_per_litre":...,"petrol_litres_per_100km":...}`. Local dates, both included, at most 400 days; without `from` it covers the last 365 days. Money is in dollars. `away_cost` is `null` when there were away sessions and no `away_cents_per_kwh`, and the `*_cents_per_km` fields are `null` for 0 km. `configured` is `false`, and the money fields `null`, without a `petrol_cents_per_litre`. See [TeslaMate](#teslamate-optional) |
| `GET /api/ev/cars` | Each TeslaMate car's fuel savings over its whole synced history, newest car first: `{"configured":true,"petrol_litres_per_100km":...,"petrol_grade":"Unleaded 91","charging_by_plan":[...],"away":{...},"self_share_pct":...,"cars":[{car_id,name,model,label,trim,paint,wheels,marketing_name,compare_with,first_day,last_day,total,months:[{month,...}],charging_by_plan,away,self_share_pct}]}`. `charging_by_plan` is `[{plan,first_day,last_day,sessions,kwh,cost,pending_sessions}]`, `away` is `{sessions,kwh,cost}` (`cost` `null` without `away_cents_per_kwh`) and `self_share_pct` is solar and battery kWh over costed home kWh; the top-level ones add up all cars. See [Comparison cars and charging by plan](#comparison-cars-and-charging-by-plan). Each period has `km`, `petrol_litres`, `petrol_cost`, `home_kwh`, `away_kwh`, `away_cost`, `away_uncosted_kwh`, `with_system` and `without_system` (`charging_cost`, `saved`, `cents_per_km`), `system_added`, `grid_kwh`, `self_kwh`, `uncovered_kwh`, `feed_in_forgone` and `pending_sessions`. Dollars. Empty until the history sync has run. See [Car versus petrol](#car-versus-petrol) |
| `GET /api/admin`, `POST/PUT/DELETE /api/admin/...` | Read and edit the tariff plans, purchases, bills and settings. See [Admin API](#admin-api) |
| `GET /api/petrol-prices` | The petrol prices: `{"fallback_cents_per_litre":...,"prices":[{"from":"YYYY-MM-DD","cents_per_litre":...},...]}`, oldest first. See [Petrol prices](#petrol-prices) |
| `PUT /api/petrol-prices` | Adds or replaces the price starting on a date. JSON body `{"from":"YYYY-MM-DD","cents_per_litre":189.9}`; answers like the `GET`. 400 for a bad date or price, 415 unless the body is `application/json` |
| `DELETE /api/petrol-prices/YYYY-MM-DD` | Removes the price starting on that date and answers like the `GET`. 404 if there is none |
| `GET /api/daily?from=YYYY-MM-DD&to=YYYY-MM-DD` | kWh totals per local day (default: the last 30 days). At most 400 days. Each day has a `source` (see [Modbus in `serve`](#modbus-in-serve))Every day (and every bucket and the `totals`) also has the grid energy split by tariff band: `import_free_kwh`, `import_shoulder_kwh`, `import_peak_kwh`, `export_super_kwh`, `export_peak_kwh`, `export_other_kwh` and `export_tier_kwh`. The bands come from the `[tariff]` (the `[[previous_tariff]]` in effect that day), told apart by price: import at 0 c is free, dearer than `default_import_cents` is peak, the rest shoulder; export at or below `default_export_cents` is other, and of the better rates the highest is super (when a lower one exists, which is peak). With an `export_tier` (a daily "first N kWh at X c" feed-in), the first N kWh exported each local day, in time order, are `export_tier_kwh` whatever the time (an interval that crosses N is split), and only the exports after that are classed super, peak or other. They are summed from the same intervals `/api/costs` prices (live samples when they cover the day, else the 5-minute cloud counters), so they add up to its `import_kwh` and `export_kwh`, and to `/api/daily`'s own totals within rounding (the cloud's daily counter can differ a little from its 5-minute steps). They are `null` without a `[tariff]` or for a day with no interval data |
| `GET /api/costs?from=YYYY-MM-DD&to=YYYY-MM-DD&group=day\|month\|year` | Dollars from the `[tariff]` section. `totals` is a bill-style breakdown: `supply`, `import` (and `import_kwh`), `feed_in` (and `export_kwh`), `credit` (zero-import credit) and `credit_days`, `actual` (paid), `baseline` (no solar or battery), `savings`, and `not_bought` (baseline less supply and import; `savings` = `not_bought` + `feed_in` + `credit`), plus `load_kwh` and `days` with data. `billed_days` is how many of those days use GloBird's own bill instead of our estimate (see below). With `group=day` (the default) `days` has one row per local day with `source` (`inverter` or `cloud` for our estimate, `globird` for a day billed by GloBird), `actual`, `baseline`, `savings`, `credit`, `supply`, `import`, `feed_in` and the import, export and load kWh. With `group=month` or `year`, `buckets` has one row per period (`period` is `2026-09` or `2026`) with the same fields as `totals`, so a year is 12 small rows. Default: the last 30 days. At most 1,500 days per day-grouped request, 20 years for month or year. Days with no data are left out. Finished days are stored in the database, so repeat requests are fast; editing the `[tariff]` rates makes them be worked out again (editing a `[[previous_tariff]]` only redoes the days on that plan). Days before the current plan are priced on the `[[previous_tariff]]` entry in effect on that day (its `until` is the last day it applies), if any are configured. A day GloBird has billed completely (see [GloBird](#globird-optional)) shows GloBird's figures: `import`, `feed_in`, `supply`, `credit` and `actual` are its lines added up, the import and export kWh are its meter's (when it has them), and `savings` is our `baseline` less that bill; `load_kwh` and `baseline` stay ours, and `source` is `globird`. Other days, and days GloBird has not finished posting, keep our estimate. The stored days are always our estimate; GloBird's figures are applied each time. A day with no data of ours is still left out. Car fuel savings: `cars_available` is true when TeslaMate cars are synced and a petrol price is configured; then `totals` and every day or bucket also has `cars` (`km`, `petrol`, `charging`, `fuel_saved`, `estimated_sessions`; zeros when the cars did not run), and `totals.cars` is the sum of the days returned, so rows and total add up; car activity on a day with no house data (no cost row) is left out. `fuel_saved` is petrol cost less what the charging would cost from the grid with no system (home sessions at their baseline, a home session not costed yet at the plan's average import price, counted in `estimated_sessions`, away sessions at `away_cents_per_kwh`). It is what to add to `savings` to include the cars: the no-solar `baseline` already prices the cars' charging at grid prices, so the system's share of that is in `savings` already. Without `cars_available`, `cars` is left out. Without a `[tariff]` section it answers `{"configured":false}` |
| `GET /api/tariff?date=YYYY-MM-DD` | The plan in force on that local day (default today; the `[[previous_tariff]]` in effect that day, else `[tariff]`), so the page needs no copy of the config: `supply_cents_per_day`, `default_import_cents`, `default_export_cents`, the `import` and `export` windows (`from`, `to` as `HH:MM`, `cents`, and `band`) `zero_import_credit` (`from`, `to`, `max_kwh`, `credit_cents`, or `null`) and `export_tier` (`first_kwh_per_day`, `cents`, or `null`). `band` uses the same price rule as the `/api/daily` split: `free`, `shoulder` or `peak` for import; `super`, `peak` or `other` for export. `default_import_band` and `default_export_band` say the same for the default rates. Without a `[tariff]` section it answers `{"configured":false}` |
| `GET /api/globird/compare?from=YYYY-MM-DD&to=YYYY-MM-DD` | GloBird's own daily bill next to our estimate, from the synced `globird_*` tables and the `[tariff]`. `days` has one row per local day GloBird has cost lines for: `complete`, `globird` (dollars `import`, `export`, `supply`, `credit`, `other` and `total`, signed as GloBird signs them, so feed-in and credits are negative; `import_kwh` and `export_kwh`; and its `lines`, each with `category`, `amount` and `quantity`), `ours` (the same fields from `/api/costs`, with `source`, always our own estimate even where `/api/costs` shows GloBird's bill) and `diff` (ours minus GloBird's `total` and kWh). `ours` and `diff` are `null` when there is no `[tariff]` or no data of ours for that day. `totals` adds up the days that are `complete` and have both sides (`days_compared`), and `categories` totals each GloBird category over the range (`amount`, `quantity`, `days` and the `bucket` it counts in), which shows new categories such as "Critical Peak - Export". Default: the last 31 days, at most 400. With no GloBird data it answers `{"configured":...,"days":[],...}`, where `configured` is whether there is a `[globird]` section |
| `GET /api/payback` | How much of the system's cost the savings have covered, from the `[[system.purchase]]` entries and the `[tariff]`: `total_cost`, `saved`, `net`, `recovered_pct`, `rate_per_day` (mean savings over the last 365 days with data), `price_rise_percent` (the `[system]` setting, default 3, that grows that rate each year in the projection), `projected_payback` (`"YYYY-MM"`, `null` once paid back, or `"never"`), the `purchases`, a monthly `series` of cumulative `saved` and `net`, and `by_year` (`partial` when a year has fewer days of data than days). Also `last_12_months_saved` (dollars saved over the same 365 days as `rate_per_day`); `parts`, one per purchase oldest first, with `name`, `date`, `cost`, `saved`, `estimated`, `recovered_pct`, `net` and `projected_payback` (the same format as the top-level one, from that part's own last-365-day rate; `saved`, `recovered_pct` and `net` are `null` for a later purchase with no `battery_kwh`, whose savings stay with the first, and then so is `projected_payback`). A later purchase with a `battery_kwh` gets an estimate (`estimated` is true): on each day from its date with a battery discharge figure (the one `/api/daily` reports), the discharge beyond 95% of the battery capacity owned before it, at that day's highest import rate; days without a figure add 0. The first purchase gets the total saved less those estimates, so the parts add up; `without`, `null` unless the last purchase has a `battery_kwh` and is not the first, else `name` (that purchase), `series` (like `series` but leaving out its cost and its estimated savings), `rate_per_day` and `projected_payback`; and `timeline`, sorted by date, of `{date, kind ("purchase" or "plan"), label, cost}` for every purchase and every named plan (the `name` of a `[[previous_tariff]]` or of `[tariff]`, dated from the day after the plan before it ended; the first plan is dated from the first purchase). Counts from the first purchase date to today, using GloBird's bill for the days it has billed (as `/api/costs` does). Without purchases or a `[tariff]` section it answers `{"configured":false}` |

For example, once logged in with a browser, try
<http://127.0.0.1:8080/api/latest>. With curl, log in and keep the cookie (needs `cookie_secure = false` over plain http):

```sh
curl -c cookies.txt -d 'password=YOUR_PASSWORD' http://127.0.0.1:8080/login
curl -b cookies.txt http://127.0.0.1:8080/api/latest
```

## Local Modbus (optional)

Besides the iSolarCloud data, solar-dash can read live power straight from the
inverter over your home network (Modbus TCP, through the WiNet-S dongle). It
is optional: without a `[modbus]` section nothing changes.

1. Enable Modbus TCP on the WiNet-S. Open the dongle's own web page (its IP
   address in a browser), go to the advanced/communication settings and turn
   on Modbus TCP with **port 502**, **SSL/TLS off**, and the **IP whitelist
   off** or including the machine running solar-dash. Menu names differ a
   little between firmware versions.
2. Add the section to `config.toml` (see `config.example.toml`). Only `host`
   is required; the rest are the defaults:

   ```toml
   [modbus]
   host = "192.168.1.60"   # the WiNet-S IP address
   port = 502
   unit = 1
   interval_secs = 15
   ```

3. Try one read:

   ```sh
   cargo run -- modbus-test
   ```

   It prints one line like
   `PV 4.90 kW, load 0.71 kW, grid 0.00 kW (import), battery 4.23 kW (charging), SOC 43.4%`.
   Grid is "import" when buying from the grid and "export" when selling.

4. Test how stable the dongle is by leaving this running for a few hours:

   ```sh
   cargo run -- modbus-test --watch
   ```

   It reads every `interval_secs` and prints a line with the time for each
   sample. A failed read prints `FAILED: ...`, then it reconnects and carries
   on. Ctrl+C stops it and prints a summary such as `240 ok, 3 failed (1.2%)`.

The dongle is known to stall or drop connections, so each request has a 3
second timeout and a failure just means a reconnect on the next read.

### Modbus in `serve`

When `[modbus]` is configured, `serve` also polls the inverter every
`interval_secs` and stores each sample in the `live_sample` table. Failures
never stop `serve`: the first one is logged as a warning, then one in every 20
in a row, and `Modbus back` is logged when reads work again. Without a
`[modbus]` section `serve` behaves exactly as before. The samples are served
by `/api/live` and `/api/live/series` (see [the API table](#dashboard-views)).

Daily energy: when the live samples cover at least 90% of a local day (for
today, 90% of the time since midnight), `/api/daily` adds them up itself
instead of using the iSolarCloud counters, and sets `"source": "inverter"`
(otherwise `"cloud"`). Gaps over 2 minutes are not counted, and
`self_consumed_kwh` is `null` for those days.

Signs in the samples: `grid_w` is positive when importing and negative when
exporting, and `battery_w` is positive when charging and negative when
discharging.

Storage: at the default 15 seconds that is 5,760 rows a day, about 2 million a
year, roughly 80 MB a year. Everything is kept; nothing is deleted
automatically.

## TeslaMate (optional)

If you run [TeslaMate](https://github.com/teslamate-org/teslamate), solar-dash
can show the Tesla's charge state next to the solar numbers. It reads the
values TeslaMate already publishes to its MQTT broker, so it never talks to
Tesla itself. It is optional: without a `[teslamate]` section nothing changes.

1. Make sure the broker is reachable from the machine running solar-dash
   (TeslaMate's compose file usually runs Mosquitto on port 1883). To see
   what TeslaMate publishes, subscribe to the car's topics:

   ```sh
   mosquitto_sub -h 192.168.1.70 -t 'teslamate/cars/1/#' -v
   ```

   You should see lines like `teslamate/cars/1/battery_level 43`. TeslaMate
   retains these, so they appear at once, even while the car is asleep.

2. Add the section to `config.toml` (see `config.example.toml`). Only `host`
   is required; the rest are the defaults:

   ```toml
   [teslamate]
   host = "192.168.1.70"   # the MQTT broker
   port = 1883
   car_id = 1
   topic_prefix = "teslamate"
   # username = "..."       # only if the broker needs a login,
   # password = "..."       # and then both
   ```

3. Start `serve` and open `/api/live` while logged in. The `car` part holds
   `display_name`, `state`, `plugged_in`, `charging_state`,
   `charger_power_kw`, `charger_actual_current_a`, `charger_voltage_v`,
   `battery_level_pct`, `usable_battery_level_pct`,
   `charge_energy_added_kwh`, `charge_limit_soc_pct`,
   `time_to_full_charge_h`, `geofence` and `updated_ts` (unix seconds of the
   last message). A value TeslaMate has not sent, or sent empty, is `null`.

Charging sessions are saved in the database (table `ev_charge_session`, shown
by `GET /api/ev/sessions`). A session starts when `charging_state` becomes
"Charging" and ends when it becomes anything else. While charging, its energy,
battery level and top power are saved every 5 minutes, so the SSD sees only a
few writes an hour. If solar-dash restarts mid-charge it carries on with the
open session; if the car stopped charging in the meantime, the session ends at
the time solar-dash noticed. The energy used from the wall is worked out by
adding up the charger power between messages (never counting more than 5
minutes for one reading), so it includes charging losses.

With a `[tariff]`, a home session (geofence "Home", or none) is also costed
when you ask for it, from the 15-second inverter samples. The car's power is
already part of the house load, so for each sample the car takes its average
power (never more than the load) and the same share of solar, battery and
grid as the whole house load has: solar first, then battery, then grid. The
grid part is priced at that time's import price, so a midday charge in a
free window costs $0. `solar_value` is what that solar would have earned
exported instead, and is not part of `cost`. Gaps in the samples over 2
minutes are left out, and `coverage` says how much of the session was seen.

### Car versus petrol

The car's odometer is saved as one row per local day (table
`ev_odometer_daily`, in the plant's time zone): the first reading of a new day
is written at once, then the day's row is updated at most every 30 minutes,
and only when the reading changed, so a parked car writes nothing. The km on a
day are its reading minus the previous saved day's. A day with no reading has
no row; its km land on the next day with one.

To compare charging with petrol, set these (see
[Where the settings live](#where-the-settings-live); they are no longer
`config.toml` keys):

- `petrol_cents_per_litre` (no default: the comparison is off without it)
- `petrol_litres_per_100km` (a comparable petrol car, default 7.5)
- `away_cents_per_kwh` (a flat price for charging away from home)

For the per-car history (below) the petrol price can change over time. Each
change is a petrol price row: a date (`YYYY-MM-DD`) and cents per litre, for
example 185.0 from 2025-01-01 and 199.0 from 2026-03-01. A day uses the row
with the latest date on or before it, and the days before the first row use
`petrol_cents_per_litre`. Without that, those days have no price and the
petrol cost stays empty. The single-car summary above keeps using
`petrol_cents_per_litre` only. The rows are listed and changed through
[`/api/petrol-prices`](#petrol-prices).

South Australian unleaded averages roughly 190-200 c/L in 2026; check a fuel
price app for what you pay. The Money tab then shows a car
panel for the month, year or (for All time) the last 12 months. It is hidden until the car has
km or charging recorded, and it says which key to set if there is no petrol
price. `GET /api/ev/summary` gives the same numbers.

- Charging cost is the home sessions (costed as above, so it needs a
  `[tariff]`) plus the away sessions at `away_cents_per_kwh` times the energy
  used (or added, if used is missing). Without `away_cents_per_kwh` away
  sessions are listed in `away_kwh` but not costed, and they do not count in
  `charging_cost`, so the saving is a little generous if you charge away.
- Petrol cost is the km times `petrol_litres_per_100km` times the price.
  Saved is petrol cost minus charging cost. The cents per km are those
  costs over the km.
- A session counts in the month it started.

#### Comparison cars and charging by plan

Two more settings feed the "EV versus petrol" view of `GET /api/ev/cars`:
`compare_cars` (for example `{"3": "Hyundai i30 Sedan N", "Y": "Toyota RAV4 GX 2WD"}`)
and `petrol_grade` (default "Unleaded 91").

`compare_cars` maps TeslaMate's model string to the name of the petrol car it
is compared with (the key is matched ignoring case); a model with no entry has
`compare_with` `null`. `petrol_litres_per_100km` stays one setting shared by all
cars. `petrol_grade` is returned as is.

Home charging is also split by the plan that applied on the day each session
started: `charging_by_plan` has one row per plan with home sessions, oldest
first, with the plan's `name` (without
one it is "Plan until <date>", "Plan from <date>" or "Plan <date> to <date>").
`sessions`, `kwh` and `cost` (dollars paid with the system) count costed
sessions only; sessions not costed yet are in `pending_sessions` and never in
the money. Days where Amber was billed use the bill's average price, as above.
The car's details (`trim`, `paint`, `wheels`, `marketing_name`) come from
TeslaMate's `cars` table and are `null` where TeslaMate has nothing.

#### Petrol prices

The petrol price changes over time are rows in the database (table
`petrol_price`), listed and changed through the API:

```sh
TOKEN=...   # the api_token from config.toml (reads only)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/api/petrol-prices
# Changes need the login cookie:
curl -c cookies.txt -d 'password=YOUR_PASSWORD' http://127.0.0.1:8080/login
curl -b cookies.txt -X PUT -H 'Content-Type: application/json' \
  -d '{"from":"2026-10-23","cents_per_litre":189.9}' http://127.0.0.1:8080/api/petrol-prices
curl -b cookies.txt -X DELETE http://127.0.0.1:8080/api/petrol-prices/2026-10-23
```

A change applies at once, without a restart, and is written to the change log
(`admin_change`). The write routes need the login cookie, not the token. The
PUT must send `Content-Type: application/json`, the cookie is `SameSite=Lax`,
and a write whose `Origin` header names another host is refused with 403, so
another website cannot change prices through a visitor's browser.

An older config's `petrol_prices_file` (a `from,cents_per_litre` CSV) or
`[[teslamate.petrol_price]]` tables are imported into the table once; see
[Where the settings live](#where-the-settings-live).

#### Per-car savings over the whole history

`GET /api/ev/cars` covers every car in the synced TeslaMate history (see
[TeslaMate history](#teslamate-optional)), not just the one followed over
MQTT, from its first drive or charge to the last. It is empty until the
history sync has run. Each car has a `label` ("Model Y", else its name, else
"Car 3"; the name is added when two cars share one), `first_day` and
`last_day`, a `total` and a `months` list (oldest first) of the same figures.
Cars are listed newest first, and a car with no drives and no charges is left
out.

- Petrol is each day's km times `petrol_litres_per_100km` times that day's
  price, from the petrol price rows.
- Home is the geofence "Home"; any other geofence, or none, is away. Away
  sessions cost `away_cents_per_kwh` per kWh; without it `away_cost` is `null`,
  `away_uncosted_kwh` says how much was left out, and it counts as nothing.
- `with_system` is the charging cost as the home sessions were costed (solar
  and battery first) and `without_system` the same with all home charging at
  the baseline (no solar or battery). Each has `charging_cost`, `saved`
  (petrol cost less charging cost) and `cents_per_km`. `system_added` is the
  difference, what the solar and battery saved on the car.
- `grid_kwh`, `self_kwh` (solar and battery), `uncovered_kwh` and
  `feed_in_forgone` come from the costed home sessions.
- Home sessions that are not costed yet are counted in `pending_sessions` and
  left out of the money and of those four figures, so the savings read low
  until they are.
- `configured` is `true` with a `petrol_cents_per_litre`, any
  petrol price row. Without a petrol price `petrol_cost` and
  `saved` are `null` but the charging costs still show.

The rest of the state is only kept in memory (the broker keeps the latest
values, so they come back after a restart). Unlike the Modbus sample, `car` is not hidden
when it gets old: TeslaMate's values stay true while the car sleeps. Use
`updated_ts` to see how fresh they are.

If the broker is down, `serve` keeps running. The first failure is logged as
a warning, then it retries after 1 second, doubling up to every 60 seconds,
and `TeslaMate MQTT back` is logged when it reconnects.

### History sync

The MQTT feed only knows the present. To keep the past too, give `[teslamate]`
the address of TeslaMate's Postgres database:

```toml
[teslamate]
host = "192.168.1.70"
database_url = "postgres://solar_dash:change-me@192.168.1.70:5432/teslamate"
```

Without `database_url` this is off. With it, `serve` copies summaries (never
TeslaMate's raw rows) of every car's history into its own database, 30 seconds
after startup and then every 15 minutes:

- `tm_car`: the cars.
- `tm_drive_daily`: km and number of drives per car per local day (in your
  `timezone`), added up in Postgres.
- `tm_charge`: one row per charging session, with energy, battery levels, the
  geofence name and whether a fast charger was used.

It backfills by itself: the first run copies the whole history, and later runs
only look at new or unfinished drives and sessions and the last 3 days. Nothing
is written to TeslaMate. The URL holds a password, so it is never logged.

With a `[tariff]` too, each session at Home (the geofence name) is costed once
it has been over for 2 hours, and the result is stored on its `tm_charge` row.
The car's power for the session is read from TeslaMate for that and not kept.
The car sits inside the house load, so in each stretch of time it gets the
house's share from the grid at that time, and the rest counts as solar or
home battery. The grid part is priced on the plan of the day; on days covered by
a `[bills]` entry (say Amber, which has no prices per interval) it is priced at
that bill's average import price instead. Time the house data does not cover
counts as grid, and each row says how much that was (`uncovered_kwh`). A session
the house data covers by less than 98% waits up to 3 days for it to arrive. A
run costs at most 50 sessions, so the first backfill takes a few runs.
Sessions are costed again if the tariff or the bills change. Sessions with no
geofence or somewhere else are not costed.

solar-dash only needs to read, so make it a read-only login in TeslaMate's
database (for example with `psql` as the `postgres` user):

```sql
CREATE ROLE solar_dash LOGIN PASSWORD 'change-me';
GRANT CONNECT ON DATABASE teslamate TO solar_dash;
GRANT USAGE ON SCHEMA public TO solar_dash;
GRANT SELECT ON cars, drives, charging_processes, charges, geofences TO solar_dash;
```

TeslaMate's compose file does not publish its database to the network by
default. Publish port 5432 of the database container on the LAN (a `ports:`
entry such as `"5432:5432"`), and keep it off the internet. The connection is
not encrypted, which is fine on a home network.

If the database cannot be reached, a warning is logged and the next run tries
again; `serve` keeps running.

## GloBird (optional)

solar-dash can log in to your GloBird Energy customer portal
(myaccount.globirdenergy.com.au) and keep its daily cost and usage in the
database. The portal has no official API: this uses the same calls its web
pages make, learned from the
[bolagnaise/globird-ha](https://github.com/bolagnaise/globird-ha) Home
Assistant integration. It is read-only, but it may break whenever GloBird
changes the portal. Without a `[globird]` section nothing changes.

1. Add your portal login to `config.toml` (see `config.example.toml`). The
   password is stored in plain text, so keep the file's mode at 600:

   ```toml
   [globird]
   email = "you@example.com"
   password = "your-portal-password"
   # Optional: local time of the daily sync. Default "09:00".
   sync_time = "09:00"
   ```

2. Start `serve` as usual. With a `[globird]` section it also runs the GloBird
   sync, next to the other background tasks. Nothing else needs doing.

### What the sync does

About 30 seconds after `serve` starts, and then every day at `sync_time`
(local time, in the `[isolarcloud]` time zone), it logs in, lists your
services and syncs each electricity service that has a smart meter. Gas
services and services with only a Basic meter are skipped.

- **Backfill.** The first time, it walks back from today in 31-day windows
  until the portal has nothing older (or three years), pausing a second
  between calls (a few months of history takes about a minute). Progress is saved after each window, so a restart carries on, and
  once it has finished it never runs again.
- **Recent days.** Every run then fetches from a week before the newest
  complete day up to today and saves the days over the stored ones, because
  GloBird can revise a day after posting it.

A day is *complete* once GloBird has posted any charge other than the daily
supply charge (which it posts first). The data lags by a day or more, so if
yesterday is still incomplete after a run, the sync tries again every 2 hours
until 23:00 and then waits for the next `sync_time`. A network or portal
error is logged as a warning and retried the same way. Each run logs in once
(it keeps no session between runs) and logs one line, for example:

```
GloBird sync: 9 days written, newest complete day 2026-10-21, incomplete days: 2026-10-22
```

Complete days are also used in place of our estimate: the Money tab and
Payback show GloBird's own figures for them (`source` is `globird` in
`/api/costs`). The Bill check still compares GloBird with our estimate.

**Captcha.** If GloBird asks for a captcha, login fails and the sync logs an
error. Log in once in a browser at the portal, and the sync works again at the
next `sync_time`. It does not retry before then, so it cannot make GloBird
more suspicious.

### The tables

Each day is saved as a whole: the day's old rows are deleted and the new ones
inserted in one transaction, so a line GloBird revised or removed does not
linger. Dates are local days (`YYYY-MM-DD`) and `fetched_at` is unix seconds.

| Table | One row per | Holds |
| --- | --- | --- |
| `globird_cost` | day and charge category | `amount_cents` (GST included, negative is a credit), `quantity` (kWh), `meter_read_missing` |
| `globird_usage` | day, meter register and charge period | `kwh`, `charge_type` such as "Peak Usage", `category_code`, `quality` |
| `globird_interval` | day and meter register | `direction` (`import` for registers starting with E, `export` for B), `kwh_total` and `intervals`, a JSON array of the 288 five-minute kWh values |

The charge categories are GloBird's own text ("USAGE", "SOLAR", "SUPPLY",
"ZEROHERO Credit", ...) and are stored as they come. The tables have no
service column, so they assume one electricity service. It is not yet checked
how the 288 intervals line up with local clock time (compare them with our own
inverter samples before relying on that). `GET /api/globird/compare` reads the tables
and sets each day against our own estimate (see the API table above); the
dashboard does not show it yet. Its kWh figures are the sums of the
`globird_interval` readings for each direction, or, for a day with none, the
sum of that direction's `globird_usage` charge periods.

### Looking at what the portal returns

`globird-probe` logs in, calls each endpoint once and saves what came back, so
you can see the real response shapes:

```sh
cargo run -- globird-probe
```

On the deployed server (see [deploy.md](deploy.md)):

```sh
sudo -u solar-dash /opt/solar-dash/solar-dash --config /etc/solar-dash/config.toml globird-probe --out /tmp/globird-probe.json
```

`--out` sets the file (default `globird-probe.json`) and `--days` how many
days of usage and cost to ask for (default 31). It calls each endpoint once
with a short pause between calls, so it takes a minute or so. A failing
endpoint is recorded as an error in the file and the rest carry on.

It prints the file's path and, per service, the number of usage and cost
rows, the `chargeCategory` values seen, the length of the first `usageArray`
(48 or 288 intervals?) and how many cost rows come back for windows one and
two years ago (how far back history goes). If GloBird asks for a captcha,
login fails with a message saying so; see the captcha note above.

The file is written with mode 600 and names, addresses, account numbers, NMIs,
meter serial numbers, tokens and payment descriptions are replaced with
`REDACTED`. The redaction goes by field name, so open the file and check it
before sharing it.

## Past bills (optional)

For a past period you may have no usable tariff (a spot-price retailer, say) or
gaps in the meter data (cloud sync was off for a while). If you still have the
bills, enter them as bills in the CSV format below. They are kept in the
database (table `bill`). An older config's `[bills] file = "..."` is imported
once at start-up; see [Where the settings live](#where-the-settings-live).

Every day a bill covers then uses the bill's real total in Money and Payback,
spread evenly over the bill's days. The Bill check still compares GloBird with
our own estimate only. A bad file stops the import with an error naming the
line.

The file has a header line, then one line per bill. Blank lines and lines
starting with `#` are ignored. Periods must not overlap.

```csv
# Neutral example numbers.
from,to,bill_total,excluded,import_kwh,import_dollars,export_kwh,export_dollars,supply_dollars,note
2026-01-01,2026-01-31,152.40,0,610,190.00,280,22.40,48.50,summer bill
2026-02-01,2026-02-28,98.00,25.00,540,170.00,310,25.00,44.00,includes a $25 rebate
```

| Column | Meaning |
| --- | --- |
| `from`, `to` | First and last local day of the bill (inclusive), `YYYY-MM-DD`. |
| `bill_total` | Dollars billed. A credit is negative. |
| `excluded` | Dollars of rebates (say government relief) on the bill. They are added back (`bill_total + excluded`) so they do not count as savings from your solar. Use `0` if none. |
| `import_kwh`, `import_dollars` | Billed grid import and its usage charges. |
| `export_kwh`, `export_dollars` | Billed export and its credits (a positive number). |
| `supply_dollars` | All the daily fixed charges and fees of the period. |
| `note` | Free text, may be empty. It is the last field and may contain commas. |

How a bill becomes days:

- Each day is priced at the bill's average rates (`import_dollars / import_kwh`
  and `export_dollars / export_kwh`) and an equal share of `supply_dollars`.
- A day we have data for uses its own import, export and load kWh. A day with
  no data uses the bill's average daily import and export, and the average daily
  load of the same calendar month from the days we do have (the bill's average
  daily import if that month has none).
- The no-solar baseline is not taken from the bill. A battery that buys when
  power is cheap makes a bill's average import price much lower than a home
  without one would pay, which would make the savings look far too small.
  So a day with data keeps our baseline, priced on the `[tariff]` or
  `[[previous_tariff]]` plan for that day. A day without data gets that plan's
  supply charge plus its estimated load at the price per kWh of load the
  bill's days with data paid on the plan (or the plan's average import price
  if the bill has no day with data).
- The averages never add up to the bill exactly, so the leftover is shared
  equally over all the days of the bill (even the ones we have no data for) and
  added to the import cost, or taken off as extra export credit if negative.
  The days then add up to exactly `bill_total + excluded`.
- A bill wins over GloBird's daily figures for the same day.

## Admin API

The data in [Where the settings live](#where-the-settings-live) is edited
through these routes. They are under `/api/`, so they need a login; the
write routes need the login cookie (the `api_token` Bearer only works for
`GET` and `HEAD`), and a write whose `Origin` header names a different host
than the `Host` header is refused with 403 (requests without an `Origin`, such
as curl, are fine). Bodies and answers are JSON, errors are `{"error":"..."}`.
Each write runs in one transaction: the whole data set is checked after the
change, and if it does not work (say two plans start on the same day) nothing
is saved and the answer is a 400 with the reason. A change shows in the rest of
the API at once, and is added to `admin_change` with the row before and after.

| Endpoint | What it does |
| --- | --- |
| `GET /api/admin` | Everything: `{"tariffs":[Plan..],"petrol":{"fallback_cents_per_litre":..,"prices":[{from,cents_per_litre}..]},"purchases":[{id,name,date,cost_dollars,battery_kwh}..],"bills":[{from,to,bill_total,excluded,import_kwh,import_dollars,export_kwh,export_dollars,supply_dollars,note}..],"settings":{key:value..},"setting_defaults":{key:value..},"changes":[{id,at,what,action,key}..]}`. `settings` holds the keys that are set, `setting_defaults` what the others are. `changes` is the last 50, newest first (`at` is unix seconds). With `?download=1` the answer is sent as `solar-dash-admin-YYYY-MM-DD.json` to save (the Export) |
| `POST /api/admin/tariffs` | Adds a plan and answers with it (201). Body: a Plan without `id` |
| `POST /api/admin/tariffs/preview` | Prices the days so far on the current plans and on the plans with this change, without saving anything. Body: a Plan; with an `id` it replaces that plan, without one it is added. Optional `?from=YYYY-MM-DD&to=YYYY-MM-DD` (at most 92 days; default: the last 30 complete days). Answers `{"days":[{"date","current_cents","new_cents"}..],"total":{"current_cents","new_cents"}}`; days with no data are left out, and `current_cents` is null when no plan is saved yet |
| `PUT /api/admin/tariffs/{id}` | Replaces a plan and answers with it. 404 if there is none |
| `DELETE /api/admin/tariffs/{id}` | Answers `{"deleted": Plan}`. Deleting the last plan turns the cost features off |
| `POST /api/admin/purchases` | Adds `{"name","date":"YYYY-MM-DD","cost_dollars","battery_kwh"?}` and answers with it, with its `id` (201) |
| `PUT /api/admin/purchases/{id}` | Replaces a purchase and answers with it. 404 if there is none |
| `DELETE /api/admin/purchases/{id}` | Answers `{"deleted": Purchase}` |
| `PUT /api/admin/bills` | Body: a whole bills CSV (see [Past bills](#past-bills-optional)) with `Content-Type: text/csv`. Replaces every bill and answers `{"rows":n}`. 415 for another content type, 400 naming the line for a bad file |
| `DELETE /api/admin/bills/YYYY-MM-DD` | Removes the bill starting that day and answers `{"deleted": Bill}`. 404 if there is none |
| `PUT /api/admin/settings` | A partial object of setting keys (`petrol_cents_per_litre`, `petrol_litres_per_100km`, `away_cents_per_kwh`, `compare_cars`, `petrol_grade`, `price_rise_percent`). `null` puts a key back to its default. An unknown key or a bad value is a 400 and nothing changes. Answers with the settings that are set |
| `GET`, `PUT`, `DELETE /api/petrol-prices` | The petrol prices; see [Petrol prices](#petrol-prices) |

A Plan is `{"id":1,"valid_from":"2026-05-26"|null,"tariff":{..}}`. It applies
from `valid_from` (a local day, inclusive) until the day before the next plan
starts. Only the earliest plan may have `valid_from` `null` (no known start),
and no two plans may start on the same day. `tariff` has the fields of the old
`[tariff]` section as JSON: `name`, `supply_cents_per_day`,
`default_import_cents`, `default_export_cents`, `import` and `export` (lists of
`{"from":"HH:MM","to":"HH:MM","cents":..}`), `zero_import_credit`
(`{"from","to","max_kwh","credit_cents"}` or `null`) and `export_tier`
(`{"first_kwh_per_day","cents"}` or `null`), with the same checks.

```sh
curl -c cookies.txt -d 'password=YOUR_PASSWORD' http://127.0.0.1:8080/login
curl -b cookies.txt -o backup.json 'http://127.0.0.1:8080/api/admin?download=1'
curl -b cookies.txt -X PUT -H 'Content-Type: application/json' \
  -d '{"petrol_grade":"Premium 98","price_rise_percent":null}' \
  http://127.0.0.1:8080/api/admin/settings
curl -b cookies.txt -X PUT -H 'Content-Type: text/csv' --data-binary @bills.csv \
  http://127.0.0.1:8080/api/admin/bills
```

## Where the settings live

Secrets, hosts, ports, `bind`, the database path, the time zone, the plant id
and the sync times stay in `config.toml`. The data below is kept in the
database instead, so it can be edited while `serve` runs, and every change is
recorded in the `admin_change` table with the row before and after:

| What | Table | Used to be in `config.toml` |
| --- | --- | --- |
| Electricity plans over time | `tariff_plan` | `[tariff]`, `[[previous_tariff]]` |
| Petrol prices over time | `petrol_price` | `petrol_prices_file`, `[[teslamate.petrol_price]]` |
| What the system cost | `purchase` | `[[system.purchase]]` |
| Past bills | `bill` | `[bills]` |
| Preferences | `setting` | `petrol_cents_per_litre`, `petrol_litres_per_100km`, `away_cents_per_kwh`, `compare_cars`, `petrol_grade`, `price_rise_percent` |

The first time `serve` starts after upgrading it copies whatever the config
has into these tables, once for each row of the table above, and logs what it
imported. It records that in `admin_change` (action `import`), even when the
config had nothing, so emptying a table later never brings the config's copy
back. From then on the config's copy is ignored. If `config.toml` still has
any of it, `serve` logs a warning on every start naming what to delete. Old
configs keep loading, and a mistake in one of those sections still stops
`serve`.

The tariff cost cache key is unchanged by the move, so costs already stored in
the database stay valid.

## Backfill history

`serve` downloads the 5-minute history by itself. Set the day your inverter
was connected in the `[isolarcloud]` section of `config.toml`:

```toml
history_from = "2024-01-01"
```

After each live sync (every 5 minutes) `serve` fetches up to 18 more days of
history, oldest first, until it reaches the day the live sync began. The
dashboard stays live the whole time, and three years fill in within about
5 hours. Progress is saved, so restarts carry on where it stopped, and once
the history is complete it makes no more calls for it. It also resumes a
backfill you started earlier with the command below, even without
`history_from`.

You can also download history without the dashboard, for example on a
laptop:

```sh
cargo run -- backfill --from 2024-01-01
```

This needs `plant_id` in `config.toml` (run `login-test` to see your plants).
Which points are fetched and the plant's time zone can be changed with
`points` and `timezone` (see `config.example.toml`).

It is safe to stop (Ctrl+C) and run again: progress is saved after every
window, and the next run resumes where the last one stopped. The API returns
at most 3 hours per call, so a day is 8 calls and three years is about 8,800
calls. The command keeps itself under the API limits (it pauses at 1,800 calls
in an hour, shared with `serve`), so the full history takes about 5 hours.

There is no need to run it alongside `serve`, which already fills in the
history: both work from the same saved progress, so running them together
just downloads the same days twice. Stop `serve` first if you want the
command instead.

To check progress:

```sh
sqlite3 data/solar.db "select count(*), datetime(min(ts),'unixepoch'), datetime(max(ts),'unixepoch') from reading"
```

## Exploring the API

The `api` subcommand logs in, calls any iSolarCloud endpoint and prints the
`result_data` as pretty JSON. The params are optional and must be a JSON
object (they default to `{}`):

```sh
cargo run -- api /openapi/getPowerStationList '{"curPage":1,"size":10}'
cargo run -- api /openapi/getDeviceList '{"ps_id":"1234567","curPage":1,"size":50}'
```

Check the developer portal docs for endpoint names and their params. Every
call counts toward the API limits of 2,000 calls per hour and 100,000 per
month, so keep exploring to a few calls.

## Database

Data is stored in a SQLite file, `data/solar.db` by default (change it with
`path` in the `[database]` section of `config.toml`). It is created, and its
tables are set up, the first time you run `serve`. To look inside:

```sh
sudo apt install sqlite3
sqlite3 data/solar.db
```

Try `.tables` and `.schema reading` at the prompt.

## Checks

Run these before opening a PR (CI runs the same):

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

## VS Code

1. Open the `solar-dash` folder in VS Code.
2. Install the recommended extensions when prompted (rust-analyzer, CodeLLDB,
   Even Better TOML), or run "Extensions: Show Recommended Extensions".
3. Run tests: open a file with tests and click the "Run Test" CodeLens above a
   `#[test]` function or the `mod tests` block (provided by rust-analyzer).
4. Debug: press F5 and pick "Debug serve" (defined in `.vscode/launch.json`,
   uses CodeLLDB). Set breakpoints by clicking in the gutter.
