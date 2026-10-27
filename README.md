# solar-dash

A self-hosted dashboard for Sungrow iSolarCloud solar, battery and grid data. It is a single Rust binary that pulls data from the iSolarCloud API into SQLite and serves a web dashboard from it. It is also a project for learning Rust, so the code aims to stay simple and idiomatic.

## Status

- [x] Project scaffold (CLI, config, docs, CI)
- [x] iSolarCloud API client: login + plant list (`login-test`)
- [x] API explorer (`api` subcommand)
- [x] SQLite storage (schema, migrations, typed queries)
- [x] Backfill of historical data (automatic in `serve` with `history_from`, or `backfill --from`)
- [x] Live sync (`serve` keeps the database current every 5 minutes)
- [x] Web server with password login and JSON API
- [x] Dashboard UI (a home page with Now and History (Day, Month, Year), plus Money, Payback and Cars views; ECharts from cdnjs)
- [x] Local Modbus reader (`modbus-test`) for live power from the inverter
- [x] `serve` polls Modbus when configured and stores live samples (`/api/live`, `/api/live/series`)
- [x] `serve` follows the Tesla's charge state from TeslaMate over MQTT when configured (`/api/live`)
- [x] `/api/ev/cars` gives each TeslaMate car's details (trim, paint, wheels), home charging per plan, and a petrol comparison car; set the `compare_cars` and `petrol_grade` settings, for example `{"3": "Hyundai i30 Sedan N", "Y": "Toyota RAV4 GX 2WD"}` and `Unleaded 91` (kept in the database; see "Where the settings live" in `docs/how-to-run.md`)
- [x] GloBird portal probe (`globird-probe`) to learn the response shapes (unofficial API)
- [x] `serve` keeps GloBird's daily cost and usage in SQLite when configured (no API or UI yet)
- [x] Deployment to a Proxmox LXC with systemd, behind a Cloudflare Tunnel

## Architecture

One binary does everything. A background sync task fetches data from iSolarCloud and writes it to SQLite. If a `[modbus]` section is configured, a second task also reads live power from the inverter over the local network. An axum web server reads only from SQLite and serves an embedded static dashboard (plain HTML/CSS/JS, no build step); the web side never calls iSolarCloud directly. This keeps us well inside the API limits of 2,000 calls per hour and 100,000 per month.

### Inverter settings

With a `[modbus]` section, the poller also reads the inverter's control settings (EMS mode, forced charge/discharge command, battery SOC limits, export limit, backup reserve, charge/discharge power limits and running state) about once a minute. They show in the "Inverter" panel on the home page and at `GET /api/inverter`, which returns `{"settings": {...}}` with raw values and labels, or `{"settings": null}` when there is no recent reading. This is read-only: the program never writes to the inverter. Some WiNet-S firmware refuses a few of these registers; then only those values show as n/a. `modbus-test` prints the settings too. Whenever a setting changes (for example when the retailer's service switches the battery's mode), the new settings are saved with the time, and the "Recent changes" list in the panel shows the latest ones; `GET /api/inverter/history?from=<unix>&to=<unix>` returns them as `{"changes": [...]}`, newest first, for the last 7 days by default (at most 400 days and 500 rows).

## Running it

See [docs/how-to-run.md](docs/how-to-run.md). To run it on a server (Proxmox LXC, systemd, Cloudflare Tunnel), see [docs/deploy.md](docs/deploy.md).

## Layout

- `src/main.rs` - CLI and program entry point
- `src/config.rs` - config file loading
- `src/cost.rs` - electricity tariff lookup and the pure daily cost calculation
- `src/admin_data.rs`, `src/admin_import.rs` - tariffs, petrol prices, purchases, bills and settings kept in tables, the swappable snapshot of them, and the one-time import from an older `config.toml`
- `src/db.rs` - SQLite storage: opening, migrations and typed queries
- `migrations/` - SQL schema, applied automatically on startup
- `src/modbus.rs` - Modbus TCP reader for live inverter data, and its poll loop
- `src/teslamate.rs` - TeslaMate MQTT reader for the car's charge state
- `src/ingest.rs` - parsing of history data, backfill and the live sync loop
- `src/web/` - web server: login (`auth.rs`), JSON API (`api.rs`), the admin API (`admin.rs`), the cost API (`costs.rs`), the payback API (`payback.rs`) and embedded static files
- `static/` - the dashboard (`index.html`, `app.js`, `style.css`), compiled into the binary
- `src/globird/` - GloBird portal client (`client.rs`), the `globird-probe` command (`probe.rs`), the daily sync (`sync.rs`) and the row types it saves (`data.rs`)
- `src/isolar/` - iSolarCloud client (`client.rs`) and request encryption (`crypto.rs`)
- `config.example.toml` - template for `config.toml`
- `deploy/` - systemd units, backup timer, cloudflared example and `install.sh`
- `docs/` - documentation
- `.vscode/` - recommended extensions and debug config
- `.github/workflows/ci.yml` - CI (fmt, clippy, tests)
