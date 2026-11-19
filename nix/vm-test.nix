{ pkgs, self }:

let
  # A small LUKS volume, attached as an extra disk and unlocked in the initrd.
  secret = rec {
    passphrase = "sample passphrase";
    uuid = "5f0a3c1e-7d42-4b8e-9c61-2a9d1e4f7b30";
    image = pkgs.runCommand "bootmap-luks-disk" { nativeBuildInputs = [ pkgs.cryptsetup ]; } ''
      truncate -s 32M disk.img
      echo -n '${passphrase}' | cryptsetup luksFormat -q --disable-locks --type luks2 \
        --pbkdf pbkdf2 --pbkdf-force-iterations 1000 --uuid ${uuid} disk.img -
      mv disk.img $out
    '';
  };

  # A round logo for the center of the graph, built here to stay out of the repo.
  logo =
    pkgs.runCommand "logo.png" { nativeBuildInputs = [ pkgs.imagemagick ]; }
      "magick -size 256x256 xc:none -fill '#7ebae4' -draw 'circle 128,128 128,8' $out";
in
pkgs.testers.runNixOSTest {
  name = "bootmap";

  nodes = {
    # No login screen to hand over to: hold the finished graph for a while so
    # the screenshot can catch it, then exit on our own. A second graphics
    # card stands in for a second monitor.
    held =
      { pkgs, ... }:
      {
        imports = [ self.nixosModules.default ];
        services.bootmap.enable = true;
        services.bootmap.holdSeconds = 8;
        services.bootmap.handoverUnits = [ ];
        # A configured look: the screenshots show it, and the checks below
        # make sure bootmap accepted the file.
        services.bootmap.settings = {
          theme = "black";
          title = "Test boot";
          text-scale = 1.3;
          center-image = logo;
          colors.active = "#ff00ff";
          show.legend = false;
        };
        environment.systemPackages = [ pkgs.jq ];
        virtualisation.qemu.options = [ "-device virtio-gpu-pci,id=second,xres=1024,yres=768" ];
      };

    # An encrypted volume to unlock in the initrd: bootmap must show the
    # passphrase prompt and answer it from the keyboard.
    luks =
      { lib, ... }:
      {
        imports = [ self.nixosModules.default ];
        services.bootmap = {
          enable = true;
          initrd = true;
          holdSeconds = 3;
          handoverUnits = [ ];
        };
        boot.initrd.luks.devices = lib.mkVMOverride {
          secret.device = "/dev/disk/by-uuid/${secret.uuid}";
        };
        virtualisation.qemu.drives = [
          {
            name = "secret";
            file = "${secret.image}";
            driveExtraOpts = {
              format = "raw";
              snapshot = "on";
            };
          }
        ];
        boot.initrd.systemd.services.bootmap.serviceConfig.StandardError = "journal+console";
      };

    # Started in the systemd initrd: the same process must follow the boot
    # across the switch to the real root, on the same display.
    initrd =
      { pkgs, ... }:
      {
        imports = [ self.nixosModules.default ];
        services.bootmap = {
          enable = true;
          initrd = true;
          holdSeconds = 8;
          handoverUnits = [ ];
          # The file and the image must be in the initrd, where bootmap starts.
          settings = {
            title = "Initrd test";
            center-image = logo;
            colors.active = "#ff00ff";
          };
        };
        environment.systemPackages = [ pkgs.jq ];
        # Keeps the initrd stage long enough to take a screenshot of it.
        boot.initrd.systemd.extraBin.sleep = "${pkgs.coreutils}/bin/sleep";
        boot.initrd.systemd.services.slow-initrd = {
          wantedBy = [ "initrd.target" ];
          before = [ "initrd.target" ];
          serviceConfig = {
            Type = "oneshot";
            ExecStart = "/bin/sleep 5";
          };
        };
        # So the test can see bootmap's messages on the serial console.
        boot.initrd.systemd.services.bootmap.serviceConfig.StandardError = "journal+console";
      };

    # Like a legacy-BIOS laptop: no display at all in the initrd (no firmware
    # framebuffer, GPU driver not in the initrd), so the display only appears
    # once the real system loads the driver. bootmap must follow the switch to
    # the real root without one, then start drawing.
    latedisplay =
      { lib, pkgs, ... }:
      {
        imports = [ self.nixosModules.default ];
        services.bootmap = {
          enable = true;
          initrd = true;
          holdSeconds = 8;
          handoverUnits = [ ];
          # Also covers the boot-only build, in the initrd.
          bootPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.bootmap-minimal;
        };
        boot.initrd.kernelModules = lib.mkForce [
          "virtio_balloon"
          "virtio_console"
          "virtio_rng"
        ];
        environment.systemPackages = [ pkgs.jq ];
      };

    # A real graphical greeter (a Wayland compositor holding DRM master) with
    # the default handover:
    # - the greeter must start (it used to be dropped from the boot
    #   transaction), and only after bootmap has released the display;
    # - the recording must still be complete;
    # - at shutdown, bootmap must take the display back from the compositor.
    greeter =
      { pkgs, lib, ... }:
      {
        imports = [ self.nixosModules.default ];
        services.bootmap.enable = true;
        # Started in the initrd, so the handover and the shutdown view are
        # exercised with a process that survived switch-root.
        services.bootmap.initrd = true;
        hardware.graphics.enable = true;
        fonts.packages = [ pkgs.dejavu_fonts ];
        services.greetd = {
          enable = true;
          settings.default_session.command = "${pkgs.writeShellScript "greeter" ''
            export WLR_RENDERER=pixman
            # The greeter user has no login shell, so give foot one to run.
            exec ${lib.getExe pkgs.cage} -s -- ${lib.getExe pkgs.foot} ${pkgs.bashInteractive}/bin/bash >/tmp/greeter.log 2>&1
          ''}";
        };
        # Holds the shutdown up long enough to look at it.
        systemd.services.slow-stop = {
          wantedBy = [ "multi-user.target" ];
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
            ExecStart = "${pkgs.coreutils}/bin/true";
            ExecStop = "${pkgs.coreutils}/bin/sleep 6";
          };
        };
        environment.systemPackages = [ pkgs.jq ];
      };
  };

  testScript = ''
    import subprocess
    import time

    def second_monitor(machine, name):
        """Screenshot the output of the second graphics card."""
        ppm = f"{machine.out_dir}/{name}.ppm"
        machine.send_monitor_command(f"screendump {ppm} second")
        subprocess.run(f"pnmtopng {ppm} > {machine.out_dir}/{name}.png && rm {ppm}", shell=True, check=True)

    # Not start_all(): the initrd machine is booted when its turn comes, because
    # its screenshots must catch short-lived moments of the boot.
    held.start()
    greeter.start()

    with subtest("held: draws during boot, records, exits after hold"):
        held.wait_until_succeeds("journalctl -b -u bootmap.service | grep -q 'drawing on'", timeout=60)
        held.sleep(1)
        held.screenshot("bootmap-during-boot")
        held.wait_for_unit("multi-user.target")
        # Holding the finished graph: on our own VT, so the login prompt that
        # has started on tty1 does not take the screen back.
        held.wait_for_unit("getty@tty1.service")
        held.succeed("test $(cat /sys/class/tty/tty0/active) = tty12")
        time.sleep(1)
        held.screenshot("held-after-getty")
        second_monitor(held, "second-monitor")
        unit_log = held.succeed("journalctl -b -u bootmap.service --no-pager")
        assert "bootmap: mirroring to Virtual-2 at 1024x768" in unit_log, unit_log
        assert "ignoring configuration" not in unit_log, unit_log
        held.succeed("test -e /etc/bootmap.toml")
        held.succeed("bootmap check-config /etc/bootmap.toml")
        held.wait_until_succeeds("test -s /var/log/bootmap/last.json", timeout=60)
        held.succeed("jq -e '.units | length > 20' /var/log/bootmap/last.json")
        held.succeed("jq -e '.phases.finish > 0' /var/log/bootmap/last.json")
        held.wait_until_succeeds("! systemctl is-active bootmap.service", timeout=30)
        # Done: back on the console's VT, where the login prompt is.
        held.succeed("test $(cat /sys/class/tty/tty0/active) = tty1")
        held.screenshot("held-after-exit")
        held.succeed("bootmap render /var/log/bootmap/last.json -o /tmp/final.png")
        held.copy_from_machine("/tmp/final.png", "")

    with subtest("luks: the passphrase prompt is shown and answered from the keyboard"):
        luks.start()
        luks.wait_for_console_text("bootmap: asking for a passphrase")
        time.sleep(1)
        luks.screenshot("passphrase-prompt")
        luks.send_chars("not the passphrase\n")
        luks.wait_for_console_text("bootmap: answered a passphrase request")
        # cryptsetup rejects it and asks again.
        luks.wait_for_console_text("bootmap: asking for a passphrase")
        luks.send_chars("sample")
        time.sleep(1)
        luks.screenshot("passphrase-retry")
        luks.send_chars(" passphrase\n")
        luks.wait_for_unit("multi-user.target")
        luks.succeed("test -e /dev/mapper/secret")
        unit_log = luks.succeed("journalctl -b -u bootmap.service --no-pager")
        assert unit_log.count("answered a passphrase request") == 2, unit_log
        # The console agent stayed away without tripping over its condition.
        whole_log = luks.succeed("journalctl -b --no-pager")
        assert "Trigger limit hit" not in whole_log
        luks.fail("systemctl is-failed systemd-ask-password-console.path systemd-ask-password-console.service")
        luks.wait_until_succeeds("! systemctl is-active bootmap.service", timeout=60)
        luks.fail("test -e /run/bootmap/agent")

    with subtest("latedisplay: no display in the initrd, drawing once the driver loads"):
        latedisplay.start()
        latedisplay.wait_for_unit("multi-user.target")
        unit_log = latedisplay.succeed("journalctl -b -u bootmap.service --no-pager")
        print(unit_log)
        assert "followed systemd into the real root" in unit_log, unit_log
        assert unit_log.count("bootmap: drawing on") == 1, unit_log
        latedisplay.succeed("test $(cat /sys/class/tty/tty0/active) = tty12")
        latedisplay.screenshot("late-display")
        latedisplay.wait_until_succeeds("jq -e '.phases.finish > 0' /var/log/bootmap/last.json", timeout=60)
        latedisplay.wait_until_succeeds("! systemctl is-active bootmap.service", timeout=30)

    with subtest("initrd: one process follows the boot across switch-root"):
        initrd.start()
        initrd.wait_for_console_text("bootmap: drawing on")
        time.sleep(2)  # host-side; lands while slow-initrd is still sleeping
        initrd.screenshot("initrd")
        # Right after the switch, while the display is still ours: once the
        # getty starts, its login prompt covers the graph.
        initrd.wait_for_console_text("followed systemd into the real root")
        time.sleep(1.5)
        initrd.screenshot("after-switch")
        initrd.wait_for_unit("multi-user.target")
        unit_log = initrd.succeed("journalctl -b -u bootmap.service --no-pager")
        assert "followed systemd into the real root" in unit_log, unit_log
        assert unit_log.count("bootmap: drawing on") == 1, unit_log
        # The file and the image were in the initrd, where bootmap read them.
        assert "ignoring configuration" not in unit_log, unit_log
        initrd.wait_until_succeeds("jq -e '.phases.finish > 0' /var/log/bootmap/last.json", timeout=60)
        initrd.succeed("jq -e '.units[\"slow-initrd.service\"].t_activating > 0' /var/log/bootmap/last.json")
        initrd.succeed("jq -e '.units[\"multi-user.target\"].t_active > 0' /var/log/bootmap/last.json")
        initrd.wait_until_succeeds("! systemctl is-active bootmap.service", timeout=30)
        initrd.succeed("bootmap render /var/log/bootmap/last.json --at -1 -o /tmp/replay-initrd.png")
        initrd.succeed("bootmap render /var/log/bootmap/last.json -o /tmp/final-initrd.png")
        initrd.copy_from_machine("/tmp/replay-initrd.png", "")
        initrd.copy_from_machine("/tmp/final-initrd.png", "")
        initrd.copy_from_machine("/var/log/bootmap/last.json", "")

    with subtest("greeter: hands over to greetd, then finishes recording"):
        greeter.wait_for_unit("greetd.service")
        greeter.wait_for_unit("graphical.target")
        unit_log = greeter.succeed("journalctl -b -u bootmap.service --no-pager")
        assert "released display" in unit_log, unit_log

        def mono(unit, prop):
            return int(greeter.succeed(f"systemctl show -P {prop} {unit}").strip())

        released = mono("bootmap-quit.service", "ExecMainExitTimestampMonotonic")
        greeter_started = mono("greetd.service", "InactiveExitTimestampMonotonic")
        assert 0 < released <= greeter_started, f"quit {released} vs greetd {greeter_started}"

        # Recording continues off-screen until the boot finishes, then exits.
        greeter.wait_until_succeeds("jq -e '.phases.finish > 0' /var/log/bootmap/last.json", timeout=60)
        greeter.succeed("jq -e '.units[\"greetd.service\"].t_active > 0' /var/log/bootmap/last.json")
        greeter.wait_until_succeeds("! systemctl is-active bootmap.service", timeout=30)
        greeter.wait_until_succeeds("pgrep cage && pgrep foot", timeout=60)
        greeter.sleep(3)
        greeter.screenshot("greeter")

    with subtest("greeter: shutdown view takes the display back from the compositor"):
        # Detached and delayed: the test shell is itself stopped by the shutdown.
        # Timers default to one-minute accuracy, so "in 1s" must be made exact.
        greeter.succeed("systemd-run --on-active=1 --timer-property=AccuracySec=100ms systemctl poweroff")
        greeter.wait_for_console_text("bootmap: showing shutdown", timeout=30)
        time.sleep(2.5)  # host-side: the guest shell is gone
        greeter.screenshot("shutdown")
        greeter.wait_for_shutdown()
  '';
}
