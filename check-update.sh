#!/usr/bin/env bash
# Compare the RUNNING file-mover daemon's baked-in git commit (GET /api/version)
# against the commit checked out in this repo. If they differ, pull/sync the
# repo and rebuild + restart the daemon; if they match, do nothing.
#
#   ./check-update.sh          # check and, if stale, pull + rebuild + restart
#   ./check-update.sh --check  # only report, never touch the daemon
#
# URL/ports and the deploy command can be overridden:
#   FM_URL=http://0.0.0.0:8787  FM_DEPLOY=./deploy-file-mover.sh ./check-update.sh
set -euo pipefail
cd "$(dirname "$0")"

FM_URL="${FM_URL:-http://0.0.0.0:8787}"
FM_DEPLOY="${FM_DEPLOY:-./deploy-file-mover.sh}"
CHECK_ONLY=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1

local_commit="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "repo commit:    ${local_commit}"

running_commit="$(curl -s --max-time 3 "${FM_URL}/api/version" | sed -n 's/.*"commit":"\([^"]*\)".*/\1/p' || true)"
if [ -z "$running_commit" ]; then
  echo "running commit: N/A (is the daemon up at ${FM_URL}?)"
  exit 1
fi
echo "running commit: ${running_commit}"

if [ "$local_commit" = "$running_commit" ]; then
  echo "==> up to date (${local_commit}) — no restart needed."
  exit 0
fi

echo "==> STALE: running ${running_commit}, repo is ${local_commit}"
if [ "$CHECK_ONLY" = 1 ]; then
  echo "    (--check: not touching the daemon)"
  exit 2
fi

echo "==> pulling latest..."
git pull

echo "==> rebuilding + restarting daemon ($FM_DEPLOY)..."
"$FM_DEPLOY"

echo "==> verifying new commit:"
curl -s --max-time 5 "${FM_URL}/api/version"
echo
echo "==> done. Reload the file-mover browser tab to pick up the new UI."
