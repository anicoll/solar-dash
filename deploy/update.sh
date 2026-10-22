#!/usr/bin/env bash
# Updates solar-dash in the LXC to the "latest" GitHub release. Run as root:
#
#   /opt/solar-dash/update.sh             # install the latest build if it is newer
#   /opt/solar-dash/update.sh --force     # install it even if it is the same
#   /opt/solar-dash/update.sh --check     # only say whether an update is waiting
#   /opt/solar-dash/update.sh --rollback  # go back to the previous version
#
# It keeps your config and database, keeps the old binary as solar-dash.prev
# (one version back), and restarts the service. After a good update it deletes
# older pre-update database backups, keeping only the newest. To run it every
# night: systemctl enable --now solar-dash-update.timer. See docs/deploy.md.
set -euo pipefail

repo="anicoll/solar-dash"
asset="solar-dash-linux-x86_64.tar.gz"
url="https://github.com/$repo/releases/download/latest/$asset"
opt=/opt/solar-dash

if [ "$(id -u)" -ne 0 ]; then
    echo "Run this script as root." >&2
    exit 1
fi

current="$(cat "$opt/VERSION" 2>/dev/null || echo unknown)"

# Waits for the web server to answer after a restart.
check_health() {
    for _ in $(seq 1 15); do
        if curl -fsS http://127.0.0.1:8080/healthz >/dev/null 2>&1; then
            echo "solar-dash is up."
            return 0
        fi
        sleep 2
    done
    echo "solar-dash did not answer on /healthz. Check: journalctl -u solar-dash -n 50" >&2
    echo "To go back to the previous version: $opt/update.sh --rollback" >&2
    return 1
}

case "${1:-}" in
    --rollback)
        if [ ! -x "$opt/solar-dash.prev" ]; then
            echo "No previous version to go back to." >&2
            exit 1
        fi
        systemctl stop solar-dash
        mv "$opt/solar-dash.prev" "$opt/solar-dash"
        if [ -e "$opt/VERSION.prev" ]; then mv "$opt/VERSION.prev" "$opt/VERSION"; fi
        systemctl start solar-dash
        echo "Rolled back to $(cat "$opt/VERSION" 2>/dev/null || echo unknown)."
        check_health
        exit
        ;;
    --check | --force | "") ;;
    *)
        echo "Unknown option: $1 (use --check, --force or --rollback)" >&2
        exit 1
        ;;
esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "Downloading the latest build..."
curl -fsSL -o "$work/$asset" "$url"
curl -fsSL -o "$work/$asset.sha256" "$url.sha256"
(cd "$work" && sha256sum --check --quiet "$asset.sha256")
tar xzf "$work/$asset" -C "$work"
new="$work/solar-dash"
latest="$(cat "$new/VERSION")"

if [ "$latest" = "$current" ] && [ "${1:-}" != "--force" ]; then
    echo "Already up to date ($current)."
    exit 0
fi
if [ "${1:-}" = "--check" ]; then
    echo "Update available: $current -> $latest"
    exit 0
fi

# Database migrations only go forward, so take an online backup first. If a
# migration ran, restore this file after --rollback. The database path comes
# from the config; override it with SOLAR_DASH_DB.
db="${SOLAR_DASH_DB:-$(sed -n 's/^path *= *"\(.*\)"/\1/p' /etc/solar-dash/config.toml 2>/dev/null | head -n 1)}"
db="${db:-/var/lib/solar-dash/solar.db}"
if [ -f "$db" ]; then
    install -d -m 750 -o solar-dash -g solar-dash /var/backups/solar-dash
    backup="/var/backups/solar-dash/pre-update-$(date +%Y%m%d-%H%M%S).db"
    echo "Backing up the database to $backup"
    runuser -u solar-dash -- sqlite3 "$db" ".backup '$backup'"
else
    echo "No database at $db yet, skipping the pre-update backup."
fi

echo "Updating $current -> $latest"
if [ -x "$opt/solar-dash" ]; then
    cp -p "$opt/solar-dash" "$opt/solar-dash.prev"
    if [ -e "$opt/VERSION" ]; then cp -p "$opt/VERSION" "$opt/VERSION.prev"; fi
fi
# install.sh copies the binary, systemd units and this script, and never
# touches an existing config. Database changes run on startup.
# It writes a new file rather than changing the running one, so the old
# version keeps running until the restart.
"$new/deploy/install.sh" "$new/solar-dash" >/dev/null
systemctl restart solar-dash
check_health

# Only one rollback is possible, so only the newest pre-update backup is useful.
if [ -n "${backup:-}" ]; then
    find /var/backups/solar-dash -maxdepth 1 -name 'pre-update-*.db' ! -path "$backup" -delete
fi
