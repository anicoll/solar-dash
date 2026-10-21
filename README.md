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
- [x] Dashboard UI (Now, Day, Month, Year views; ECharts from cdnjs)
- [x] Local Modbus reader (`modbus-test`) for live power from the inverter
- [x] `serve` polls Modbus when configured and stores live samples (`/api/live`, `/api/live/series`)
- [x] Deployment to a Proxmox LXC with systemd, behind a Cloudflare Tunnel

## Architecture

One binary does everything. A background sync task fetches data from iSolarCloud and writes it to SQLite. If a `[modbus]` section is configured, a second task also reads live power from the inverter over the local network. An axum web server reads only from SQLite and serves an embedded static dashboard (plain HTML/CSS/JS, no build step); the web side never calls iSolarCloud directly. This keeps us well inside the API limits of 2,000 calls per hour and 100,000 per month.

## Running it

See [docs/how-to-run.md](docs/how-to-run.md). To run it on a server (Proxmox LXC, systemd, Cloudflare Tunnel), see [docs/deploy.md](docs/deploy.md).

## Layout

- `src/main.rs` - CLI and program entry point
- `src/config.rs` - config file loading
- `src/cost.rs` - electricity tariff lookup and the pure daily cost calculation
- `src/db.rs` - SQLite storage: opening, migrations and typed queries
- `migrations/` - SQL schema, applied automatically on startup
- `src/modbus.rs` - Modbus TCP reader for live inverter data, and its poll loop
- `src/ingest.rs` - parsing of history data, backfill and the live sync loop
- `src/web/` - web server: login (`auth.rs`), JSON API (`api.rs`), the cost API (`costs.rs`), the payback API (`payback.rs`) and embedded static files
- `static/` - the dashboard (`index.html`, `app.js`, `style.css`), compiled into the binary
- `src/isolar/` - iSolarCloud client (`client.rs`) and request encryption (`crypto.rs`)
- `config.example.toml` - template for `config.toml`
- `deploy/` - systemd units, backup timer, cloudflared example and `install.sh`
- `docs/` - documentation
- `.vscode/` - recommended extensions and debug config
- `.github/workflows/ci.yml` - CI (fmt, clippy, tests)
