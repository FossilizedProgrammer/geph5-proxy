#!/usr/bin/env bash
# Build static/standalone geph5-client binaries for Linux and Windows.
#
# Linux:   fully static ELF via crt-static on the gnu target (no system deps).
#          If the musl target is installed it is preferred (matches official CI).
# Windows: single .exe via x86_64-pc-windows-gnu, cross-compiled with mingw-w64.
#          Works without downloading the windows rust-std component: builds
#          std from source with -Zbuild-std + RUSTC_BOOTSTRAP=1 (see setup_rust_src).
#
# Requirements:
#   - Rust stable toolchain, gcc/make/perl (for vendored OpenSSL)
#   - Windows cross: mingw-w64 (x86_64-w64-mingw32-gcc)
#   - Network access to github.com (for the rust library source) and crates.io
#
# Usage:
#   ./build-static.sh            # Linux (default)
#   ./build-static.sh linux
#   ./build-static.sh windows
#   ./build-static.sh all
#   PKG=geph5-client ./build-static.sh all   # pick crate (default: geph5-proxy)
set -euo pipefail
cd "$(dirname "$0")"

PKG="${PKG:-geph5-proxy}"
BIN="binaries/$PKG"
OUT=dist
RUST_SRC_TAG="$(rustc --version | awk '{print $2}')"   # e.g. 1.97.1
SYSROOT="$(rustc --print sysroot)"
RUST_SRC="$SYSROOT/lib/rustlib/src/rust"

mkdir -p "$OUT"

have() { command -v "$1" >/dev/null 2>&1; }
have_target() { rustup target list --installed | grep -qx "$1"; }

# Install the rust library source into the sysroot (needed by -Zbuild-std).
# Uses a sparse git clone so only library/ is downloaded.
setup_rust_src() {
    if [ -f "$RUST_SRC/library/std/src/lib.rs" ] && [ -f "$RUST_SRC/library/backtrace/src/lib.rs" ]; then
        return
    fi
    echo "==> Fetching rust library source $RUST_SRC_TAG into sysroot"
    local tmp
    tmp=$(mktemp -d)
    git clone --depth 1 --branch "$RUST_SRC_TAG" --filter=blob:none --sparse \
        https://github.com/rust-lang/rust "$tmp/rust"
    git -C "$tmp/rust" sparse-checkout set library
    git -C "$tmp/rust" sparse-checkout add --skip-checks src/version
    git -C "$tmp/rust" submodule update --init --depth 1 library/backtrace
    mkdir -p "$RUST_SRC/src"
    cp -r "$tmp/rust/library" "$RUST_SRC/"
    cp "$tmp/rust/src/version" "$RUST_SRC/src/"
    cp "$tmp/rust/Cargo.lock" "$RUST_SRC/"
    rm -rf "$tmp"
}

build_linux() {
    echo "==> Building $PKG for Linux (static)"
    if have_target x86_64-unknown-linux-musl; then
        RUSTFLAGS="-C target-feature=+crt-static" \
            cargo build --release \
            --target x86_64-unknown-linux-musl \
            --manifest-path "$BIN/Cargo.toml"
        cp target/x86_64-unknown-linux-musl/release/$PKG "$OUT/$PKG-linux-amd64"
    else
        echo "    (musl target not installed; using gnu target with crt-static)"
        RUSTFLAGS="-C target-feature=+crt-static" \
            cargo build --release \
            --target x86_64-unknown-linux-gnu \
            --manifest-path "$BIN/Cargo.toml"
        cp target/x86_64-unknown-linux-gnu/release/$PKG "$OUT/$PKG-linux-amd64"
    fi
    strip "$OUT/$PKG-linux-amd64" 2>/dev/null || true
    echo "    -> $OUT/$PKG-linux-amd64"
}

build_windows() {
    echo "==> Building $PKG for Windows (x86_64 .exe)"
    if ! have x86_64-w64-mingw32-gcc; then
        echo "error: mingw-w64 cross compiler not found (x86_64-w64-mingw32-gcc)." >&2
        echo "       on Debian/Ubuntu: sudo apt install mingw-w64" >&2
        exit 1
    fi
    setup_rust_src
    RUSTC_BOOTSTRAP=1 \
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
    RUSTFLAGS="-C link-self-contained=no" \
        cargo build -Zbuild-std=std,panic_abort,panic_unwind \
        --release --target x86_64-pc-windows-gnu \
        --manifest-path "$BIN/Cargo.toml"
    cp target/x86_64-pc-windows-gnu/release/$PKG.exe "$OUT/$PKG-windows-amd64.exe"
    echo "    -> $OUT/$PKG-windows-amd64.exe"
}

case "${1:-linux}" in
    linux)   build_linux ;;
    windows) build_windows ;;
    all)     build_linux; build_windows ;;
    *) echo "usage: $0 [linux|windows|all]" >&2; exit 1 ;;
esac
echo "Done."
