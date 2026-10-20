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

Start the server:

```sh
cargo run -- serve
```

Use a different config file with `--config path/to/file.toml`.
For more logging:

```sh
RUST_LOG=debug cargo run -- serve
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
