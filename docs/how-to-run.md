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
(appkey, access key, RSA public key) and your normal iSolarCloud login.
`config.toml` is gitignored.

## Run

Check your credentials (not implemented yet, currently it only loads the config):

```sh
cargo run -- login-test
```

Start the server:

```sh
cargo run -- serve
```

Use a different config file with `--config path/to/file.toml`.
For more logging:

```sh
RUST_LOG=debug cargo run -- serve
```

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
