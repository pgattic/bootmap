self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.bootmap;
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    ;
  args = lib.cli.toCommandLineShellGNU { } {
    record = cfg.recordPath;
    hold = cfg.holdSeconds;
    devices = cfg.showDevices;
    no-processes = !cfg.showProcesses;
  };
in
{
  options.services.bootmap = {
    enable = mkEnableOption "bootmap, a graphical radial boot visualizer drawn on the display during boot";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.bootmap-minimal;
      defaultText = lib.literalExpression "bootmap.packages.\${system}.bootmap-minimal";
      description = "bootmap package used at boot (the build without windowing libraries is enough).";
    };

    holdSeconds = mkOption {
      type = types.numbers.nonnegative;
      default = 0;
      description = ''
        How long to keep the finished graph on screen after boot completes.
        0 means until a unit listed in `handoverUnits` starts (which stops bootmap).
      '';
    };

    handoverUnits = mkOption {
      type = types.listOf types.str;
      default = [
        "display-manager.service"
        "getty@tty1.service"
        "autovt@tty1.service"
      ];
      description = ''
        Units that need the display. Starting any of them stops bootmap first,
        which restores the console and releases DRM master.
      '';
    };

    showDevices = mkOption {
      type = types.bool;
      default = false;
      description = "Draw *.device units too (there are many).";
    };

    showProcesses = mkOption {
      type = types.bool;
      default = true;
      description = "Draw the processes of running units as satellites.";
    };

    recordPath = mkOption {
      type = types.str;
      default = "/var/log/bootmap/last.json";
      description = "Where the recorded boot timeline is saved (view it later with `bootmap view <file>`).";
    };

    quietKernel = mkOption {
      type = types.bool;
      default = true;
      description = "Add `quiet loglevel=3` so kernel messages do not scribble over the console before bootmap starts.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = !config.boot.plymouth.enable;
        message = "services.bootmap and boot.plymouth both want the display during boot; disable one of them.";
      }
    ];

    boot.kernelParams = lib.optionals cfg.quietKernel [
      "quiet"
      "loglevel=3"
      "udev.log_level=3"
    ];

    systemd.services.bootmap = {
      description = "Graphical boot visualizer";
      # Start as early as possible in stage 2. bootmap talks to PID 1 over its
      # private socket, so it does not need dbus, and it waits for /dev/dri
      # itself, so it does not need udev either.
      unitConfig.DefaultDependencies = false;
      wantedBy = [ "sysinit.target" ];
      # Conflicts= is symmetric: starting one of these stops bootmap. When a
      # stop and a start are queued together, systemd runs the stop first.
      conflicts = cfg.handoverUnits;
      before = cfg.handoverUnits ++ [ "shutdown.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package} boot ${args}";
        Restart = "no";
        TimeoutStopSec = "3s";
        # Never get in the way of the boot.
        Nice = 5;
        OOMScoreAdjust = 500;
      };
    };

    environment.systemPackages = [ cfg.package ];
  };
}
