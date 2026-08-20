#!/usr/bin/env bash
# Build nzk-webdavs.
#
#   ./build.sh            # release build
#   ./build.sh --debug    # debug build
#   NZK_WEBDAVS_PROFILE=debug ./build.sh
#
# Extra args are passed through to `cargo build`.
set -euo pipefail
cd "$(dirname "$0")"

PROFILE="${NZK_WEBDAVS_PROFILE:-release}"
if [[ "${1:-}" == "--debug" ]]; then
    PROFILE="debug"
    shift
fi

case "$PROFILE" in
    release) REL_FLAG="--release" ;;
    debug)   REL_FLAG="" ;;
    *)       echo "error: unknown profile '$PROFILE' (use 'release' or 'debug')" >&2; exit 1 ;;
esac

command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found on PATH" >&2; exit 1; }

echo "==> Building nzk-webdavs ($PROFILE) ..."
cargo build $REL_FLAG "$@"

echo "==> Done: target/$PROFILE/nzk-webdavs"
