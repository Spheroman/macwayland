#!/bin/sh
# Build the GPU's Mac side (docs/gpu.md): virglrenderer for macOS with
# gpu/virglrenderer-macos-vtest.patch, and KosmicKrisp from Mesa main
# (gpu/kosmickrisp-main.nix). Prints the environment macwl-run needs for
# the render node (eval it, or see gpu/run.sh):
#   MACWL_GPU                    libvirglrenderer.1.dylib and virgl_render_server
#   VK_ICD_FILENAMES             KosmicKrisp, for the render server
#   MACWL_GPU_DYLD_LIBRARY_PATH  the Vulkan loader, for the render server
#   MESA_KK_EXPERIMENTAL         custom border colours (Zink wants them)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$here/../target/gpu
rev=01637fc   # virglrenderer main, 2026-11-11
mkdir -p "$out/bin" "$out/roots"
if [ ! -x "$out/virglrenderer/build/vtest/virgl_test_server" ]; then
    rm -rf "$out/virglrenderer"
    git clone -q https://gitlab.freedesktop.org/virgl/virglrenderer.git "$out/virglrenderer"
    git -C "$out/virglrenderer" checkout -q "$rev"
    git -C "$out/virglrenderer" apply "$here/virglrenderer-macos-vtest.patch"
    (cd "$out/virglrenderer" && nix-shell "$here/build-shell.nix" --run \
        'meson setup build -Dvrend=false -Dvenus=true -Drender-server-mode=process && ninja -C build') >&2
fi
ln -sf ../virglrenderer/build/src/libvirglrenderer.1.dylib "$out/bin/"
ln -sf ../virglrenderer/build/server/virgl_render_server "$out/bin/"
# (GC roots, so a collection doesn't take them while they're in use.)
nix build --out-link "$out/roots/kk-main" --impure -f "$here/kosmickrisp-main.nix" >&2
nix build --out-link "$out/roots/vulkan-loader" nixpkgs#vulkan-loader >&2
kk=$(readlink "$out/roots/kk-main")
loader=$(readlink "$out/roots/vulkan-loader")
echo "export MACWL_GPU=$(cd "$out/bin" && pwd)"
echo "export VK_ICD_FILENAMES=$kk/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json"
echo "export MACWL_GPU_DYLD_LIBRARY_PATH=$loader/lib"
echo "export MESA_KK_EXPERIMENTAL=\${MESA_KK_EXPERIMENTAL-custom_border}"
