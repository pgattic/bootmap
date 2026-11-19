# A mistake in services.bootmap.settings must fail the build. Here the key
# "theme" is misspelled; the checked config file must fail to build, and the
# log must name the key.
{
  pkgs,
  self,
  nixpkgs,
}:

let
  system = pkgs.stdenv.hostPlatform.system;
  bad = nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      self.nixosModules.default
      {
        fileSystems."/" = {
          device = "x";
          fsType = "ext4";
        };
        boot.loader.grub.enable = false;
        system.stateVersion = "26.11";
        services.bootmap = {
          enable = true;
          settings.tehme = "black";
        };
      }
    ];
  };
in
pkgs.testers.testBuildFailure' {
  drv = bad.config.services.bootmap.configFile;
  name = "bootmap-bad-config";
  expectedBuilderLogEntries = [ "tehme" ];
}
