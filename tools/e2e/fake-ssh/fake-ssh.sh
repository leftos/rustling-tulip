#!/usr/bin/env bash
# POSIX shim for the deterministic ssh stand-in. Same role as fake-ssh.cmd.
# Point RUSTLING_TULIP_SSH at it directly.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec node "$DIR/index.mjs" "$@"
