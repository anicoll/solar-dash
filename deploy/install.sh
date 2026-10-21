#!/usr/bin/env bash
# Installs solar-dash in a Debian 13 LXC. Run as root, from a checkout or an
# unpacked release tarball:
#
#   sudo ./deploy/install.sh                 # uses target/release/solar-dash
#   sudo ./deploy/install.sh /path/to/binary # uses a prebuilt binary
#
# From an unpacked release tarball it uses the binary that came with it.
#
# Safe to run again (for example to update the binary). It never overwrites an
# existing config and does not start solar-dash; see docs/deploy.md.
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "Run this script as root." >&2
    exit 1
fi

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [ -n "${1:-}" ]; then
    binary="$1"
elif [ -f "$repo_dir/solar-dash" ]; then
    # An unpacked release tarball: the binary sits next to deploy/.
    binary="$repo_dir/solar-dash"
else
    binary="$repo_dir/target/release/solar-dash"
fi

if [ ! -x "$binary" ]; then
    echo "Binary not found: $binary" >&2
    echo "Build it first with 'cargo build --release', or pass a path as the first argument." >&2
    exit 1
fi

if ! command -v sqlite3 >/dev/null 2>&1 || [ ! -e /etc/ssl/certs/ca-certificates.crt ]; then
    echo "Installing packages..."
    apt-get update -qq
    apt-get install -y -qq sqlite3 ca-certificates >/dev/null
fi

echo "Creating user and directories..."
if ! id -u solar-dash >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/solar-dash --shell /usr/sbin/nologin solar-dash
fi
install -d -m 755 /opt/solar-dash
install -d -m 750 -o root -g solar-dash /etc/solar-dash
install -d -m 750 -o solar-dash -g solar-dash /var/lib/solar-dash
install -d -m 750 -o solar-dash -g solar-dash /var/backups/solar-dash

echo "Installing binary and systemd units..."
install -m 755 "$binary" /opt/solar-dash/solar-dash
install -m 755 "$repo_dir/deploy/update.sh" /opt/solar-dash/update.sh
if [ -f "$repo_dir/VERSION" ]; then
    install -m 644 "$repo_dir/VERSION" /opt/solar-dash/VERSION
else
    # Built from a checkout: record the commit, if git knows it.
    git -C "$repo_dir" rev-parse HEAD >/opt/solar-dash/VERSION 2>/dev/null || echo unknown >/opt/solar-dash/VERSION
fi
install -m 644 "$repo_dir/deploy/solar-dash.service" /etc/systemd/system/
install -m 644 "$repo_dir/deploy/solar-dash-backup.service" /etc/systemd/system/
install -m 644 "$repo_dir/deploy/solar-dash-backup.timer" /etc/systemd/system/

if [ -e /etc/solar-dash/config.toml ]; then
    echo "Config already exists, leaving it alone: /etc/solar-dash/config.toml"
else
    echo "Creating /etc/solar-dash/config.toml from the example..."
    install -m 640 -o root -g solar-dash "$repo_dir/config.example.toml" /etc/solar-dash/config.toml
    # Point the database at the service's data directory.
    sed -i 's|^path = "data/solar.db"|path = "/var/lib/solar-dash/solar.db"|' /etc/solar-dash/config.toml
fi

systemctl daemon-reload
systemctl enable --now solar-dash-backup.timer

cat <<'MSG'

Installed. solar-dash is NOT started yet. Next steps:

  1. Edit the config (iSolarCloud login, plant_id, password_hash, session_secret):
       nano /etc/solar-dash/config.toml
  2. Optional: copy your existing solar.db into /var/lib/solar-dash/ first
     (see docs/deploy.md), then: chown solar-dash:solar-dash /var/lib/solar-dash/solar.db
  3. Start it and watch the logs:
       systemctl enable --now solar-dash
       journalctl -u solar-dash -f
MSG
