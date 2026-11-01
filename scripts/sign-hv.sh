#!/bin/sh
# Ad-hoc sign binaries that use Hypervisor.framework (no developer account needed).
# Usage: scripts/sign-hv.sh target/release/<binary>...
set -e
here=$(dirname "$0")
for bin in "$@"; do
    codesign --force --sign - --entitlements "$here/hypervisor.entitlements" "$bin"
done
