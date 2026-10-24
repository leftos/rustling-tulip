#!/usr/bin/env bash
# POSIX shim for the deterministic tailscale stand-in. Same role as
# fake-tailscale.cmd. Point RUSTLING_TULIP_TAILSCALE at it directly.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec node "$DIR/index.mjs" "$@"
