# solar-dash

A self-hosted dashboard for Sungrow iSolarCloud solar, battery and grid data. It is a single Rust binary that pulls data from the iSolarCloud API into SQLite and serves a web dashboard from it. It is also a project for learning Rust, so the code aims to stay simple and idiomatic.

## Status

- [x] Project scaffold (CLI, config, docs, CI)
- [x] iSolarCloud API client: login + plant list (`login-test`)
- [x] API explorer (`api` subcommand)
- [x] SQLite storage (schema, migrations, typed queries)
- [x] Backfill of historical data (`backfill --from`)
- [ ] Live sync
- [ ] Web dashboard
- [ ] Deployment to a Proxmox LXC with systemd, behind a Cloudflare Tunnel

## Architecture

One binary does everything. A background sync task fetches data from iSolarCloud and writes it to SQLite. An axum web server reads only from SQLite and serves an embedded static dashboard; the web side never calls iSolarCloud directly. This keeps us well inside the API limits of 2,000 calls per hour and 100,000 per month.

## Running it

See [docs/how-to-run.md](docs/how-to-run.md).

## Layout

- `src/main.rs` - CLI and program entry point
- `src/config.rs` - config file loading
- `src/db.rs` - SQLite storage: opening, migrations and typed queries
- `migrations/` - SQL schema, applied automatically on startup
- `src/ingest.rs` - parsing of history data and the backfill loop
- `src/isolar/` - iSolarCloud client (`client.rs`) and request encryption (`crypto.rs`)
- `config.example.toml` - template for `config.toml`
- `docs/` - documentation
- `.vscode/` - recommended extensions and debug config
- `.github/workflows/ci.yml` - CI (fmt, clippy, tests)
