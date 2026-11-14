# What building virglrenderer on macOS needs (gpu/vtest.sh).
{ pkgs ? import (builtins.getFlake "nixpkgs") {} }:
pkgs.mkShell {
  packages = with pkgs; [ meson ninja pkg-config vulkan-headers vulkan-loader (python3.withPackages (p: [ p.mako p.pyyaml ])) ];
}
