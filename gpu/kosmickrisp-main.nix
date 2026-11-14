# KosmicKrisp (and the rest of nixpkgs' macOS Mesa) from Mesa main, for
# geometry shaders and custom border colours (docs/gpu.md). Mesa main dropped
# some of the options nixpkgs passes, and looks for clang's libraries in
# LLVM's libdir only.
let
  pkgs = import (builtins.getFlake "nixpkgs") {};
  lib = pkgs.lib;
  rev = "c9ce420e39bc5d97d3e54ef54ec9603a45772bb2";
in pkgs.mesa.overrideAttrs (o: let
  clangFlag = lib.findFirst (f: lib.hasPrefix "-Dclang-libdir=" f) null o.mesonFlags;
  clangLib = lib.removePrefix "-Dclang-libdir=" clangFlag;
in {
  version = "27.0.0-devel";
  src = pkgs.fetchurl {
    url = "https://gitlab.freedesktop.org/mesa/mesa/-/archive/${rev}/mesa-${rev}.tar.gz";
    hash = "sha256-MJ2nLRjO8UAAOP/0j/RHw9oG271Itw1q8NcKcSh6XGk=";
  };
  patches = [];
  # Options main no longer has.
  mesonFlags = builtins.filter (f: f != clangFlag)
    (map (f: if lib.hasPrefix "-Dvulkan-layers" f then "-Dvulkan-layers=" else f) o.mesonFlags);
  postPatch = (o.postPatch or "") + ''
    substituteInPlace meson.build --replace-quiet "dirs : llvm_libdir, required" "dirs : [llvm_libdir, '${clangLib}'], required"
  '';
})
