#!/usr/bin/env bash
# Pull updates from git, rebuild, and restart the nzk-webdavs service.
#
# Safe: config/env, certs/ and webdav_root are gitignored, so `git pull` never
# conflicts with your local settings. Local changes to *tracked* files will
# abort the pull (commit or stash them first).
#
# Usage:
#   ./update.sh                                     # origin/main
#   NZK_WEBDAVS_REMOTE=upstream NZK_WEBDAVS_BRANCH=main ./update.sh
#
# Run as root (or the repo owner) on the server; restart needs systemctl access.
set -euo pipefail
cd "$(dirname "$0")"

REMOTE="${NZK_WEBDAVS_REMOTE:-origin}"
BRANCH="${NZK_WEBDAVS_BRANCH:-main}"

echo "==> Fetching $REMOTE ..."
git fetch "$REMOTE" 2>/dev/null || { echo "error: git fetch failed" >&2; exit 1; }

BEFORE=$(git rev-parse HEAD)
echo "==> Pulling latest from $REMOTE/$BRANCH ..."
git pull --ff-only "$REMOTE" "$BRANCH" 2>/dev/null || {
    echo "error: pull failed (uncommitted changes to tracked files?)" >&2
    exit 1
}

if [[ "$(git rev-parse HEAD)" == "$BEFORE" ]]; then
    echo "==> Already up to date ($(git rev-parse --short HEAD))"
    exit 0
fi

echo "==> Updated: $(git rev-parse --short "$BEFORE") -> $(git rev-parse --short HEAD)"
echo "==> Rebuilding ..."
./build.sh

if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet nzk-webdavs 2>/dev/null; then
    echo "==> Restarting nzk-webdavs service ..."
    systemctl restart nzk-webdavs
else
    echo "==> nzk-webdavs service not active - rebuilt; start it with ./launch.sh or systemctl."
fi

echo "==> Done: $(git rev-parse --short HEAD)"
