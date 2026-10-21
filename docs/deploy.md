# Deploy solar-dash on Proxmox

This guide runs solar-dash in a Debian 13 (trixie) LXC container on Proxmox,
managed by systemd, with a nightly database backup. It is published to the
internet through a Cloudflare Tunnel, so you do not open or forward any ports
on your router.

Throughout, **`solar.example.com` is a placeholder. Replace it with the real
subdomain you choose** (in the tunnel commands and in the cloudflared config).

The files used here are in [`deploy/`](../deploy):

| File | What it is |
| --- | --- |
| `solar-dash.service` | systemd unit that runs `solar-dash serve` as its own user |
| `solar-dash-backup.service` / `.timer` | nightly SQLite backup at 03:30, keeps 14 days |
| `cloudflared-config.yml` | example Cloudflare Tunnel config |
| `install.sh` | sets up the user, folders, binary and units |
| `update.sh` | updates to the latest build from GitHub, with rollback |

Where things end up inside the LXC:

| Path | Contents |
| --- | --- |
| `/opt/solar-dash/solar-dash` | the program (and `update.sh`, `VERSION`) |
| `/etc/solar-dash/config.toml` | your config and secrets (only root and the service can read it) |
| `/var/lib/solar-dash/solar.db` | the database |
| `/var/backups/solar-dash/` | nightly backups |

## 1. Create the LXC

You need a Debian 13 template on Proxmox. Download it once on the Proxmox host:

```sh
pveam update
pveam available --section system | grep debian-13
pveam download local debian-13-standard_<version>_amd64.tar.zst
```

(Use the exact file name that `pveam available` shows; the version number may differ.)

Create the container. Change the id `120`, storage names, bridge and the
address to suit your network. This is unprivileged, with 1 core, 512 MB RAM
and 4 GB disk, which is plenty when you install the prebuilt release (use
2048 MB if you build inside the LXC instead, see below):

```sh
pct create 120 local:vztmpl/debian-13-standard_<version>_amd64.tar.zst \
  --hostname solar-dash \
  --unprivileged 1 \
  --cores 1 --memory 512 --swap 512 \
  --rootfs local-lvm:4 \
  --net0 name=eth0,bridge=vmbr0,ip=192.168.1.50/24,gw=192.168.1.1 \
  --nameserver 1.1.1.1 \
  --onboot 1 \
  --password
pct start 120
pct enter 120
```

For DHCP use `ip=dhcp` instead and add a DHCP reservation for the container on
your router. Either way, give it an address that does not change.

Via the web UI instead: **Create CT**, tick **Unprivileged container**, pick
the Debian 13 template, set Cores 1, Memory 512 MB, Disk 4 GB, and on the
Network tab set a static IPv4/CIDR and gateway (or DHCP). Tick **Start at boot**.

Inside the container, as root:

```sh
apt update && apt full-upgrade -y
apt install -y git curl ca-certificates sqlite3
```

