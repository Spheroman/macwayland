#!/bin/sh
# Build the Linux guest test programs (crates/runtime/guest/*.c, and
# hello-client) as static aarch64-linux-musl binaries in target/guest/, plus
# dynamically linked builds of hello.c and musl's dynamic linker, using
# the cross toolchain from the flake's dev shell. Needs the Rust target:
# rustup target add aarch64-unknown-linux-musl
set -e
root=$(cd "$(dirname "$0")/.." && pwd)
if ! command -v aarch64-unknown-linux-musl-gcc >/dev/null; then
    exec nix develop "$root" -c "$0" "$@"
fi
mkdir -p "$root/target/guest"
for src in "$root"/crates/runtime/guest/*.c; do
    name=$(basename "$src" .c)
    aarch64-unknown-linux-musl-gcc -static -O2 -Wall -o "$root/target/guest/$name" "$src"
    echo "target/guest/$name"
done

# Dynamically linked: hello-dyn names musl's loader by its /nix/store path, as
# Nix-built programs do; hello-distro names /lib/ld-musl-aarch64.so.1, as a
# distribution's would (the tests give it a root with the loader there).
aarch64-unknown-linux-musl-gcc -O2 -Wall -o "$root/target/guest/hello-dyn" "$root/crates/runtime/guest/hello.c"
aarch64-unknown-linux-musl-gcc -O2 -Wall -Wl,--dynamic-linker=/lib/ld-musl-aarch64.so.1 \
    -o "$root/target/guest/hello-distro" "$root/crates/runtime/guest/hello.c"
install -m 755 "$(aarch64-unknown-linux-musl-gcc -print-file-name=libc.so)" "$root/target/guest/ld-musl-aarch64.so.1"
echo "target/guest/hello-dyn target/guest/hello-distro target/guest/ld-musl-aarch64.so.1"

# The Rust Wayland client, as a Linux binary (B2). The old copy goes first,
# so that a build that fails leaves none for the tests to run instead.
rm -f "$root/target/guest/hello-client"
if ! rustup target list --installed 2>/dev/null | grep -qx aarch64-unknown-linux-musl; then
    echo "build-guest.sh: no Rust standard library for aarch64-unknown-linux-musl" >&2
    echo "  (a toolchain update drops it): rustup target add aarch64-unknown-linux-musl" >&2
    exit 1
fi
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-unknown-linux-musl-gcc \
    cargo build --quiet --release --manifest-path "$root/Cargo.toml" -p hello-client --target aarch64-unknown-linux-musl
cp "$root/target/aarch64-unknown-linux-musl/release/hello-client" "$root/target/guest/hello-client"
echo "target/guest/hello-client"
