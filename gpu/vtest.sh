#!/bin/sh
# Venus over vtest (docs/gpu.md, step 1): build virglrenderer for macOS
# with gpu/virglrenderer-macos-vtest.patch and KosmicKrisp from Mesa main
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
# GALLIUM_DRIVER=zink for EGL's surfaceless platform). KosmicKrisp has no
# VK_EXT_transform_feedback, so Zink says GL 2.1 and GLES 2.0 unless
# MESA_GL_VERSION_OVERRIDE / MESA_GLES_VERSION_OVERRIDE say more.
set -eu
socket=${1:?usage: gpu/vtest.sh SOCKET}
here=$(cd "$(dirname "$0")" && pwd)
out=$here/../target/gpu
rev=01637fc   # virglrenderer main, 2026-11-11
mkdir -p "$out"
if [ ! -x "$out/virglrenderer/build/vtest/virgl_test_server" ]; then
    rm -rf "$out/virglrenderer"
    git clone -q https://gitlab.freedesktop.org/virgl/virglrenderer.git "$out/virglrenderer"
    git -C "$out/virglrenderer" checkout -q "$rev"
    git -C "$out/virglrenderer" apply "$here/virglrenderer-macos-vtest.patch"
    (cd "$out/virglrenderer" && nix-shell "$here/build-shell.nix" --run \
        'meson setup build -Dvrend=false -Dvenus=true -Drender-server-mode=process && ninja -C build')
fi
mesa=$(nix build --no-link --print-out-paths --impure -f "$here/kosmickrisp-main.nix" | grep -v -- '-dev$')
loader=$(nix build --no-link --print-out-paths nixpkgs#vulkan-loader)
export RENDER_SERVER_EXEC_PATH=$out/virglrenderer/build/server/virgl_render_server
export VK_ICD_FILENAMES=$mesa/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json
export DYLD_LIBRARY_PATH=$loader/lib:$out/virglrenderer/build/src
# Zink wants custom border colours, an experimental feature of KosmicKrisp's.
export MESA_KK_EXPERIMENTAL=${MESA_KK_EXPERIMENTAL-custom_border}
rm -f "$socket"
exec "$out/virglrenderer/build/vtest/virgl_test_server" --venus --no-virgl --socket-path "$socket"
