#!/bin/sh
# C++ counterpart of cc-shim.sh — see that file for why --sysroot is explicit.
set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NDK_DIR="${PUNKTFUNK_WEBOS_NDK:-$REPO_ROOT/.toolchains/arm-webos-linux-gnueabi_sdk-buildroot}"

exec "$NDK_DIR/bin/arm-webos-linux-gnueabi-g++" \
    --sysroot="$NDK_DIR/arm-webos-linux-gnueabi/sysroot" "$@"
