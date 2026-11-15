{ pkgs, self }:

pkgs.testers.runNixOSTest {
  name = "bootmap";

  nodes = {
    # No login screen to hand over to: hold the finished graph for a while so
    # the screenshot can catch it, then exit on our own.
    held =
      { pkgs, ... }:
      {
        imports = [ self.nixosModules.default ];
        services.bootmap.enable = true;
        services.bootmap.holdSeconds = 8;
        services.bootmap.handoverUnits = [ ];
        environment.systemPackages = [ pkgs.jq ];
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
    import time

    # Not start_all(): the initrd machine is booted when its turn comes, because
    # its screenshots must catch short-lived moments of the boot.
    held.start()
    greeter.start()

    with subtest("held: draws during boot, records, exits after hold"):
        held.wait_until_succeeds("journalctl -b -u bootmap.service | grep -q 'drawing on'", timeout=60)
        held.sleep(1)
        held.screenshot("bootmap-during-boot")
        held.wait_for_unit("multi-user.target")
        held.wait_until_succeeds("test -s /var/log/bootmap/last.json", timeout=60)
        held.succeed("jq -e '.units | length > 20' /var/log/bootmap/last.json")
        held.succeed("jq -e '.phases.finish > 0' /var/log/bootmap/last.json")
        held.wait_until_succeeds("! systemctl is-active bootmap.service", timeout=30)
        held.succeed("bootmap render /var/log/bootmap/last.json -o /tmp/final.png")
        held.copy_from_machine("/tmp/final.png", "")

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
        initrd.wait_until_succeeds("jq -e '.phases.finish > 0' /var/log/bootmap/last.json", timeout=60)
        initrd.succeed("jq -e '.units[\"slow-initrd.service\"].t_activating > 0' /var/log/bootmap/last.json")
        initrd.succeed("jq -e '.units[\"multi-user.target\"].t_active > 0' /var/log/bootmap/last.json")
        initrd.wait_until_succeeds("! systemctl is-active bootmap.service", timeout=30)
        initrd.succeed("bootmap render /var/log/bootmap/last.json --at -1 -o /tmp/replay-initrd.png")
        initrd.succeed("bootmap render /var/log/bootmap/last.json -o /tmp/final-initrd.png")
        initrd.copy_from_machine("/tmp/replay-initrd.png", "")
        initrd.copy_from_machine("/tmp/final-initrd.png", "")

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
        greeter.succeed("systemd-run --on-active=1 systemctl poweroff")
        greeter.wait_for_console_text("bootmap: showing shutdown", timeout=30)
        time.sleep(2.5)  # host-side: the guest shell is gone
        greeter.screenshot("shutdown")
        greeter.wait_for_shutdown()
  '';
}