The LXC needs outbound internet. If you use [Modbus](how-to-run.md#local-modbus-optional),
it must also be on the same LAN as the WiNet-S dongle and able to reach its IP
address on TCP port 502 (check with `nc -vz <winet-ip> 502`; the systemd unit
already allows `AF_INET`, so no change is needed there). The dashboard listens on
`127.0.0.1:8080`, and the tunnel connects to it from inside the container, so
nothing needs to reach the container from outside.

## 2. Get the program

### Option A (recommended): download the latest build

Every merge to `main` is built on GitHub for Debian 13 and published as the
[Latest build](https://github.com/anicoll/solar-dash/releases/tag/latest)
release. In the LXC, as root:

```sh
cd /root
curl -fsSLO https://github.com/anicoll/solar-dash/releases/download/latest/solar-dash-linux-x86_64.tar.gz
curl -fsSLO https://github.com/anicoll/solar-dash/releases/download/latest/solar-dash-linux-x86_64.tar.gz.sha256
sha256sum --check solar-dash-linux-x86_64.tar.gz.sha256
tar xzf solar-dash-linux-x86_64.tar.gz
```

That gives you `/root/solar-dash/` with the program, `deploy/` and
`config.example.toml`. No Rust or build tools are needed in the LXC.

### Option B: build inside the LXC

A release build can use more than 1 GB of RAM, so give the LXC 2048 MB while
building. You can lower it to 512 MB afterwards.

```sh
apt install -y build-essential pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
git clone https://github.com/anicoll/solar-dash.git /root/solar-dash
cd /root/solar-dash
cargo build --release
```

The first build takes a few minutes. The result is `target/release/solar-dash`.
A binary built on your Ubuntu laptop may also work if your Ubuntu's glibc is no
newer than Debian 13's (2.41); if not it fails with `GLIBC_2.xx not found`.

## 3. Install and configure

In the LXC, as root (both options):

```sh
cd /root/solar-dash
./deploy/install.sh
```

It creates the `solar-dash` user and the folders, copies the program and the
systemd units, creates `/etc/solar-dash/config.toml` from
`config.example.toml` (only if you do not have one yet) and turns on the
backup timer. It is safe to run again. It does **not** start solar-dash, because
the config still needs your values.

Make the dashboard password hash (type the password twice) and a session secret:

```sh
/opt/solar-dash/solar-dash hash-password
openssl rand -base64 64 | tr -d '\n'; echo
```

Now edit the config:

```sh
nano /etc/solar-dash/config.toml
```

Fill in:

- `[isolarcloud]`: `gateway`, `appkey`, `access_key`, `rsa_public_key`,
  `encrypt`, `username` and `password` (see [how-to-run.md](how-to-run.md)).
- `plant_id`: your plant id (uncomment the line). If you do not know it, run
  `/opt/solar-dash/solar-dash --config /etc/solar-dash/config.toml login-test`.
- `[server]`: `bind = "127.0.0.1:8080"`, the `password_hash = "..."` line from
  `hash-password`, and `session_secret = "..."` (the 88 characters printed by
  the `openssl` command, ending in `=`; leave out any `%`). `serve` will not
  start without it. Leave `cookie_secure` at its default (`true`): visitors
  reach the site over https through the tunnel.
- `[database]`: `path = "/var/lib/solar-dash/solar.db"` (install.sh already
  sets this in a new config).

The file holds secrets, which is why it is readable only by root and the
`solar-dash` group (mode 640).

## 4. Copy your existing data (optional)

If you already backfilled `data/solar.db` on your laptop, copy it over so you do
not have to download the history again. On the LXC, stop the service if it is
running:

```sh
systemctl stop solar-dash
```

On the laptop, from the repo folder. Closing `serve` first makes sure the
database is not mid-write. If `data/solar.db-wal` exists, run
`sqlite3 data/solar.db "pragma wal_checkpoint(truncate)"` first:

```sh
scp data/solar.db root@192.168.1.50:/var/lib/solar-dash/solar.db
```

Back on the LXC:

```sh
chown solar-dash:solar-dash /var/lib/solar-dash/solar.db
chmod 640 /var/lib/solar-dash/solar.db
rm -f /var/lib/solar-dash/solar.db-wal /var/lib/solar-dash/solar.db-shm
```

Without this step solar-dash starts with an empty database. With
`history_from = "2023-02-21"` in the `[isolarcloud]` section of the config it
fills in the history by itself in the background, which takes about 5 hours
for three years.

## 5. Start it

```sh
systemctl enable --now solar-dash
systemctl status solar-dash
journalctl -u solar-dash -f
```

Healthy logs look like this (details differ). Stop following with Ctrl+C:

```
INFO solar_dash: config loaded config=/etc/solar-dash/config.toml
INFO solar_dash: database ready path=/var/lib/solar-dash/solar.db
INFO solar_dash: web server started address=127.0.0.1:8080
```

followed by a line about each sync every 5 minutes. Check the web server from
inside the LXC:

```sh
curl http://127.0.0.1:8080/healthz
```

It should print `{"ok":true}`. If it fails, see
[Troubleshooting](#troubleshooting).

## 6. Cloudflare Tunnel

You need a Cloudflare account with your domain on it. Install `cloudflared`
from Cloudflare's apt repo (in the LXC, as root):

```sh
mkdir -p --mode=0755 /usr/share/keyrings
curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg \
  -o /usr/share/keyrings/cloudflare-main.gpg
echo 'deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main' \
  > /etc/apt/sources.list.d/cloudflared.list
apt update && apt install -y cloudflared
```

(If Cloudflare's instructions at <https://pkg.cloudflare.com> differ, follow
theirs.) Then log in, create the tunnel and point your subdomain at it.
**Replace `solar.example.com` with your real subdomain:**

```sh
cloudflared tunnel login
cloudflared tunnel create solar-dash
cloudflared tunnel route dns solar-dash solar.example.com
```

`tunnel login` prints a link; open it in a browser and pick your domain.
`tunnel create` prints a tunnel id (a UUID) and writes
`~/.cloudflared/<TUNNEL_ID>.json`, the tunnel's credentials. Copy the config and
credentials into place and edit the config:

```sh
mkdir -p /etc/cloudflared
cp ~/.cloudflared/*.json /etc/cloudflared/
cp /root/solar-dash/deploy/cloudflared-config.yml /etc/cloudflared/config.yml
nano /etc/cloudflared/config.yml
```

Replace both `<TUNNEL_ID>` values with your tunnel id, and `solar.example.com`
with your subdomain. Then install and start the service:

```sh
cloudflared service install
systemctl enable --now cloudflared
systemctl status cloudflared
```

Open `https://solar.example.com` (your subdomain) and log in. Nothing is port
forwarded: the tunnel makes an outbound connection to Cloudflare.

Security notes:

- The dashboard password is the only login. After 5 wrong tries in 15 minutes
  logins are refused for 15 minutes. Use a long, unique password.
- For an extra layer you can add Cloudflare Access (Zero Trust dashboard, then
  Access, then Applications) in front of the subdomain with an email one-time-PIN
  policy. This is optional.

## 7. Updating

After a PR is merged, wait for the **Release** workflow on GitHub to finish
(a few minutes), then in the LXC, as root:

```sh
/opt/solar-dash/update.sh
```

It downloads the latest build, checks its checksum, installs it (keeping your
config and database), restarts solar-dash and checks `/healthz`. It also
updates the systemd units and itself. Database changes are applied
automatically on startup. Other options:

```sh
/opt/solar-dash/update.sh --check     # is there a newer build?
/opt/solar-dash/update.sh --rollback  # go back to the version before the last update
cat /opt/solar-dash/VERSION           # the commit that is installed
```

Before installing, the update takes an online backup of the database to
`/var/backups/solar-dash/pre-update-<timestamp>.db` (these are not deleted
automatically; remove old ones by hand). If your database is not at the `path`
in `/etc/solar-dash/config.toml`, set `SOLAR_DASH_DB=/path/to/solar.db`.

Rollback swaps the program back. A database change made by the newer version
stays, so if an update included one, restore the latest `pre-update-*.db`
backup too (section 8).

The update that removes the `raw_response` table (nothing used it) frees space
inside the database file but does not shrink the file. To shrink it once, stop
solar-dash and run `VACUUM` (it cannot run inside a migration):

```sh
sudo systemctl stop solar-dash && sudo -u solar-dash sqlite3 /var/lib/solar-dash/solar.db 'VACUUM;' && sudo systemctl start solar-dash
```

Old API call records (kept 40 days) are now pruned automatically.

If you built inside the LXC (option B), update with `git pull`,
`cargo build --release`, `./deploy/install.sh` and `systemctl restart solar-dash`
instead.

## 8. Backups and restore

The timer runs every night at 03:30 and writes
`/var/backups/solar-dash/solar-YYYY-MM-DD.db`. It uses SQLite's online
backup, which is safe while solar-dash is running, and deletes backups older
than 14 days. Useful commands:

```sh
systemctl list-timers solar-dash-backup.timer     # when it next runs
systemctl start solar-dash-backup.service         # run a backup now
journalctl -u solar-dash-backup.service           # see what happened
ls -l /var/backups/solar-dash/
```

To restore a backup (pick a file from the list):

```sh
systemctl stop solar-dash
cp /var/backups/solar-dash/solar-2026-10-20.db /var/lib/solar-dash/solar.db
rm -f /var/lib/solar-dash/solar.db-wal /var/lib/solar-dash/solar.db-shm
chown solar-dash:solar-dash /var/lib/solar-dash/solar.db
systemctl start solar-dash
```

The `-wal` and `-shm` files belong to the old database, so they must be removed
or SQLite may mix them with the restored file.

The backups live inside the LXC, so a lost container loses them too. Also back
up the whole container with Proxmox: **Datacenter, Backup, Add** (or the
container's **Backup** tab, **Backup now**), or on the host:

```sh
vzdump 120 --storage local --mode snapshot --compress zstd
```

Better still, send these to a different disk or machine.

## Troubleshooting

- **Service will not start or keeps restarting**: run
  `journalctl -u solar-dash -n 50 --no-pager`. A config error is named in the
  message (for example a missing `password_hash`).
- **"could not listen on ..."**: something else is using port 8080, or `bind`
  is wrong. Check with `ss -ltnp | grep 8080`.
- **"login to iSolarCloud failed ... retrying"** (a warning in the log): the
  dashboard and Modbus data keep working, and the login is retried after 1
  minute, then 2, 4 and so on up to every 30 minutes. If it never succeeds the
  iSolarCloud values are probably wrong. See the error table in [how-to-run.md](how-to-run.md#troubleshooting). Test with
  `/opt/solar-dash/solar-dash --config /etc/solar-dash/config.toml login-test`
  (run it as root so it can read the config).
- **"unable to open database file" / permission denied**: the database folder
  or file is not owned by the service user. Run
  `chown -R solar-dash:solar-dash /var/lib/solar-dash`.
- **`GLIBC_2.xx not found`**: the binary was built on a newer system. Use the
  release build (option A) or build inside the LXC (option B).
- **`serve` exits asking for `session_secret`**: add it to `[server]`.
- **Login works but you bounce back to the login page** (plain http only):
  `cookie_secure` is true; set it to false for http testing.
- **Site not reachable**: check `systemctl status cloudflared` and
  `journalctl -u cloudflared -n 50`, that the tunnel id and hostname in
  `/etc/cloudflared/config.yml` are right, that the DNS route exists
  (`cloudflared tunnel route dns ...`), and that
  `curl http://127.0.0.1:8080/healthz` works inside the LXC. A 502 from
  Cloudflare means solar-dash is not running.
- **Charts empty**: the browser needs access to cdnjs (see how-to-run.md), and
  the database needs data (copy it over or run a backfill).
