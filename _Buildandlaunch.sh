#!/usr/bin/env bash
# Build and launch nzk-webdavs in one step.
#
#   ./_Buildandlaunch.sh            # release build + launch
#   ./_Buildandlaunch.sh --debug    # debug build + launch
#   ./_Buildandlaunch.sh --verbose  # extra args are passed to the server
set -euo pipefail
cd "$(dirname "$0")"

if [[ "${1:-}" == "--debug" ]]; then
    export NZK_WEBDAVS_PROFILE="debug"
    shift
fi

./build.sh
./launch.sh "$@"
