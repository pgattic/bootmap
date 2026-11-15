{ pkgs, self }:

pkgs.testers.runNixOSTest {
  name = "bootmap";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ self.nixosModules.default ];
      services.bootmap.enable = true;
      # Hold the finished graph briefly so the screenshot can catch it, then
      # let getty take over the console.
      services.bootmap.holdSeconds = 8;
      services.bootmap.handoverUnits = [ ];
      environment.systemPackages = [ pkgs.jq ];
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("bootmap.service")
    machine.wait_until_succeeds("journalctl -b -u bootmap.service | grep -q 'drawing on'", timeout=60)
    machine.sleep(1)
    machine.screenshot("bootmap-during-boot")
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds("test -s /var/log/bootmap/last.json", timeout=60)
    machine.succeed("jq -e '.units | length > 20' /var/log/bootmap/last.json")
    machine.succeed("jq -e '.phases.finish > 0' /var/log/bootmap/last.json")
    # The drawing used the private socket and a KMS device.
    unit_log = machine.succeed("journalctl -b -u bootmap.service --no-pager")
    assert "drawing on /dev/dri/card" in unit_log, unit_log
    machine.wait_until_succeeds("! systemctl is-active bootmap.service", timeout=30)
    machine.succeed("bootmap render /var/log/bootmap/last.json -o /tmp/final.png")
    machine.copy_from_machine("/tmp/final.png", "")
  '';
}
