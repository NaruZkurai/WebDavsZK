#!/usr/bin/env bash
# Pull the latest repo and redeploy the file-mover daemon (rebuild + restart).
# Intended to be launched DETACHED (nohup) by the server's POST /api/update so
# the running daemon can update itself and restart. Logs go to
# /tmp/file-mover-update.log.
set -uo pipefail
cd "$(dirname "$0")/.."

echo "[$(date '+%F %T')] update-file-mover: pull + rebuild + restart"
echo "repo before: $(git rev-parse --short HEAD)"

if ! git pull; then
  echo "[$(date '+%F %T')] git pull FAILED — aborting (server not restarted)"
  exit 1
fi

echo "[$(date '+%F %T')] git pull OK, new HEAD: $(git rev-parse --short HEAD)"

# Rebuild + restart the daemon (deploy-file-mover.sh bakes GIT_COMMIT itself).
if ! ./deploy-file-mover.sh; then
  echo "[$(date '+%F %T')] deploy-file-mover FAILED"
  exit 1
fi

echo "[$(date '+%F %T')] done. running commit: $(curl -s --max-time 5 http://0.0.0.0:8787/api/version || echo '?')"
