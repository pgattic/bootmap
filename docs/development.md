# Development

## Building

The flake provides the toolchain:

```sh
nix develop                       # Rust, cargo, clippy, the libraries
cargo build
cargo run -- view samples/mbair-initrd-boot.json
```

Packages:

| Package | Contents |
|---|---|
| `bootmap` (default) | Everything: `view`, `render`, `capture`, `boot`. |
| `bootmap-minimal` | Only `bootmap boot`, without windowing libraries; for a smaller initrd. |

They are built from these cargo features of the `bootmap` crate:

| Feature | Adds |
|---|---|
| `drm` | `bootmap boot`: KMS display, terminal, password agent. |
| `window` | `bootmap view` (winit and softbuffer). |
| `tools` | `bootmap capture` and `bootmap render` (and PNG output). |

## Code layout

```
crates/bootmap-core/      the library
  model.rs                recordings: units, phases, events
  systemd.rs              the D-Bus client
  layout.rs               dependency tree and template groups
  force.rs                force simulation
  render.rs               drawing, labels, HUD, passphrase prompt
  config.rs               the TOML configuration: parsing, themes, loading font and image
crates/bootmap/           the command
  main.rs                 command line (including check-config)
  live.rs                 live and replayed sources, the scene (layout, physics, camera)
  window.rs               bootmap view
  kms.rs                  bootmap boot: main loop, following the boot from the initrd
  display.rs              KMS outputs
  vt.rs                   virtual terminal and keyboard
  ask.rs                  password agent
nix/                      package, NixOS module, VM test, bad-config check
samples/                  recorded boots used by tests and the demo
docs/demo/                the README animation and the script that makes it
```

## Tests

```sh
nix develop -c cargo test
nix build .#checks.aarch64-linux.vm -L     # or x86_64-linux
nix build .#checks.aarch64-linux.bad-config -L   # a misspelled setting fails the build
```

The unit tests run the layout and physics on the recorded boots in
`samples/`, and cover the model, template grouping and the password agent.

The VM test boots NixOS machines with the module, using QEMU:

| Machine | Checks |
|---|---|
| `held` | Draws during boot, mirrors to a second graphics card, keeps the finished graph on its own terminal while a login prompt runs on tty1, goes back to tty1 afterwards, records the whole boot. |
| `luks` | An encrypted volume in the initrd: a wrong passphrase, then the right one, typed into bootmap's prompt; the volume opens and systemd's console agent stays out without failing. |
| `latedisplay` | No display in the initrd (the graphics driver loads later, as on a legacy-BIOS laptop): bootmap follows the switch to the real system and starts drawing when the driver loads. Uses `bootmap-minimal`. |
| `initrd` | The same process survives the switch to the real system, on the same display, and the recording includes the initrd. |
| `greeter` | A Wayland compositor as the login screen: the handover happens before it starts, and at shutdown the graph takes the display back from it. |

Screenshots from each machine land in `result/`.

The `held` and `initrd` machines also set `services.bootmap.settings`. `held`
uses a theme, title, colors and a center image built in the test, so its
screenshots show the configured look. `initrd` uses a small set with a center
image, to prove that the file and the image reach the initrd. Both assert that
the journal has no `ignoring configuration`.

## Configuration

`bootmap check-config FILE` parses a configuration file, loads its font and
image, and prints `ok` or fails with a message. Both packages have it.

The NixOS module (`nix/module.nix`) turns `services.bootmap.settings` into TOML
with `pkgs.formats.toml`, replaces `font` and `center-image` with store paths,
runs `check-config` on the result in a `runCommand` (skipped when the build
platform cannot run the host's binaries) and installs it as
`/etc/bootmap.toml`, also in the initrd with `initrd = true`. The checked file
is the internal option `services.bootmap.configFile`.

The flake check `bad-config` (`nix/bad-config-test.nix`) evaluates a NixOS
configuration with the misspelled key `tehme` and uses `testers.testBuildFailure'`
to assert that building the checked file fails and that the log names the key.
It needs the package to build, but no VM.

## Continuous integration

`.github/workflows/ci.yml` runs on every push and pull request:

| Job | Runs on | Checks |
|---|---|---|
| Clippy | x86_64 | `cargo clippy` on all four build variants (default, `drm`, `window`, `tools`), warnings are errors. |
| Build and unit tests | x86_64 and aarch64 | Both packages (building them runs the unit tests) and the `bad-config` check. |
| NixOS VM test | x86_64, with KVM | The full VM test. Its screenshots are kept as a build artifact. |

The aarch64 VM test is not in CI; run it locally on an aarch64 machine.

## Debugging

| Variable | Effect |
|---|---|
| `BOOTMAP_DEBUG=1` | Log every D-Bus call and state change. |
| `BOOTMAP_STATS=1` | Print frame rate and time per frame every 2 s. With `render`, plays the recording for a few seconds as a benchmark. |

At boot: `journalctl -b -u bootmap.service`. To log the initrd stage to the
console as well, set
`boot.initrd.systemd.services.bootmap.serviceConfig.StandardError = "journal+console";`.

## The demo animation

`docs/demo/boot.gif` is rendered from `samples/mbair-initrd-boot.json`:

```sh
docs/demo/make-demo.sh [recording.json]
```

To use a boot of your own, copy `/var/log/bootmap/last.json` and pass it to
the script.
