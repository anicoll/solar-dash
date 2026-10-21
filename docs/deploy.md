# Deploy solar-dash on Proxmox

This guide runs solar-dash in a Debian 12 (bookworm) LXC container on Proxmox,
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

Where things end up inside the LXC:

| Path | Contents |
| --- | --- |
| `/opt/solar-dash/solar-dash` | the program |
| `/etc/solar-dash/config.toml` | your config and secrets (only root and the service can read it) |
| `/var/lib/solar-dash/solar.db` | the database |
| `/var/backups/solar-dash/` | nightly backups |

## 1. Create the LXC

You need a Debian 12 template on Proxmox. Download it once on the Proxmox host:

```sh
pveam update
pveam available --section system | grep debian-12
pveam download local debian-12-standard_12.7-1_amd64.tar.zst
```

(Use the exact file name that `pveam available` shows; the version number may differ.)

Create the container. Change the id `120`, storage names, bridge and the
address to suit your network. This is unprivileged, with 1 core, 512 MB RAM
(use 2048 while you build inside it, see below) and 4 GB disk:

```sh
pct create 120 local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst \
  --hostname solar-dash \
  --unprivileged 1 \
  --cores 1 --memory 1024 --swap 512 \
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
the Debian 12 template, set Cores 1, Memory 2048 MB, Disk 4 GB, and on the
Network tab set a static IPv4/CIDR and gateway (or DHCP). Tick **Start at boot**.

Inside the container, as root:

```sh
apt update && apt full-upgrade -y
apt install -y git curl ca-certificates sqlite3
```

The LXC only needs outbound internet. The dashboard listens on
`127.0.0.1:8080`, and the tunnel connects to it from inside the container, so
nothing needs to reach the container from outside.

## 2. Build

### Option A (simplest): build inside the LXC

This keeps the binary matched to Debian 12. A release build can use more than 1 GB of RAM,
so give it 2048 MB while building. You can lower it to 512 MB afterwards.

```sh
apt install -y build-essential pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
git clone <your-repo-url> /root/solar-dash
cd /root/solar-dash
cargo build --release
```

The first build takes a few minutes. The result is `target/release/solar-dash`.

### Option B: build elsewhere and copy it

You can build on your Ubuntu laptop (`cargo build --release`) and copy it:

```sh
scp target/release/solar-dash root@192.168.1.50:/root/solar-dash-bin
```

Be careful: a binary built on a newer Ubuntu may need a newer glibc than
Debian 12 has and fail with `GLIBC_2.xx not found`. Debian 12 has glibc 2.36.
Building in the LXC (option A) avoids this. If you use option B you still need
the repo's `deploy/` folder and `config.example.toml` in the LXC, so clone the
repo there too.

## 3. Install and configure

From the repo checkout in the LXC, as root:

```sh
cd /root/solar-dash
./deploy/install.sh                      # option A
# ./deploy/install.sh /root/solar-dash-bin   # option B
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
  the `openssl` command, ending in `=`; leave out any `%`).
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

Without this step solar-dash starts with an empty database. You can backfill
later with
`sudo -u solar-dash /opt/solar-dash/solar-dash --config /etc/solar-dash/config.toml backfill --from 2023-02-21`.

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

It should print `{"ok":true,"last_reading_ts":...}`. If it fails, see
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

In the LXC, as root:

```sh
cd /root/solar-dash
git pull
cargo build --release
systemctl stop solar-dash
install -m 755 target/release/solar-dash /opt/solar-dash/solar-dash
systemctl start solar-dash
journalctl -u solar-dash -f
```

or build, then rerun `./deploy/install.sh` (it keeps your config and database)
and `systemctl restart solar-dash`. Database changes are applied automatically
on startup.

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
- **"login failed at startup"**: the iSolarCloud values are wrong. See the
  error table in [how-to-run.md](how-to-run.md#troubleshooting). Test with
  `/opt/solar-dash/solar-dash --config /etc/solar-dash/config.toml login-test`
  (run it as root so it can read the config).
- **"unable to open database file" / permission denied**: the database folder
  or file is not owned by the service user. Run
  `chown -R solar-dash:solar-dash /var/lib/solar-dash`.
- **`GLIBC_2.xx not found`**: the binary was built on a newer system. Build it
  inside the LXC (option A).
- **Logged out after every restart**: `session_secret` is not set.
- **Site not reachable**: check `systemctl status cloudflared` and
  `journalctl -u cloudflared -n 50`, that the tunnel id and hostname in
  `/etc/cloudflared/config.yml` are right, that the DNS route exists
  (`cloudflared tunnel route dns ...`), and that
  `curl http://127.0.0.1:8080/healthz` works inside the LXC. A 502 from
  Cloudflare means solar-dash is not running.
- **Charts empty**: the browser needs access to cdnjs (see how-to-run.md), and
  the database needs data (copy it over or run a backfill).
