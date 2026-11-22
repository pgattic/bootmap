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
  settingsFormat = pkgs.formats.toml { };

  # Paths given as Nix paths (./logo.png) or derivations are copied to the
  # store by the interpolation, so the file names store paths. Unset keys
  # are left out.
  settings = lib.filterAttrs (_: v: v != null) (
    cfg.settings
    // lib.optionalAttrs (cfg.settings.font != null) { font = "${cfg.settings.font}"; }
    // lib.optionalAttrs (cfg.settings.center-image != null) {
      center-image = "${cfg.settings.center-image}";
    }
  );
  hasSettings = settings != { };
  uncheckedConfigFile = settingsFormat.generate "bootmap-unchecked.toml" settings;
  canRunPackage = pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform;
  # A mistake in the settings fails the build here, not the boot.
  configFile = pkgs.runCommand "bootmap.toml" { } ''
    ${lib.optionalString canRunPackage "${lib.getExe cfg.package} check-config ${uncheckedConfigFile}"}
    cp ${uncheckedConfigFile} $out
  '';
  # Font and image are read at boot, so the initrd needs them too.
  assetPaths = lib.filter (p: p != null) [
    (settings.font or null)
    (settings.center-image or null)
  ];

  args = lib.cli.toCommandLineShellGNU { } {
    record = cfg.recordPath;
    hold = cfg.holdSeconds;
    devices = cfg.showDevices;
    radial = cfg.radialPull;
    no-processes = !cfg.showProcesses;
  };

  # The same unit, by the same name, runs in the initrd and in the real system:
  # that is how systemd keeps tracking the process across the switch to the
  # real root. The "@" in ExecStart makes argv[0] start with "@", which keeps
  # switch-root from killing it; IgnoreOnIsolate keeps initrd-switch-root.target
  # from stopping it.
  bootmapService = {
    description = "Graphical boot visualizer";
    # Start as early as possible. bootmap talks to PID 1 over its private
    # socket, so it does not need dbus, and it waits for /dev/dri itself, so
    # it does not need udev either.
    unitConfig = {
      DefaultDependencies = false;
      IgnoreOnIsolate = true;
    };
    wantedBy = [ "sysinit.target" ];
    # Without default dependencies we must opt in to being stopped at
    # shutdown, and to not hiding an emergency shell behind the graph.
    conflicts = [
      "shutdown.target"
      "emergency.service"
      "rescue.service"
    ];
    before = [
      "shutdown.target"
      "emergency.service"
      "rescue.service"
    ];
    serviceConfig = {
      # Ready means: has the display (and with it the passphrase prompt), or
      # has stopped expecting one in time. The console password agent is
      # ordered after this.
      Type = "notify";
      ExecStart = "@${lib.getExe cfg.bootPackage} @bootmap boot ${args}";
      Restart = "no";
      TimeoutStopSec = "3s";
      RuntimeDirectory = "bootmap";
      # Never get in the way of the boot.
      Nice = 5;
      OOMScoreAdjust = 500;
    };
  };

  consoleAgentPath = {
    after = [ "bootmap.service" ];
    unitConfig.ConditionPathExists = "!/run/bootmap/agent";
  };
