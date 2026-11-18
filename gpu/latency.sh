#!/bin/sh
# Key → glass latency of a guest GPU client (docs/gpu.md, step 4):
# gpu/keyframe.c (Vulkan, through the render node) in macwl-shell, full
# screen, with the shell typing N keys itself (MACWL_AUTO_KEYS) and timing
# each to its answer's presentation (MTLDrawable.presentedTime). Takes over
# the screen while it runs; Esc quits.
#
#   gpu/latency.sh [N] [fifo|mailbox]
#
# Native baseline, measured the same way: native-probe metal --no-capture N.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$here/..
n=${1:-40}
mode=${2:-fifo}
cargo build -q --release -p macwl-shell -p macwl-runtime
"$root/scripts/sign-hv.sh" "$root/target/release/macwl-run" >/dev/null 2>&1
gpu_env=$("$here/build.sh")
eval "$gpu_env"
[ -e "$root/target/gpu/roots/linux-mesa" ] || nix build --max-jobs 0 --out-link "$root/target/gpu/roots/linux-mesa" nixpkgs#legacyPackages.aarch64-linux.mesa.out >&2
export MACWL_ENV_VK_ICD_FILENAMES=$(readlink "$root/target/gpu/roots/linux-mesa")/share/vulkan/icd.d/virtio_icd.aarch64.json
runtime=/tmp/macwl-latency-$(id -u)
rm -rf "$runtime"
mkdir -p "$runtime"
chmod 700 "$runtime"
MACWL_AUTO_KEYS=$n XDG_RUNTIME_DIR=$runtime "$root/target/release/macwl-shell" &
shell=$!
trap 'kill $shell $client 2>/dev/null' EXIT
client=
while [ ! -S "$runtime/wayland-mac" ]; do
    kill -0 $shell 2>/dev/null || exit 1
    sleep 0.1
done
XDG_RUNTIME_DIR=$runtime WAYLAND_DISPLAY=wayland-mac "$root/target/release/macwl-run" "$root/target/gpu/keyframe-linux" "$mode" &
client=$!
wait $shell
