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

    start_all()

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