in
{
  imports = [
    (lib.mkRemovedOptionModule [ "services" "bootmap" "layout" ] "bootmap now only has the force layout; services.bootmap.radialPull = 1 keeps units close to their dependency depth.")
  ];

  options.services.bootmap = {
    enable = mkEnableOption "bootmap, a graphical boot visualizer drawn on the display during boot";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.bootmap;
      defaultText = lib.literalExpression "bootmap.packages.\${system}.bootmap";
      description = ''
        bootmap build to install and to run at boot. Installed for users:
        `bootmap replay` replays this boot, `bootmap view` watches systemd
        live.
      '';
    };

    bootPackage = mkOption {
      type = types.package;
      default = cfg.package;
      defaultText = lib.literalExpression "config.services.bootmap.package";
      example = lib.literalExpression "bootmap.packages.\${system}.bootmap-minimal";
      description = ''
        bootmap build the boot-time services run, and with `initrd` the one
        copied into every generation's initrd. `bootmap-minimal` has only
        `bootmap boot`, which is all they need: it makes each initrd about
        1 MiB smaller (compressed). Without `initrd` the choice does not
        affect /boot.
      '';
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

    radialPull = mkOption {
      type = types.numbers.between 0 1;
      default = 0.5;
      description = "How strongly units are pulled toward their depth in the tree: 0 lets the graph float freely, 1 keeps each unit on the ring of its depth.";
    };

    showDevices = mkOption {
      type = types.bool;
      default = false;
      description = "Draw *.device units too (there are many).";
    };

    showShutdown = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Also show the graph while shutting down or rebooting: units fade out as
        they stop, and the ones holding up the shutdown are listed.
      '';
    };

    showProcesses = mkOption {
      type = types.bool;
      default = true;
      description = "Draw the processes of running units as satellites.";
    };

    recordPath = mkOption {
      type = types.str;
      default = "/var/log/bootmap/last.json";
      description = "Where the recorded boot timeline is saved (replay it with `bootmap replay <file>`).";
    };

    initrd = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Start bootmap in the systemd initrd and follow the boot across the
        switch to the real root with the same process, so the graph covers the
        whole boot. Needs a systemd initrd, which current NixOS always uses (on
        older releases, set `boot.initrd.systemd.enable`). Passphrase requests,
        such as unlocking an encrypted disk, are shown over the graph and
        answered from the keyboard.
      '';
    };

    settings = mkOption {
      type = types.submodule {
        freeformType = settingsFormat.type;
        options = {
          font = mkOption {
            type = types.nullOr types.path;
            default = null;
            example = lib.literalExpression ''"''${pkgs.noto-fonts}/share/fonts/noto/NotoSans[wdth,wght].ttf"'';
            description = ''
              TrueType or OpenType font file for all text. Copied to the Nix
              store. null uses the bundled DejaVu Sans.
            '';
          };
          center-image = mkOption {
            type = types.nullOr types.path;
            default = null;
            example = lib.literalExpression "./logo.png";
            description = ''
              PNG drawn in the middle of the graph, scaled to fit. Copied to
              the Nix store. null draws the plain center dot.
            '';
          };
        };
      };
      default = { };
      example = lib.literalExpression ''
        {
          theme = "black";
          title = "Welcome";
          text-scale = 1.2;
          center-image = ./logo.png;
          colors.active = "#7ebae4";
          show.legend = false;
        }
      '';
      description = ''
        How the graph looks: theme, colors, text size, font, a title, a
        picture in the middle, and which panels are shown. Written as TOML to
        {file}`/etc/bootmap.toml`, which `bootmap boot`, `bootmap view`,
        `bootmap replay` and `bootmap render` read, and with `initrd` also to the initrd. Keys are
        written as in the file (`text-scale`, `center-image`, `[colors]`,
        `[show]`); see "Appearance" in docs/usage.md for all of them. The
        build fails if `bootmap check-config` rejects the result, for example
        on an unknown key or a bad color. Empty (the default) keeps the
        built-in look and installs no file.
      '';
    };

    configFile = mkOption {
      type = types.path;
      internal = true;
      readOnly = true;
      default = configFile;
      description = "The checked bootmap.toml generated from `settings`.";
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
      {
        # Newer nixpkgs removed the scripted initrd and with it this option
        # (reading it throws): the initrd is then always systemd.
        assertion =
          !cfg.initrd
          || (
            let
              r = builtins.tryEval config.boot.initrd.systemd.enable;
            in
            !r.success || r.value
          );
        message = "services.bootmap.initrd needs a systemd initrd (boot.initrd.systemd.enable = true).";
      }
    ];

    boot.kernelParams = lib.optionals cfg.quietKernel [
      "quiet"
      "loglevel=3"
      "udev.log_level=3"
    ];

    systemd.services.bootmap = bootmapService;

    # While bootmap has the screen and keyboard it answers passphrase requests
    # itself and keeps /run/bootmap/agent in place. systemd's console agent
    # would read from the same terminal and take keystrokes meant for
    # bootmap's prompt, so it is left out for that boot stage, the same way it
    # is for Plymouth: its path unit is ordered after bootmap.service, which
    # reports ready once it has the display or has given up waiting, and is
    # skipped if the marker is there. Without the marker (no display) the
    # console agent works as usual.
    systemd.paths.systemd-ask-password-console = consoleAgentPath;

    boot.initrd.systemd = mkIf cfg.initrd {
      storePaths = [ (lib.getExe cfg.bootPackage) ] ++ assetPaths;
      contents."/etc/bootmap.toml" = mkIf hasSettings { source = configFile; };
      services.bootmap = bootmapService;
      paths.systemd-ask-password-console = consoleAgentPath;
    };

    # Shutdown view. Pulled in by shutdown.target and, having no default
    # dependencies or conflicts, it runs until the final killing spree. It takes
    # the display back by switching to its own VT, which makes a running
    # compositor drop DRM master.
    systemd.services.bootmap-shutdown = mkIf cfg.showShutdown {
      description = "Graphical shutdown visualizer";
      unitConfig.DefaultDependencies = false;
      wantedBy = [ "shutdown.target" ];
      # Lets the boot instance (stopped by shutdown.target) release the display first.
      after = [ "bootmap.service" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${lib.getExe cfg.bootPackage} boot --shutdown ${args}";
        Restart = "no";
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

    environment.etc."bootmap.toml" = mkIf hasSettings { source = configFile; };

    environment.systemPackages = [ cfg.package ];
  };
}
