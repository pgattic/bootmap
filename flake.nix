{
  description = "bootmap: graphical radial systemd boot visualizer";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      windowLibs =
        pkgs: with pkgs; [
          libxkbcommon
          wayland
          libGL
          libx11
          libxcursor
          libxrandr
          libxi
        ];
    in
    {
      packages = forAll (pkgs: rec {
        bootmap = pkgs.callPackage ./nix/package.nix { };
        # Build used at boot: no windowing dependencies.
        bootmap-minimal = bootmap.override { withWindow = false; };
        default = bootmap;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            pkg-config
            dbus
          ];
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (windowLibs pkgs);
        };
      });

      nixosModules.default = import ./nix/module.nix self;
      nixosModules.bootmap = self.nixosModules.default;

      checks = forAll (pkgs: {
        vm = import ./nix/vm-test.nix { inherit pkgs self; };
      });
    };
}
