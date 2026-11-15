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

    # A real greeter with the default handover: the greeter must start (it
    # used to be dropped from the boot transaction), only after bootmap has
    # released the display, and the recording must still be complete.
    greeter =
      { pkgs, lib, ... }:
      {
        imports = [ self.nixosModules.default ];
        services.bootmap.enable = true;
        services.greetd = {
          enable = true;
          settings.default_session.command = "${lib.getExe' pkgs.greetd "agreety"} --cmd ${pkgs.bashInteractive}/bin/bash";
        };
        environment.systemPackages = [ pkgs.jq ];
      };
  };

  testScript = ''
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
        greeter.sleep(2)
        greeter.screenshot("greeter")
  '';
}
