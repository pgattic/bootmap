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
    layout = cfg.layout;
    radial = cfg.radialPull;
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
        Units that need the display (login screens). Before any of them starts,
        bootmap-quit.service makes bootmap release the display (restoring the
        console and dropping DRM master). bootmap then keeps recording off-screen
        until the boot finishes. Set to `[ ]` to keep drawing instead (see `holdSeconds`).
      '';
    };

    layout = mkOption {
      type = types.enum [
        "force"
        "radial"
      ];
      default = "force";
      description = ''
        `force`: a physics-based, Obsidian-like graph where units float and
        settle, with a soft pull toward their dependency depth.
        `radial`: a rigid radial tree where distance from the center is exactly dependency depth.
      '';
    };

    radialPull = mkOption {
      type = types.numbers.between 0 1;
      default = 0.5;
      description = "Force layout only: how strongly units are pulled toward their dependency-depth ring (0 = free, 1 = close to rings).";
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
      # Without default dependencies we must opt in to being stopped at shutdown.
      conflicts = [ "shutdown.target" ];
      before = [ "shutdown.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package} boot ${args}";
        Restart = "no";
        TimeoutStopSec = "3s";
        RuntimeDirectory = "bootmap";
        # Never get in the way of the boot.
        Nice = 5;
        OOMScoreAdjust = 500;
      };
    };

    # Handover, like plymouth-quit: ordered before the login screen, this asks
    # bootmap to give the display back (SIGUSR1) and waits until it has. bootmap
    # keeps recording off-screen until the boot finishes.
    #
    # Deliberately not Conflicts= between bootmap and the greeter: both start
    # jobs land in the same boot transaction, and systemd resolves the conflict
    # by silently dropping the greeter's job.
    systemd.services.bootmap-quit = mkIf (cfg.handoverUnits != [ ]) {
      description = "Hand the display from bootmap to the login screen";
      wantedBy = [ "multi-user.target" ];
      after = [ "bootmap.service" ];
      before = cfg.handoverUnits;
      path = [ config.systemd.package pkgs.coreutils ];
      script = ''
        systemctl is-active --quiet bootmap.service || exit 0
        systemctl kill --signal=SIGUSR1 --kill-whom=main bootmap.service
        for _ in $(seq 50); do
          [ -e /run/bootmap/released ] && exit 0
          sleep 0.1
        done
        echo "bootmap did not release the display in time; stopping it"
        systemctl stop bootmap.service
      '';
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
    };

    environment.systemPackages = [ cfg.package ];
  };
}
