#!/bin/sh
# Cargo runner (see .cargo/config.toml): sign the binary with the hypervisor
# entitlement, then run it, so `cargo run` and `cargo test` work for code that
# uses Hypervisor.framework. codesign's output is shown only if it fails.
set -e
out=$("$(dirname "$0")/sign-hv.sh" "$1" 2>&1) || { echo "$out" >&2; exit 1; }
exec "$@"
