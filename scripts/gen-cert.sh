#!/usr/bin/env bash
# Generate a self-signed certificate for nzk-webdavs.
#
# Usage:
#   ./scripts/gen-cert.sh                       # defaults: certs/server.{crt,key}
#   SAN="nas.local,192.0.2.10" ./scripts/gen-cert.sh
set -euo pipefail
cd "$(dirname "$0")/.."

cargo run --release -- --gen-cert "$@"
