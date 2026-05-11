#!/usr/bin/env bash
# POSIX shim for the deterministic claude stand-in. Same role as
# fake-claude.cmd. Symlink/copy this onto your PATH or point
# RUSTLING_TULIP_CLAUDE at it directly.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec node "$DIR/index.mjs" "$@"
