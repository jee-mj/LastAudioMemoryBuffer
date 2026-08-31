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
        lamb-tests = lamb.overrideAttrs (_final: prev: {
          doCheck = true;
          preCheck = (prev.preCheck or "") + ''
            export XDG_RUNTIME_DIR="$TMPDIR/lamb-xdg-runtime"
            mkdir -p "$XDG_RUNTIME_DIR"
            chmod 0700 "$XDG_RUNTIME_DIR"
          '';
        });
        modulePolicyConfig = nixpkgs.lib.nixosSystem {
          inherit system;
          modules = [
            ./nix/module.nix
            {
              system.stateVersion = "26.05";
              users.groups.lamb-test.gid = 987;
              users.users.lamb-test = {
                isSystemUser = true;
                group = "lamb-test";
                home = "/var/lib/lamb-test";
                uid = 987;
              };
              services.lamb = {
                enable = true;
                user = "lamb-test";
                package = lamb;
              };
            }
          ];
        };
        modulePolicyService = modulePolicyConfig.config.systemd.services.lamb;
      in
      {
        packages.default = lamb;
        packages.lamb = lamb;
        checks = {
          tests = lamb-tests;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          module-policy =
            assert modulePolicyService.serviceConfig.Restart == "on-failure";
            assert modulePolicyService.serviceConfig.RestartPreventExitStatus == [ 78 ];
            assert modulePolicyService.serviceConfig.RestartSec == 5;
            assert modulePolicyService.startLimitIntervalSec == 60;
            assert modulePolicyService.startLimitBurst == 3;
            pkgs.runCommand "lamb-module-policy" { } ''
              wrapper=${modulePolicyService.serviceConfig.ExecStart}
              count="$(${pkgs.gnugrep}/bin/grep -Ec '^[[:blank:]]*exit 78[[:blank:]]*$' "$wrapper")"
              test "$count" -eq 2
              ${pkgs.gnugrep}/bin/grep -Pzq '(?m)^[ \t]*if \[ ! -d "\$XDG_RUNTIME_DIR" \]; then[ \t]*\n[ \t]*echo "lamb: XDG_RUNTIME_DIR does not exist: \$XDG_RUNTIME_DIR" >&2[ \t]*\n[ \t]*exit 78[ \t]*\n[ \t]*fi[ \t]*$' "$wrapper"
              ${pkgs.gnugrep}/bin/grep -Pzq '(?m)^[ \t]*if \[ "\$owner" != "\$uid" \]; then[ \t]*\n[ \t]*echo "lamb: XDG_RUNTIME_DIR owner \$owner does not match uid \$uid" >&2[ \t]*\n[ \t]*exit 78[ \t]*\n[ \t]*fi[ \t]*$' "$wrapper"
              touch "$out"
            '';
        };
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
