{
  description = "LAMB — LastAudioMemoryBuffer rolling audio daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lamb = pkgs.callPackage ./default.nix { };
      in
      {
        packages.default = lamb;
        packages.lamb = lamb;
        devShells.default = pkgs.mkShell {
          inputsFrom = [ lamb ];
          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
            rustfmt
            clippy
            pkg-config
          ];
        };
      }
    ) // {
      nixosModules.default = ./nix/module.nix;
    };
}
