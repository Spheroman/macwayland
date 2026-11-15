#!/bin/sh
# Build the GPU's Mac side (docs/gpu.md): virglrenderer for macOS with
# gpu/virglrenderer-macos.patch, and KosmicKrisp from Mesa main
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
patch=$here/virglrenderer-macos.patch
if [ ! -x "$out/virglrenderer/build/vtest/virgl_test_server" ] || [ "$patch" -nt "$out/virglrenderer/build/vtest/virgl_test_server" ]; then
    if [ ! -d "$out/virglrenderer/.git" ]; then
        rm -rf "$out/virglrenderer"
        git clone -q https://gitlab.freedesktop.org/virgl/virglrenderer.git "$out/virglrenderer"
    fi
    git -C "$out/virglrenderer" checkout -q -f "$rev"
    git -C "$out/virglrenderer" apply "$patch"
    (cd "$out/virglrenderer" && nix-shell "$here/build-shell.nix" --run \
        '[ -d build ] || meson setup build -Dvrend=false -Dvenus=true -Drender-server-mode=process; ninja -C build') >&2
fi
ln -sf ../virglrenderer/build/src/libvirglrenderer.1.dylib "$out/bin/"
ln -sf ../virglrenderer/build/server/virgl_render_server "$out/bin/"
# (GC roots, so a collection doesn't take them while they're in use.)
nix build --out-link "$out/roots/kk-main" --impure -f "$here/kosmickrisp-main.nix" >&2
nix build --out-link "$out/roots/vulkan-loader" nixpkgs#vulkan-loader >&2
# fencebench for Linux (glibc, from nixpkgs' cache: no Linux builder needed)
# and for macOS, to compare (docs/gpu.md, step 2).
# (drawbench too, its shaders compiled to SPIR-V with glslang.)
for src in "$here/drawbench.c" "$here/shaders/draw.vert" "$here/shaders/draw.frag"; do
    [ "$src" -nt "$out/drawbench-linux" ] && rm -f "$out/fencebench-linux"
done
if [ ! -x "$out/fencebench-linux" ] || [ "$here/fencebench.c" -nt "$out/fencebench-linux" ]; then
    nix build --out-link "$out/roots/glslang" nixpkgs#glslang >&2
    for stage in vert frag; do
        "$(readlink "$out/roots/glslang-bin")/bin/glslangValidator" -V --vn "draw_$stage" -o "$out/draw.$stage.h" "$here/shaders/draw.$stage" >&2
    done
    # (Nix names a non-default output's link LINK-OUTPUT: linux-glibc-dev.)
    nix build --max-jobs 0 --out-link "$out/roots/linux-glibc" "nixpkgs#legacyPackages.aarch64-linux.glibc^out,dev" >&2
    nix build --max-jobs 0 --out-link "$out/roots/linux-vulkan-loader" "nixpkgs#legacyPackages.aarch64-linux.vulkan-loader^out" >&2
    nix build --out-link "$out/roots/vulkan-headers" nixpkgs#vulkan-headers >&2
    nix build --out-link "$out/roots/lld" nixpkgs#llvmPackages.lld >&2
    gl=$(readlink "$out/roots/linux-glibc")
    clang --target=aarch64-unknown-linux-gnu -O2 -nostdinc -isystem "$(readlink "$out/roots/linux-glibc-dev")/include" \
        -isystem "$(clang -print-resource-dir)/include" -I "$(readlink "$out/roots/vulkan-headers")/include" \
        -c "$here/fencebench.c" -o "$out/fencebench.o"
    "$(readlink "$out/roots/lld")/bin/ld.lld" -o "$out/fencebench-linux" --dynamic-linker="$gl/lib/ld-linux-aarch64.so.1" \
        -rpath "$gl/lib" -rpath "$(readlink "$out/roots/linux-vulkan-loader")/lib" \
        "$gl/lib/crt1.o" "$gl/lib/crti.o" "$out/fencebench.o" -L"$gl/lib" -L"$(readlink "$out/roots/linux-vulkan-loader")/lib" \
        -lvulkan -lc "$gl/lib/crtn.o"
    clang -O2 -I "$(readlink "$out/roots/vulkan-headers")/include" "$here/fencebench.c" \
        -L "$(readlink "$out/roots/vulkan-loader")/lib" -lvulkan -Wl,-rpath,"$(readlink "$out/roots/vulkan-loader")/lib" \
        -o "$out/fencebench-mac"
    clang --target=aarch64-unknown-linux-gnu -O2 -nostdinc -isystem "$(readlink "$out/roots/linux-glibc-dev")/include" \
        -isystem "$(clang -print-resource-dir)/include" -I "$(readlink "$out/roots/vulkan-headers")/include" -I "$out" \
        -c "$here/drawbench.c" -o "$out/drawbench.o"
    "$(readlink "$out/roots/lld")/bin/ld.lld" -o "$out/drawbench-linux" --dynamic-linker="$gl/lib/ld-linux-aarch64.so.1" \
        -rpath "$gl/lib" -rpath "$(readlink "$out/roots/linux-vulkan-loader")/lib" \
        "$gl/lib/crt1.o" "$gl/lib/crti.o" "$out/drawbench.o" -L"$gl/lib" -L"$(readlink "$out/roots/linux-vulkan-loader")/lib" \
        -lvulkan -lc -lm "$gl/lib/crtn.o"
    clang -O2 -I "$(readlink "$out/roots/vulkan-headers")/include" -I "$out" "$here/drawbench.c" \
        -L "$(readlink "$out/roots/vulkan-loader")/lib" -lvulkan -Wl,-rpath,"$(readlink "$out/roots/vulkan-loader")/lib" \
        -o "$out/drawbench-mac"
fi
kk=$(readlink "$out/roots/kk-main")
loader=$(readlink "$out/roots/vulkan-loader")
echo "export MACWL_GPU=$(cd "$out/bin" && pwd)"
echo "export VK_ICD_FILENAMES=$kk/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json"
echo "export MACWL_GPU_DYLD_LIBRARY_PATH=$loader/lib"
echo "export MESA_KK_EXPERIMENTAL=\${MESA_KK_EXPERIMENTAL-custom_border}"
