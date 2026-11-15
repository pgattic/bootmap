{
  lib,
  rustPlatform,
  pkg-config,
  dbus,
  libxkbcommon,
  wayland,
  libGL,
  libx11,
  libxcursor,
  libxrandr,
  libxi,
  withWindow ? true,
}:

let
  runtimeLibs = [
    libxkbcommon
    wayland
    libGL
    libx11
    libxcursor
    libxrandr
    libxi
  ];
in
rustPlatform.buildRustPackage {
  pname = "bootmap";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../assets
      ../samples
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ dbus ];

  buildNoDefaultFeatures = true;
  buildFeatures = [ "drm" ] ++ lib.optional withWindow "window";

  # winit dlopen()s the windowing libraries at runtime.
  postFixup = lib.optionalString withWindow ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/bootmap
  '';

  meta = {
    description = "Graphical radial systemd boot visualizer";
    mainProgram = "bootmap";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
