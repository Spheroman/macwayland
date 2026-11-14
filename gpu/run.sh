#!/bin/sh
# Run a Linux program under macwl-run with the GPU (docs/gpu.md, step 2):
# /dev/dri/renderD128, Venus's virtio-gpu node, for Mesa's Venus driver
# (and Zink on it). The guest's Mesa is nixpkgs' for aarch64-linux unless
# MACWL_ENV_VK_ICD_FILENAMES says otherwise.
#
#   gpu/run.sh vulkaninfo --summary
set -eu
here=$(cd "$(dirname "$0")" && pwd)
eval "$("$here/build.sh")"
if [ -z "${MACWL_ENV_VK_ICD_FILENAMES:-}" ]; then
    nix build --max-jobs 0 --out-link "$here/../target/gpu/roots/linux-mesa" nixpkgs#legacyPackages.aarch64-linux.mesa.out >&2
    export MACWL_ENV_VK_ICD_FILENAMES=$(readlink "$here/../target/gpu/roots/linux-mesa")/share/vulkan/icd.d/virtio_icd.aarch64.json
fi
exec "$here/../target/debug/macwl-run" "$@"
