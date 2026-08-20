#!/usr/bin/env bash
# Build the file-mover (which embeds index.html via include_str!) and redeploy
# the running systemd user service, so every source/HTML change is actually
# pushed to the running app.
#
#   ./deploy-file-mover.sh            # release build + restart service
#
# The file-mover service embeds tools/file-mover/index.html at compile time, so
# ANY edit to index.html or src/bin/file-mover.rs REQUIRES a rebuild + restart
# to take effect. Run this every time you change file-mover code or UI.
set -euo pipefail
cd "$(dirname "$0")"

echo "==> building release file-mover + file-mover-sudo"
# Embed the git commit so GET /api/version reports exactly what's running.
GIT_COMMIT=$(git rev-parse --short HEAD) \
  cargo build --release --bin file-mover --bin file-mover-sudo

echo "==> restarting file-mover user service"
systemctl --user daemon-reload
systemctl --user restart file-mover

sleep 1
echo "==> verifying"
systemctl --user --no-pager status file-mover 2>&1 | grep -E 'Active|Main PID' || true
curl -s -o /dev/null -w "file-mover http://0.0.0.0:8787/ -> %{http_code}\n" http://0.0.0.0:8787/ || echo "(file-mover not up)"
echo "==> done"
