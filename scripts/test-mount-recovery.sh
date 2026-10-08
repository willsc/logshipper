#!/usr/bin/env bash
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
binary=${1:-"$script_dir/../target/debug/logshipper"}
binary=$(realpath "$binary")
# Every mount operation occurs in a disposable private user/mount namespace.
exec unshare --user --map-root-user --mount --fork --propagation private \
    python3 "$script_dir/mount_recovery.py" "$binary"
