{
  lib,
  rustPlatform,
  pkg-config,
  dbus,
  libxkbcommon,
  wayland,
  libx11,
  libxcursor,
  libxrandr,
  libxi,
  # Only `bootmap boot`, for the boot-time services: no window, no tools,
  # no windowing libraries. The full build adds `view`, `render`, `capture`.
  bootOnly ? false,
}:

let
  runtimeLibs = [
    libxkbcommon
    wayland
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
  buildFeatures = [ "drm" ] ++ lib.optionals (!bootOnly) [ "window" "tools" ];

  # winit dlopen()s the windowing libraries at runtime.
  postFixup = lib.optionalString (!bootOnly) ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/bootmap
  '';

  meta = {
    description = "Graphical systemd boot visualizer";
    mainProgram = "bootmap";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
