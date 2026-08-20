#!/usr/bin/env bash
# Install ONLY the nzk-webdavs WebDAVS **server** as a systemd system service.
#
#   ./install-server.sh            # build + install server (will prompt for sudo)
#   ./install-server.sh --skip-build   # install existing target/release binary
#   ./install-server.sh --yes          # don't prompt (installs the auto-update timer too)
#
# What this does:
#   1. Builds the release `nzk-webdavs` binary.
#   2. Creates the `webdav` system user (home=/srv/webdav).
#   3. Installs:
#        deploy/nzk-webdavs.env     -> /etc/nzk-webdavs/nzk-webdavs.env  (first time only)
#        target/release/nzk-webdavs -> /usr/local/bin/nzk-webdavs
#        deploy/nzk-webdavs.service -> /etc/systemd/system/nzk-webdavs.service
#   4. (Optional) installs the daily auto-update timer.
#
# After it finishes, EDIT /etc/nzk-webdavs/nzk-webdavs.env (set a real password
# and cert paths), generate a cert, then:
#     sudo systemctl enable --now nzk-webdavs
#
# The "client" (file-mover UI) is NOT installed; run ./install.sh for that.
set -euo pipefail
REPO="$(cd "$(dirname "$0")" && pwd)"
cd "$REPO"

SKIP_BUILD=0
ASSUME_YES=0
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=1 ;;
        --yes)        ASSUME_YES=1 ;;
        *) echo "warning: ignoring unknown arg '$arg'" >&2 ;;
    esac
done

command -v sudo >/dev/null 2>&1 || { echo "error: sudo required" >&2; exit 1; }
command -v systemctl >/dev/null 2>&1 || { echo "error: systemctl (systemd) required" >&2; exit 1; }

# ---------- 1. Build ----------
if [[ "$SKIP_BUILD" == "1" ]]; then
    echo "==> --skip-build: using existing binary (target/release/nzk-webdavs)"
else
    echo "==> Building nzk-webdavs (release) ..."
    ./build.sh
fi
[[ -x target/release/nzk-webdavs ]] || { echo "error: target/release/nzk-webdavs not found" >&2; exit 1; }

# ---------- 2. system user + directories ----------
echo "==> Ensuring 'webdav' system user and /srv/webdav ..."
if ! id webdav >/dev/null 2>&1; then
    sudo useradd --system --home /srv/webdav --create-home webdav
else
    echo "    (user 'webdav' already exists)"
fi
sudo mkdir -p /etc/nzk-webdavs /srv/webdav

# ---------- 3. install files (never clobber an existing env) ----------
if [[ ! -f /etc/nzk-webdavs/nzk-webdavs.env ]]; then
    sudo install -D -m 0644 deploy/nzk-webdavs.env /etc/nzk-webdavs/nzk-webdavs.env
else
    echo "    (keeping existing /etc/nzk-webdavs/nzk-webdavs.env)"
fi
sudo install -D -m 0755 target/release/nzk-webdavs /usr/local/bin/nzk-webdavs
sudo install -D -m 0644 deploy/nzk-webdavs.service /etc/systemd/system/nzk-webdavs.service

echo "==> Optional: install the daily auto-update timer (04:00) ?"
if [[ "$ASSUME_YES" == "1" ]]; then
    ans="y"
else
    read -r -p "    install auto-update timer? [y/N] " ans
fi
if [[ "${ans,,}" == "y" || "${ans,,}" == "yes" ]]; then
    sudo install -D -m 0644 deploy/nzk-webdavs-update.service /etc/systemd/system/
    sudo install -D -m 0644 deploy/nzk-webdavs-update.timer /etc/systemd/system/
    sudo systemctl daemon-reload
    sudo systemctl enable --now nzk-webdavs-update.timer || true
    echo "==> auto-update timer enabled (daily 04:00)"
else
    sudo systemctl daemon-reload
fi

# ---------- 4. ownership + next steps ----------
sudo chown -R webdav:webdav /srv/webdav

echo ""
echo "==> Server installed."
echo "    NEXT STEPS:"
echo "    1. Edit  /etc/nzk-webdavs/nzk-webdavs.env  and set a real password and cert paths."
echo "    2. Generate a self-signed cert, e.g.: sudo NZK_WEBDAVS_CERT=/etc/nzk-webdavs/server.crt \\"
echo "           NZK_WEBDAVS_KEY=/etc/nzk-webdavs/server.key target/release/nzk-webdavs --gen-cert"
echo "    3. Start it:  sudo systemctl enable --now nzk-webdavs"
echo "    4. Check it:  systemctl status nzk-webdavs"
echo ""
echo "    To install the client (file-mover UI) too, run:  ./install.sh"
