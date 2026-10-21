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
off; without a backfill it starts from midnight today. The web dashboard will
be added to `serve` later.

Use a different config file with `--config path/to/file.toml`.
For more logging:

```sh
RUST_LOG=debug cargo run -- serve
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
