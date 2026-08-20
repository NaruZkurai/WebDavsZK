#!/usr/bin/env bash
# Launch nzk-webdavs.
#
#   ./launch.sh                 # release binary, foreground (Ctrl-C to stop)
#   ./launch.sh --debug         # debug binary
#   ./launch.sh --verbose       # extra args are passed to the server
#
# Everything is configurable via NZK_WEBDAVS_* env vars (see `--help`).
set -euo pipefail
cd "$(dirname "$0")"

# Local runtime config (gitignored). Auto-created from config/env.example on
# first run, so `git pull` never conflicts with local settings. CLI flags
# still win over these values for one-off overrides.
if [[ ! -f config/env && -f config/env.example ]]; then
    echo "==> First run: creating config/env from config/env.example"
    echo "    edit config/env to set port, root, auth, certs, ..."
    cp config/env.example config/env
fi
if [[ -f config/env ]]; then
    set -a
    # shellcheck disable=SC1091
    source config/env
    set +a
fi

PROFILE="${NZK_WEBDAVS_PROFILE:-release}"
if [[ "${1:-}" == "--debug" ]]; then
    PROFILE="debug"
    shift
fi

# Auto-check for and apply updates from git (disable with NZK_WEBDAVS_AUTO_UPDATE=0).
# Safe because config/env, certs/ and webdav_root are gitignored. If you have
# uncommitted changes to tracked files, the pull is skipped with a warning.
if [[ -d .git && "${NZK_WEBDAVS_AUTO_UPDATE:-1}" != "0" ]] && git fetch --quiet 2>/dev/null; then
    LOCAL=$(git rev-parse HEAD 2>/dev/null || echo "")
    REMOTE=$(git rev-parse @{u} 2>/dev/null || echo "")
    if [[ -n "$LOCAL" && -n "$REMOTE" && "$LOCAL" != "$REMOTE" ]]; then
        if git merge-base --is-ancestor HEAD @{u} 2>/dev/null; then
            echo "==> Update available ($(git rev-parse --short HEAD) -> $(git rev-parse --short @{u}))"
            BEFORE=$(git rev-parse HEAD)
            if git pull --ff-only --quiet 2>/dev/null; then
                if [[ "$(git rev-parse HEAD)" != "$BEFORE" ]]; then
                    echo "==> Pulled, rebuilding ($PROFILE) ..."
                    NZK_WEBDAVS_PROFILE="$PROFILE" ./build.sh
                fi
            else
                echo "    warning: auto-update pull failed (uncommitted changes?); using current code" >&2
            fi
        else
            echo "==> Local is ahead of / diverged from origin (unpushed commits?) - no auto-update"
        fi
    fi
fi

BIN="target/$PROFILE/nzk-webdavs"

# Sensible local defaults (override with env vars).
export NZK_WEBDAVS_BIND="${NZK_WEBDAVS_BIND:-0.0.0.0:8443}"
export NZK_WEBDAVS_ROOT="${NZK_WEBDAVS_ROOT:-$(pwd)/webdav_root}"
export NZK_WEBDAVS_CERT="${NZK_WEBDAVS_CERT:-certs/server.crt}"
export NZK_WEBDAVS_KEY="${NZK_WEBDAVS_KEY:-certs/server.key}"

# Build the binary first if it doesn't exist yet.
if [[ ! -x "$BIN" ]]; then
    echo "==> Binary not found ($BIN), building ..."
    NZK_WEBDAVS_PROFILE="$PROFILE" ./build.sh
fi

# Generate a self-signed certificate if TLS is on and cert/key are missing.
if [[ -z "${NZK_WEBDAVS_NO_TLS:-}" && ( ! -f "$NZK_WEBDAVS_CERT" || ! -f "$NZK_WEBDAVS_KEY" ) ]]; then
    echo "==> Generating self-signed certificate ..."
    "$BIN" --gen-cert
fi

mkdir -p "$NZK_WEBDAVS_ROOT"

echo "==> Launching nzk-webdavs ($PROFILE)"
echo "    bind : $NZK_WEBDAVS_BIND"
echo "    root : $NZK_WEBDAVS_ROOT"
if [[ -n "${NZK_WEBDAVS_NO_TLS:-}" ]]; then
    echo "    tls  : disabled"
else
    echo "    tls  : $NZK_WEBDAVS_CERT"
fi
echo "    press Ctrl-C to stop"

exec "$BIN" "$@"
