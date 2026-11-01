{
  description = "macwayland: dev tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.aarch64-darwin;
      # Cross toolchain for Linux guest programs (Track B). Hydra builds it for
      # aarch64-darwin, so it comes from cache.nixos.org without a Linux builder.
      guest = pkgs.pkgsCross.aarch64-multiplatform-musl;
    in {
      devShells.aarch64-darwin.default = pkgs.mkShellNoCC {
        packages = [ guest.stdenv.cc ];
      };
    };
}
