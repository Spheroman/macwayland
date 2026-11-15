#!/bin/sh
# Venus over vtest (docs/gpu.md, step 1): build virglrenderer for macOS
# with gpu/virglrenderer-macos.patch and KosmicKrisp from Mesa main
# (gpu/kosmickrisp-main.nix), then run the vtest server on SOCKET.
#
#   gpu/vtest.sh SOCKET
#
# A Linux program under macwl-run uses it with Mesa's Venus driver:
#
#   MACWL_ENV_VN_DEBUG=vtest MACWL_ENV_VTEST_SOCKET_NAME=SOCKET \
#   MACWL_ENV_VK_ICD_FILENAMES=<linux mesa>/share/vulkan/icd.d/virtio_icd.aarch64.json \
#   macwl-run vulkaninfo
#
# Zink on top: also MACWL_ENV_MESA_LOADER_DRIVER_OVERRIDE=zink (and
# GALLIUM_DRIVER=zink for EGL's surfaceless platform): GL 3.3 and GLES 3.1
# with gpu/kosmickrisp-transform-feedback.patch.
set -eu
socket=${1:?usage: gpu/vtest.sh SOCKET}
here=$(cd "$(dirname "$0")" && pwd)
eval "$("$here/build.sh")"
out=$here/../target/gpu
export RENDER_SERVER_EXEC_PATH=$out/virglrenderer/build/server/virgl_render_server
export DYLD_LIBRARY_PATH=$MACWL_GPU_DYLD_LIBRARY_PATH:$out/virglrenderer/build/src
rm -f "$socket"
exec "$out/virglrenderer/build/vtest/virgl_test_server" --venus --no-virgl --socket-path "$socket"
