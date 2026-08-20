#!/usr/bin/env bash
# Full install: **client + server**.
#
#   * server  = nzk-webdavs WebDAVS server (systemd SYSTEM service, /srv/webdav)
#   * client  = the two-panel file-mover UI (systemd USER service, port 8787,
#               browsable from other LAN devices)
#
#   ./install.sh                          # build + install server and client
#   ./install.sh --skip-build             # install from existing release binaries
#
# Requires: rust/cargo, sudo (for the server system service), systemd.
# The client (file-mover) service is installed under YOUR user (systemctl
# --user); the server is installed as a root system service.
set -euo pipefail
REPO="$(cd "$(dirname "$0")" && pwd)"
cd "$REPO"

SKIP_BUILD=0
[[ "${1:-}" == "--skip-build" ]] && SKIP_BUILD=1

command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found" >&2; exit 1; }
command -v sudo >/dev/null 2>&1 || { echo "error: sudo required (for the server service)" >&2; exit 1; }
command -v systemctl >/dev/null 2>&1 || { echo "error: systemd required" >&2; exit 1; }

echo "====================================================="
echo " WebDavsZK — installing client (file-mover) + server"
echo "====================================================="

# ---------- 0. Build everything ----------
if [[ "$SKIP_BUILD" == "1" ]]; then
    echo "==> --skip-build: using existing release binaries"
else
    echo "==> Building server (nzk-webdavs) ..."
    ./build.sh
    echo "==> Building client (file-mover + file-mover-sudo) ..."
    GIT_COMMIT=$(git rev-parse --short HEAD) \
        cargo build --release --bin file-mover --bin file-mover-sudo
fi

# ---------- 1. Install the server (system service) ----------
echo "==> Installing server ..."
./install-server.sh --skip-build --yes

# ---------- 2. Install the client (user file-mover service) ----------
echo "==> Installing client (file-mover user service) ..."
mkdir -p "$HOME/.config/systemd/user"

UNIT="$HOME/.config/systemd/user/file-mover.service"
if [[ -f "$UNIT" ]]; then
    echo "    (client unit already exists: $UNIT)"
    echo "    leaving it as-is; starting it below."
else
    echo "    writing $UNIT"
    sed "s|__REPO__|$REPO|g" deploy/file-mover.service > "$UNIT"
fi

systemctl --user daemon-reload
systemctl --user enable --now file-mover 2>/dev/null || systemctl --user start file-mover
sleep 1

echo ""
echo "====================================================="
echo " Install complete."
echo "  - Server (WebDAVS):  sudo systemctl status nzk-webdavs"
echo "    Edit /etc/nzk-webdavs/nzk-webdavs.env, generate a cert, then:"
echo "      sudo systemctl enable --now nzk-webdavs"
echo "  - Client (file-mover): http://0.0.0.0:8787/"
echo "    systemctl --user status file-mover"
echo "====================================================="
