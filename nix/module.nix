# modules/audio/lamb.nix — LAMB daemon service
{ config, lib, pkgs, ... }:
let
  cfg = config.services.lamb;
  lambPkg = cfg.package;
  userHome = config.users.users.${cfg.user}.home;
  userUid = config.users.users.${cfg.user}.uid;
  configPath = lib.replaceStrings [ "%h" ] [ userHome ] cfg.configPath;
  controlSocketShellDefault = lib.replaceStrings [ "%t" ] [ "$XDG_RUNTIME_DIR" ] cfg.control.socketPath;
  wrapper = pkgs.writeShellScript "lamb-daemon-wrapper" ''
    set -euo pipefail
    uid="$(${pkgs.coreutils}/bin/id -u ${lib.escapeShellArg cfg.user})"
    export XDG_RUNTIME_DIR="''${XDG_RUNTIME_DIR:-/run/user/$uid}"
    if [ ! -d "$XDG_RUNTIME_DIR" ]; then
      echo "lamb: XDG_RUNTIME_DIR does not exist: $XDG_RUNTIME_DIR" >&2
      exit 1
    fi
    owner="$(${pkgs.coreutils}/bin/stat -c %u "$XDG_RUNTIME_DIR")"
    if [ "$owner" != "$uid" ]; then
      echo "lamb: XDG_RUNTIME_DIR owner $owner does not match uid $uid" >&2
      exit 1
    fi
    export LD_LIBRARY_PATH=${pkgs.pipewire.jack}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
    exec ${lambPkg}/bin/lamb daemon --config ${lib.escapeShellArg configPath}
  '';
in
{
  options.services.lamb = {
    enable = lib.mkEnableOption "LAMB rolling audio daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.lamb or (pkgs.callPackage ../default.nix { });
      description = "LAMB package to run.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      description = "User account that runs the LAMB daemon.";
    };

    configPath = lib.mkOption {
      type = lib.types.str;
      default = "%h/.config/lamb/lamb.toml";
      description = "Path to the LAMB config file. Supports systemd %h expansion.";
    };

    control.socketPath = lib.mkOption {
      type = lib.types.str;
      default = "%t/lamb/control.sock";
      description = "Path to the LAMB control socket. Supports systemd %t expansion.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [
      lambPkg
      (pkgs.writeShellScriptBin "lamb-recall" ''
        exec ${lambPkg}/bin/lamb recall --socket "''${LAMB_CONTROL_SOCKET:-${controlSocketShellDefault}}"
      '')
      (pkgs.writeShellScriptBin "lamb-clear" ''
        exec ${lambPkg}/bin/lamb clear --socket "''${LAMB_CONTROL_SOCKET:-${controlSocketShellDefault}}"
      '')
      (pkgs.writeShellScriptBin "lamb-status" ''
        exec ${lambPkg}/bin/lamb status --socket "''${LAMB_CONTROL_SOCKET:-${controlSocketShellDefault}}" "$@"
      '')
      (pkgs.writeShellScriptBin "lamb-stop" ''
        exec ${lambPkg}/bin/lamb stop --socket "''${LAMB_CONTROL_SOCKET:-${controlSocketShellDefault}}"
      '')
      (pkgs.writeShellScriptBin "lamb-dump" ''
        exec ${lambPkg}/bin/lamb dump --socket "''${LAMB_CONTROL_SOCKET:-${controlSocketShellDefault}}"
      '')
      (pkgs.writeShellScriptBin "lamb-start-capture" ''
        exec ${lambPkg}/bin/lamb start-capture --socket "''${LAMB_CONTROL_SOCKET:-${controlSocketShellDefault}}" "$@"
      '')
      (pkgs.writeShellScriptBin "lamb-stop-capture" ''
        exec ${lambPkg}/bin/lamb stop-capture --socket "''${LAMB_CONTROL_SOCKET:-${controlSocketShellDefault}}"
      '')
      (pkgs.writeShellScriptBin "lamb-reload" ''
        exec ${lambPkg}/bin/lamb reload --socket "''${LAMB_CONTROL_SOCKET:-${controlSocketShellDefault}}"
      '')
    ];

    systemd.services.lamb = {
      description = "LAMB — LastAudioMemoryBuffer daemon";
      documentation = [ "https://github.com/jee-mj/LastAudioMemoryBuffer" ];
      after = [ "user@${toString userUid}.service" ];
      wants = [ "user@${toString userUid}.service" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        ExecStart = wrapper;
        Restart = "on-failure";
        RestartSec = 5;
        LimitRTPRIO = "95";
        LimitMEMLOCK = "512M";
        Nice = -15;
        ReadWritePaths = [ userHome ];
        ProtectSystem = "strict";
        NoNewPrivileges = true;
        TimeoutStopSec = 30;
      };
    };
  };
}
