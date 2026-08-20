#!/usr/bin/env bash
# git push, then notify configured receivers to auto-update via their webhook
# so servers pick up the new code the moment it's pushed (instead of waiting
# for the nightly timer).
#
# Receivers + shared secret come from config/env (gitignored):
#   NZK_WEBDAVS_RECEIVERS="https://192.0.2.1:8443 https://192.0.2.2:51337"
#   NZK_WEBDAVS_UPDATE_SECRET=<same secret as configured on each receiver>
#
# Usage:  ./push.sh          (push to origin, then notify)
set -euo pipefail
cd "$(dirname "$0")"

if [[ -f config/env ]]; then
    set -a
    # shellcheck disable=SC1091
    source config/env
    set +a
fi

git push "$@"

SECRET="${NZK_WEBDAVS_UPDATE_SECRET:-}"
RECEIVERS="${NZK_WEBDAVS_RECEIVERS:-}"
if [[ -z "$SECRET" || -z "$RECEIVERS" ]]; then
    echo "==> no webhook configured (NZK_WEBDAVS_UPDATE_SECRET / NZK_WEBDAVS_RECEIVERS)"
    echo "    push done; receivers NOT notified."
    exit 0
fi

for base in $RECEIVERS; do
    url="${base%/}/.nzk-webdavs-update"
    echo "==> notifying ${base}"
    code=$(curl -k -s -o /dev/null -w '%{http_code}' --max-time 10 -X POST "$url" \
        -H "X-Nzk-Update-Token: ${SECRET}" || echo "ERR")
    echo "    -> HTTP ${code}"
done
