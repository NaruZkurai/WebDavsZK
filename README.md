# WebDavsZK

A small, robust **Rust WebDAVS** (WebDAV over TLS) server, tuned for two things:

1. **Recursive file uploads** — directory trees copied via WebDAV (`MKCOL` + `PUT`,
   executed in parallel by clients like KDE Dolphin / KIO) are handled correctly.
2. **KDE KIO bug [KDE #422668](https://bugs.kde.org/show_bug.cgi?id=422668)
   "WebDAV copy fails in the middle"** — large copies (typically > 500 MB) used
   to die mid-transfer with `connection was disconnected`. The server side now
   does everything it can to make those transfers succeed.

It is built on [`dav-server`](https://crates.io/crates/dav-server) (a maintained
fork of `webdav-handler`), `hyper` 1.x, `tokio` and `rustls`.

---

## Why the KIO bug happens, and what this server does about it

KIO's WebDAV worker buffers aggressively and reports inflated copy speeds, then
the connection drops "in the middle". Maintainers suspect an HTTP timeout on the
server side of long transfers ([comment 5](https://bugs.kde.org/show_bug.cgi?id=422668#c5)).
Many WebDAV servers kill connections that take too long or buffer entire uploads
in RAM, making the problem far worse.

This server's mitigations (all implemented in `src/server.rs`):

| # | Mitigation | Effect |
|---|------------|--------|
| 1 | **No request-body read/idle/write timeout** | hyper's HTTP/1.1 connection is built *without a timer*, so a PUT can stream for as long as it needs. Keep-alive only applies *between* requests, never during an active body stream. This is the key fix for #422668. |
| 2 | **Streaming `PUT` to disk** | each body chunk is written to the file as it arrives (`dav-server` `LocalFs`); RAM stays flat for multi-GB uploads. |
| 3 | **`Expect: 100-continue` handled natively** | hyper answers `100 Continue` as soon as the body is polled, so KIO's preflighted large PUTs start cleanly instead of hanging. |
| 4 | **Chunked transfer-encoding** | supported natively by hyper (KIO uses it when the length isn't known in advance). |
| 5 | **Partial PUT / resume** | Apache `Content-Range` and SabreDAV `X-Update-Range` partial uploads are implemented; `Accept-Ranges: bytes` is advertised. Clients that can resume don't have to restart a multi-GB copy. |
| 6 | **`X-Expected-Entity-Length`** | honoured (used by macOS Finder and other clients). |
| 7 | **Concurrent connections** | each connection runs in its own task, so KIO's parallel `MKCOL`/`PUT` requests during a recursive directory copy don't serialize. |
| 8 | **Graceful shutdown** | on `SIGTERM`/`SIGINT` in-flight transfers get up to 30 s to finish instead of being truncated mid-upload. |
| 9 | **TCP keep-alive** | idle connections send keep-alive probes (default 60 s) so they stay alive through NATs/firewalls during long pauses such as KIO's overwrite dialog, instead of being dropped mid-session. |

> **Note:** the *fake copy speed* you see in Dolphin is KIO's own read-ahead
> buffering — no server can fix that display. But with the above the server no
> longer *causes* the mid-copy disconnect.

---

## Features

- **WebDAV over TLS (WebDAVS)** out of the box; self-signed cert generator included.
- Full RFC 4918 methods: `PROPFIND`, `PROPPATCH`, `MKCOL`, `PUT`, `GET`/`HEAD`,
  `DELETE`, `COPY`, `MOVE`, `LOCK`, `UNLOCK`, `OPTIONS` — passes the WebDAV
  [Litmus](http://www.webdav.org/neon/litmus/) test suite.
- **Recursive operations**: recursive `COPY`/`MOVE` within the server, plus
  correct handling of client-driven recursive uploads (`MKCOL` + `PUT` trees).
- HTML directory index on `GET` of a folder (handy for a browser sanity check).
- Optional HTTP **Basic auth** (constant-time password comparison).
- Optional URL prefix (`https://host:8443/dav/...`).
- Plain-HTTP mode for debugging or when fronted by a reverse proxy (Caddy,
  HAProxy, nginx) that terminates TLS.
- Configurable entirely via CLI flags **or** environment variables (systemd friendly).

## Requirements

- Rust **1.85+** (edition 2024). Works on stable and nightly.
- Linux / macOS / Windows (Linux is what the systemd unit targets).

## Quick start

```bash
# 1. Build
cargo build --release

# 2. Generate a self-signed certificate
./scripts/gen-cert.sh
#    (overrides: NZK_WEBDAVS_CERT, NZK_WEBDAVS_KEY, NZK_WEBDAVS_CERT_SAN=...)

# 3. Create the share and run
mkdir -p /srv/webdav
RUST_LOG=info ./target/release/nzk-webdavs --root /srv/webdav \
    --auth-user webdav --auth-pass 'change-me' \
    --cert certs/server.crt --key certs/server.key
```

Then connect from Dolphin/KIO:

```
webdavs://webdav@your-host:8443/
```

or with a URL prefix:

```
webdavs://webdav@your-host:8443/dav/
```

Smoke test with curl:

```bash
# OPTIONS + DAV compliance header
curl -k -u webdav:'change-me' -X OPTIONS -i https://0.0.0.0:8443/

# recursive-ish upload: make a directory, put a file into it
curl -k -u webdav:'change-me' -X MKCOL https://0.0.0.0:8443/testdir
curl -k -u webdav:'change-me' -T bigfile.iso https://0.0.0.0:8443/testdir/bigfile.iso

# PROPFIND a directory (Depth: infinity shows the whole tree)
curl -k -u webdav:'change-me' -X PROPFIND -H 'Depth: 1' https://0.0.0.0:8443/testdir/
```

## Configuration

Everything is a CLI flag with a matching environment variable
(`NZK_WEBDAVS_*`). Run `nzk-webdavs --help` for the full list.

**Recommended: edit `config/env`.** On first `./launch.sh` it is auto-created
from `config/env.example`, and it is **gitignored** — so updating the repo
with `git pull` never conflicts with your local settings (no stashing). Edit
`config/env`, then restart. For one-off overrides, command-line flags still
take precedence.

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--bind` | `NZK_WEBDAVS_BIND` | `0.0.0.0:8443` | Listen address. |
| `--root` | `NZK_WEBDAVS_ROOT` | `/srv/webdav` | Directory served over WebDAV. |
| `--prefix` | `NZK_WEBDAVS_PREFIX` | *(empty)* | URL prefix to strip, e.g. `/dav`. |
| `--cert` | `NZK_WEBDAVS_CERT` | `certs/server.crt` | PEM certificate chain. |
| `--key` | `NZK_WEBDAVS_KEY` | `certs/server.key` | PEM private key. |
| `--no-tls` | `NZK_WEBDAVS_NO_TLS` | off | Serve plain HTTP (debug/reverse proxy). |
| `--gen-cert` | `NZK_WEBDAVS_GEN_CERT` | off | Generate a self-signed cert and exit. |
| `--cert-san` | `NZK_WEBDAVS_CERT_SAN` | `localhost,0.0.0.0,::1` | SANs for the generated cert. |
| `--auth-user` | `NZK_WEBDAVS_AUTH_USER` | *(empty)* | Basic-auth username (empty = no auth). |
| `--auth-pass` | `NZK_WEBDAVS_AUTH_PASS` | *(empty)* | Basic-auth password. |
| `--principal` | `NZK_WEBDAVS_PRINCIPAL` | `nzk-webdavs` | Lock owner reported to clients. |
| `--create-parents` | `NZK_WEBDAVS_CREATE_PARENTS` | `true` | Auto-create missing parent folders on `PUT`/`MKCOL` (KIO recursive upload fix). Set `false` for strict RFC 4918 (`409` when the parent is missing). |
| `--atomic-writes` | `NZK_WEBDAVS_ATOMIC_WRITES` | `true` | Write uploads to a temp file and rename into place on success, so readers never see a partial file and aborted uploads leave no corrupt file. |
| `--verbose` | `NZK_WEBDAVS_VERBOSE` | off | Debug logging (`RUST_LOG` also works). |

`--auth-user` and `--auth-pass` must be set together (or neither).

## Recursive uploads (and the "file doesn't exist" copy failure)

When KIO/Dolphin copies a folder tree it sends `MKCOL` + `PUT` requests, and
several KIO versions send them in parallel or skip `MKCOL` for folders it
thinks already exist. A strict server (like a bare `dav-server`) answers a
`PUT` into a not-yet-created folder with `409 Conflict`, which KIO reports as
*"the file/folder does not exist"* and the copy stops midway.

By default this server **auto-creates missing parent folders** on `PUT` and
`MKCOL` (like Nextcloud/SabreDAV do), so recursive uploads succeed regardless
of request ordering. To get strict RFC 4918 behavior back:

```bash
nzk-webdavs --create-parents=false ...
```

## Reading and writing at the same time (atomic writes)

By default uploads are **atomic**: a `PUT` streams into a temp file in the same
directory and is `rename()`d into place only when the upload completes. The
in-progress file is visible as `{name}.uploading.nzk_webdavs` and becomes
`{name}` once the upload is confirmed done. This means:

- a client downloading (or listing) a file while it is being uploaded sees
either the old file or the new file, **never a half-written one**;
- a failed or interrupted upload **leaves no corrupt/partial file** at the
destination — the temp file is simply removed;
- a restarted copy therefore starts from a clean state instead of fighting a
partial file left by the previous attempt.

Uploads stream chunk-by-chunk to a `.uploading.nzk_webdavs` temp file and are
atomically renamed into place once verified complete (small files can fail
mid-transfer too, so **all uploads get the same temp-file protection**).
Readers never see a partial file and an aborted upload leaves no corrupt file
behind. When the size is known in advance (`Content-Length`), the server checks
the remaining disk space (`statvfs`) before starting, so a `PUT` that can't fit
fails immediately with `507 Insufficient Storage` — with no slow upfront
pre-allocation that would delay the start of the transfer. Every upload's
SHA-256 is recorded on the file (`user.nzk_webdavs.sha256` xattr), and a
client-provided `OC-Checksum: SHA256:` header is verified before the file is
finalized (mismatch = upload rejected, temp removed). Disable atomic writes
with `--atomic-writes=false` if you need direct-to-target semantics.

### Many small files are slow?

Per-file `fsync` was the main cost when moving thousands of small files
(especially on spinning disks). It is now **off by default** — the OS page
cache plus the atomic rename still guarantee no partial/corrupt files, and
`--fsync=true` (`NZK_WEBDAVS_FSYNC=true`) restores per-file durability if you
need power-loss safety over speed.

Clients that support *resumable* uploads (which send byte ranges, e.g. via
Apache `Content-Range` or SabreDAV `X-Update-Range` partial `PUT`) are handled
by `dav-server` natively. Note that KIO/Dolphin itself does **not** resume —
when a copy fails it restarts the whole file from byte 0, which is exactly what
the atomic-temp-write path is designed to handle cleanly.

## Installing (server / client / both)

Two one-shot install scripts are provided. "Server" = the **nzk-webdavs**
WebDAVS server (systemd **system** service, `webdav` user, `/srv/webdav`).
"Client" = the **file-mover** two-panel UI (systemd **user** service, port
8787, browsable from other LAN devices).

```bash
# Install BOTH client + server:
./install.sh

# Install ONLY the server:
./install-server.sh

# Install from existing release binaries (skip the cargo build):
./install.sh --skip-build
./install-server.sh --skip-build
```

What `./install.sh` does:

1. Builds the release binaries (`nzk-webdavs`, `file-mover`, `file-mover-sudo`).
2. Installs the **server** system service — creates the `webdav` user and
   `/srv/webdav`, installs `target/release/nzk-webdavs` → `/usr/local/bin`,
   `deploy/nzk-webdavs.service` → `/etc/systemd/system`, and (unless one
   exists) `deploy/nzk-webdavs.env` → `/etc/nzk-webdavs/nzk-webdavs.env`.
3. Installs the **client** `file-mover` user service to
   `~/.config/systemd/user/file-mover.service` (generated from
   `deploy/file-mover.service`, so it points at this repo's release binary),
   then `systemctl --user enable --now file-mover`.

`./install-server.sh` does the same *server* parts only (steps 1–2) and skips
the client.

After installing the server, you still must:

```bash
# 1. edit /etc/nzk-webdavs/nzk-webdavs.env  (set a real password + cert paths)
sudo nano /etc/nzk-webdavs/nzk-webdavs.env

# 2. generate a self-signed cert
sudo NZK_WEBDAVS_CERT=/etc/nzk-webdavs/server.crt \
     NZK_WEBDAVS_KEY=/etc/nzk-webdavs/server.key \
     target/release/nzk-webdavs --gen-cert

# 3. start it
sudo systemctl enable --now nzk-webdavs
```

The client (file-mover) depends on your **rclone mounts** (`~/webdav`, `~/webdav-local`).
If the file-mover uses any rclone-mount user services, add them to the
`After=`/`Wants=` lines of `~/.config/systemd/user/file-mover.service`. It
listens on `0.0.0.0:8787` so LAN devices can open it in a browser.

## Running as a systemd service

`deploy/nzk-webdavs.env` is a **template** — install a copy to
`/etc/nzk-webdavs/nzk-webdavs.env` and edit that file (the repo copy is never
your live config, so `git pull` can't conflict with it).

```bash
sudo useradd --system --home /srv/webdav --create-home webdav

sudo install -D -m 0644 deploy/nzk-webdavs.env /etc/nzk-webdavs/nzk-webdavs.env
sudo install -D -m 0755 target/release/nzk-webdavs /usr/local/bin/nzk-webdavs
sudo install -D -m 0644 deploy/nzk-webdavs.service /etc/systemd/system/nzk-webdavs.service

# edit /etc/nzk-webdavs/nzk-webdavs.env (set a real password, cert paths)
sudo systemctl daemon-reload
sudo systemctl enable --now nzk-webdavs
```

### Updating

Because your runtime settings live in gitignored files, updating is just:

```bash
git pull          # never conflicts with config/env or your certs
./build.sh        # or ./_Buildandlaunch.sh
# systemd users: sudo systemctl restart nzk-webdavs
```

**Automatic updates**: `./launch.sh` checks `origin` on every start and, if
behind, pulls + rebuilds automatically (disable with `NZK_WEBDAVS_AUTO_UPDATE=0`).
For periodic updates on a server, install the timer:

```bash
sudo install -D -m 0644 deploy/nzk-webdavs-update.service /etc/systemd/system/
sudo install -D -m 0644 deploy/nzk-webdavs-update.timer /etc/systemd/system/
sudo systemctl enable --now nzk-webdavs-update.timer   # daily at 04:00
```

or run `./update.sh` manually (pull + rebuild + restart). Because `config/env`,
`certs/` and `webdav_root` are gitignored, none of these ever touch your
settings.

**Update-on-push webhook**: the server can also update the instant a client
pushes, instead of waiting for the timer. Each receiver can expose a
secret-protected endpoint:

- On the receiver, set `NZK_WEBDAVS_UPDATE_SECRET=<shared secret>` (and
  `NZK_WEBDAVS_UPDATE_CMD` if its restart differs from `scripts/update.sh`).
- Then `POST /.nzk-webdavs-update` with header
  `X-Nzk-Update-Token: <secret>` triggers a git pull + rebuild + restart (the
  server replies `202` and runs the update detached). Missing/wrong token →
  `403`; the endpoint is disabled (404) when no secret is configured.
- On the machine that pushes, use `./push.sh` instead of bare `git push` — it
  pushes, then notifies every receiver listed in `NZK_WEBDAVS_RECEIVERS`
  (space-separated base URLs in `config/env`, sharing the same secret).

## Logging

Every request is logged by default (one line per request, `info` level):

```
2026-08-01 20:00:00 [INFO ] nzk_webdavs::server: REQ PUT /docs/file.txt -> 201 Created in 12ms from 192.0.2.10:47123 to 192.0.2.20:8443
```

- **Level**: `RUST_LOG` (e.g. `RUST_LOG=debug`) or `--verbose` for debug; default is `info`.
- **Target IP**: each line shows the target address the client connected to
  (local address). To show a specific IP instead (e.g. a public/NAT IP), set
  `NZK_WEBDAVS_TARGET_IP` / `--target-ip`.
- **Log file**: set `NZK_WEBDAVS_LOG_FILE` / `--log-file` to also append to a
  file (stderr is always used too). Example for systemd:
  `NZK_WEBDAVS_LOG_FILE=/var/log/nzk-webdavs.log` (rotate with logrotate).
- This is what to grab when debugging client (KIO) connection issues — the
  request log shows exactly what the client sent and what the server answered.

Generate the certificate into `/etc/nzk-webdavs` first (as root):

```bash
sudo NZK_WEBDAVS_CERT=/etc/nzk-webdavs/server.crt \
     NZK_WEBDAVS_KEY=/etc/nzk-webdavs/server.key \
     /usr/local/bin/nzk-webdavs --gen-cert
```

## TLS notes

- The included generator produces a self-signed cert. Clients will warn about
  the unknown CA; trust it once, or use your own CA / a reverse proxy with a
  Let's Encrypt certificate in front of `--no-tls` mode.
- **Never keep the TLS private key inside the served root.** Relative cert
  paths plus a working directory inside the root would expose the key over
  WebDAV. The server now **refuses to start** if the certificate or key
  resolves inside `--root`. Keep them at `/etc/nzk-webdavs/` (systemd) or in
  the repo `certs/` outside the served tree.
- With a reverse proxy (e.g. Caddy), point the proxy at the plain-HTTP port and
  disable TLS in the app:

  ```bash
  nzk-webdavs --no-tls --bind 0.0.0.0:8080 --root /srv/webdav
  ```

## Testing

- **Litmus** (the WebDAV conformance suite):

  ```bash
  # client side
  sudo apt install cadaver   # or: git clone git://github.com/neonwebdav/litmus
  litmus http://0.0.0.0:8080/ -k -u webdav -p 'change-me'
  ```

  `dav-server` passes the `basic`, `copymove`, `props`, `locks` and `http`
  suites.

- **Large transfer soak test** (exercises the #422668 path):

  ```bash
  dd if=/dev/zero of=/tmp/2gb.iso bs=1M count=2048
  curl -k -u webdav:'change-me' -T /tmp/2gb.iso https://0.0.0.0:8443/2gb.iso
  ```

  Watch memory stay flat (`top`) and the transfer finish without a disconnect.

## File mover (two-panel drag & drop UI)

A separate tool in this repo: a two-panel **file mover** web app that sits on
on top of the WebDAV mounts (`~/webdav` = Target PC / nas, `~/webdav-local` =
this PC) and lets you browse, move, copy, rename and delete files between the
two panels in a browser.

- **UI**: `http://0.0.0.0:8787/` — listens on all interfaces so LAN devices can
  open the mover in their own browser.
- **Server**: `src/bin/file-mover.rs` (embeds `tools/file-mover/index.html` at
  compile time). **Privileged helper**: `src/bin/file-mover-sudo.rs`, run via
  `sudo -S` only for permission-denied paths (your sudo password is used once,
  never stored or logged).
- **Security model**: the mover itself is a convenience UI over the *local*
  rclone mounts. The real data access is the **WebDAVS servers**, which are
  the authenticated endpoints — you connect to those with the
  `webdavs://user:pass@host:port/` login. Keep the WebDAVS `--auth-user/--auth-pass`
  credentials strong; treat the LAN as trusted (or firewall port 8787 if you
  want to restrict who can open the mover UI).
- **Run (dev)**: `cargo run --bin file-mover`
  **Run (release)**: `cargo build --release --bin file-mover --bin file-mover-sudo`
  then `~/.local/bin/...` or the systemd user service below.

### Updating / restarting (README for "how do I get the update")

**Important:** the file-mover is a **separate user systemd service** from the
`nzk-webdavs` WebDAVS server — updating the WebDAV server does **not** update
the file-mover.

```bash
cd /nzk/git/NZK_WEBDAVS
git pull
./deploy-file-mover.sh        # build release + systemctl --user restart file-mover + verify 200
```

`deploy-file-mover.sh` does: `cargo build --release --bin file-mover
--bin file-mover-sudo` → `systemctl --user daemon-reload` →
`systemctl --user restart file-mover` → checks `http://0.0.0.0:8787/` returns
200.

Because `tools/file-mover/index.html` is embedded at **compile time**, editing
the HTML or `src/bin/file-mover.rs` has **no effect** until you rebuild +
restart — this is the #1 gotcha. **Always run `./deploy-file-mover.sh` (or at
minimum `cargo build … && systemctl --user restart file-mover`) after any
file-mover change.** Then **reload the browser tab** to pick up the new
embedded JS.

Manual status/restart:

```bash
systemctl --user status file-mover
systemctl --user restart file-mover
journalctl --user -u file-mover -f     # logs
```

### Version checking / auto-sync ("is it the same commit?")

Every build embeds the **git commit** it was compiled from, exposed by the
daemon:

```bash
curl -s http://0.0.0.0:8787/api/version
# {"commit":"7b70e7f","version":"0.1.0","port":8787}
```

The commit is embedded by `deploy-file-mover.sh` via
`GIT_COMMIT=$(git rev-parse --short HEAD)` + `build.rs`.

To compare the running daemon's commit against this repo and, if they differ,
pull/sync + rebuild + restart the daemon automatically:

```bash
./check-update.sh            # check; if stale: git pull + ./deploy-file-mover.sh
./check-update.sh --check    # ONLY report, don't touch the daemon (exit 2 = stale)
```

**Self-update from the UI**: the header shows the running version/commit and
has an **⬆ Update server** button. It calls `POST /api/update`, which launches
`scripts/update-file-mover.sh` **detached** (`nohup`) to `git pull` + rebuild +
restart the daemon (log at `/tmp/file-mover-update.log`), then the UI shows an
**"Updating server…"** overlay, keeps polling `/api/version`, and auto-reloads
when the daemon is back. The UI also polls `/api/version` every 30 s and, if
the running commit ever differs from the one this tab loaded (i.e. this client
is stale), it logs "server updated … — reloading to sync" and reloads.

### Features

- Two panels, each independently pointed at **This PC** or **Target PC**
  (per-panel PC dropdown), plus a `⇄ swap` button.
- Browse, **up/refresh/upload**, paste a `webdav://` / `webdavs://` URL into a
  path box, and a background **search** (name / `*.ext` / `*.a;*.b`).
- **Selection**: Ctrl/Cmd+click toggles, **Shift+click selects a contiguous
  range**, right-click for Copy name/path, Rename, **Set as target window**,
  and Delete. Middle bar: `⇄`, `⧉ Copy →`, `Move →`, red `🗑 Delete`.
- **Move/Copy** run as background jobs with live file/folder/byte progress in a
  jobs panel (they keep running even after you close the tab). The global
  **"no confirm"** checkbox (top bar) skips the confirm dialog for bulk
  operation.
- Move/copy conflict dialog: **Merge / Overwrite / Rename**.
- **Delete** runs as a background job too, with file/folder counts and progress.
- Performance: deletes are **multithreaded**; **same-drive** moves (e.g.
  `sda4 → sda4`) use a single `mv -r`-style rename with **no data copy**;
  **cross-drive** moves (e.g. `sda4 → sdc2`) and copies stream data through a
  multithreaded copy loop. Duplicate submissions are **deduped** so the same
  file is never copied twice to the same place.

See `/memories/…` notes in the repo for the nitty-gritty of each fix.

## Project layout

```
src/
  main.rs      # entry point, CLI parsing, logging
  config.rs    # configuration (flags + env vars) and validation
  server.rs    # hyper server loop, no-timeout connections, graceful shutdown
  tls.rs       # rustls server config + self-signed cert generation
  auth.rs      # HTTP Basic auth guard
  bin/
    file-mover.rs        # two-panel file-mover server (localhost:8787)
    file-mover-sudo.rs   # privileged helper for permission-denied paths
  autofs.rs    # AutoMkcolFs (auto-create parents) — see server.rs
  logging.rs   # request logging
tools/
  file-mover/
    index.html # the file-mover UI (embedded into file-mover.rs at build time)
deploy/        # systemd unit + env file (WebDAVS server)
deploy-file-mover.sh   # build + restart the file-mover user service
scripts/       # cert generation helper
```

## License

Apache-2.0
