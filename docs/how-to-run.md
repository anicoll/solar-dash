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

Keep the database up to date (run [backfill](#backfill-history) once first
if you want older history):

```sh
cargo run -- serve
```

`serve` logs in, then syncs straight away and every 5 minutes after that
(about a minute after each 5-minute mark, when the data has landed). Stop it
with Ctrl+C. It makes about 12 calls an hour, about 8,900 a month, well inside
the API limits. If it was stopped for a while it catches up from where it left
off; without a backfill it starts from midnight today.  `serve` also runs the
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

Also add a `session_secret` so you stay logged in across restarts (see
`config.example.toml`):

```sh
openssl rand -base64 64 | tr -d '\n'; echo
```

It prints 88 characters ending in `=`; copy just those. (The `echo` stops
zsh adding a `%` after the output, which isn't part of the secret.)

Without it, `serve` makes a new one on each start and logs a warning, which
logs everyone out whenever it restarts.

Then run `cargo run -- serve` and open <http://127.0.0.1:8080>. After 5 wrong passwords in 15 minutes, logins are refused for 15
minutes. A login lasts 30 days.

### Dashboard views

The page has four tabs, and the address keeps the view (for example
`#day=2026-10-20`, `#month=2026-10`, `#year=2026`), so refresh and bookmarks
work. All times are shown in Australia/Adelaide.

- **Now**: solar and load power, battery charge, and today's energy totals.
  It refreshes every minute and shows how old the newest reading is.
- **Day**: a power chart for one day (pick a date or step with the arrows).
  Solar, load, grid import, export (below the axis) and battery charge and
  discharge are in kW, and battery % is on the right axis. 11:00-14:00 is
  shaded as the "Free 11-2" window and 18:00-21:00 as "Peak". Energy totals
  for the day are underneath.
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
| `GET /healthz` | `{"ok":true,"last_reading_ts":...}` (public) |
| `GET /api/latest` | The newest value of every point, with its name and unit |
| `GET /api/series?points=83033,83106&from=<unix>&to=<unix>` | Raw `[ts, value]` pairs per point. At most 20 points and 31 days |
| `GET /api/day?date=YYYY-MM-DD` | One local day (default today): PV, load and battery % plus grid and battery power worked out from the energy counters. Every series has a `key` such as `pv_w` or `grid_import_w` |
| `GET /api/daily?from=YYYY-MM-DD&to=YYYY-MM-DD` | kWh totals per local day (default: the last 30 days). At most 400 days |

For example, once logged in with a browser, try
<http://127.0.0.1:8080/api/latest>. With curl, log in and keep the cookie:

```sh
curl -c cookies.txt -d 'password=YOUR_PASSWORD' http://127.0.0.1:8080/login
curl -b cookies.txt http://127.0.0.1:8080/api/latest
```

## Backfill history

Download the 5-minute history into the database, starting from the day your
inverter was connected:

```sh
cargo run -- backfill --from 2023-02-21
```

This needs `plant_id` in `config.toml` (run `login-test` to see your plants).
Which points are fetched and the plant's time zone can be changed with
`points` and `timezone` (see `config.example.toml`).

It is safe to stop (Ctrl+C) and run again: progress is saved after every
window, and the next run resumes where the last one stopped. The API returns
at most 3 hours per call, so a day is 8 calls and three years is about 8,800
calls, roughly 2.5 hours with the 1 s pause between calls. The command keeps
itself under the API limits and pauses if it gets close to the hourly limit.

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
