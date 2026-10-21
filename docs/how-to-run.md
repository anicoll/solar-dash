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

The page has four tabs, and the address keeps the view (for example
`#day=2026-10-20`, `#month=2026-10`, `#year=2026`), so refresh and bookmarks
work. All times are shown in Australia/Adelaide.

- **Now**: solar and load power, battery, and today's energy totals. With
  [Modbus](#local-modbus-optional) running it shows the inverter's live values
  (solar, load, grid importing or exporting, battery charging or discharging,
  and battery %), refreshed every 15 seconds. Without a sample under 5 minutes
  old it falls back to the iSolarCloud readings, refreshed every minute. A line
  at the top says which source it is and how old the reading is.
- **Day**: a power chart for one day (pick a date or step with the arrows).
  Solar, load, grid import, export (below the axis) and battery charge and
  discharge are in kW, and battery % is on the right axis. 11:00-14:00 is
  shaded as the "Free 11-2" window and 18:00-21:00 as "Peak". Energy totals
  for the day are underneath. The chart uses the inverter's 1-minute data when
  there is at least 30 minutes of it for that day, and iSolarCloud's 5-minute
  data otherwise. If Modbus started part way through the day, iSolarCloud fills
  the time before it. A note under the chart names the source. The totals
  always come from iSolarCloud.
- **Month**: daily bars of PV generation and grid import (up) and export
  (down), with a line for load, plus the month's totals.
- **Year**: the same, summed by month.

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
| `GET /api/daily?from=YYYY-MM-DD&to=YYYY-MM-DD` | kWh totals per local day (default: the last 30 days). At most 400 days. Each day has a `source` (see [Modbus in `serve`](#modbus-in-serve)) |
| `GET /api/costs?from=YYYY-MM-DD&to=YYYY-MM-DD` | Dollars per local day from the `[tariff]` section: `actual`, `baseline` (no solar or battery), `savings`, zero-import `credit`, and import, export and load kWh, plus `totals`. Default: the last 30 days. At most 1,500 days. Days with no data are left out. Finished days are stored in the database, so repeat requests are fast; editing the `[tariff]` rates makes them be worked out again (editing a `[[previous_tariff]]` only redoes the days on that plan). Days before the current plan are priced on the `[[previous_tariff]]` entry in effect on that day (its `until` is the last day it applies), if any are configured. Without a `[tariff]` section it answers `{"configured":false}` |
| `GET /api/payback` | How much of the system's cost the savings have covered, from the `[[system.purchase]]` entries and the `[tariff]`: `total_cost`, `saved`, `net`, `recovered_pct`, `rate_per_day` (mean savings over the last 365 days with data), `price_rise_percent` (the `[system]` setting, default 3, that grows that rate each year in the projection), `projected_payback` (`"YYYY-MM"`, `null` once paid back, or `"never"`), the `purchases`, a monthly `series` of cumulative `saved` and `net`, and `by_year` (`partial` when a year has fewer days of data than days). Counts from the first purchase date to today. Without purchases or a `[tariff]` section it answers `{"configured":false}` |

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
   mosquitto_sub -h 192.168.10.84 -t 'teslamate/cars/1/#' -v
   ```

   You should see lines like `teslamate/cars/1/battery_level 43`. TeslaMate
   retains these, so they appear at once, even while the car is asleep.

2. Add the section to `config.toml` (see `config.example.toml`). Only `host`
   is required; the rest are the defaults:

   ```toml
   [teslamate]
   host = "192.168.10.84"   # the MQTT broker
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

The state is only kept in memory (the broker keeps the latest values, so they
come back after a restart). Unlike the Modbus sample, `car` is not hidden
when it gets old: TeslaMate's values stay true while the car sleeps. Use
`updated_ts` to see how fresh they are.

If the broker is down, `serve` keeps running. The first failure is logged as
a warning, then it retries after 1 second, doubling up to every 60 seconds,
and `TeslaMate MQTT back` is logged when it reconnects.

## Backfill history

`serve` downloads the 5-minute history by itself. Set the day your inverter
was connected in the `[isolarcloud]` section of `config.toml`:

```toml
history_from = "2023-02-21"
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
cargo run -- backfill --from 2023-02-21
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
