#!/bin/sh
# Cargo/cc-crate linker+compiler shim for the armv7-unknown-linux-gnueabi target
# (webOS TV). Wraps the webosbrew NDK's arm-webos-linux-gnueabi-gcc with an
# explicit --sysroot: this toolchain build's baked-in default sysroot points at a
# build-time path segment that no longer exists post-relocate (confirmed via
# -print-sysroot vs the actual on-disk layout), so every invocation needs it passed
# explicitly. See `task toolchain` (Taskfile.yml) for how the NDK gets here — this
# script and its C++ counterpart (cxx-shim.sh) are the only build logic that stays
# outside the Taskfile: Cargo's `linker`/`CC`/`CXX` config need a real executable it
# invokes directly with a full compiler argv, not a task name.
set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NDK_DIR="${PUNKTFUNK_WEBOS_NDK:-$REPO_ROOT/.toolchains/arm-webos-linux-gnueabi_sdk-buildroot}"

exec "$NDK_DIR/bin/arm-webos-linux-gnueabi-gcc" \
    --sysroot="$NDK_DIR/arm-webos-linux-gnueabi/sysroot" "$@"
