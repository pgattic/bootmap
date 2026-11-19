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
crates/bootmap/           the command
  main.rs                 command line
  live.rs                 live and replayed sources, the scene (layout, physics, camera)
  window.rs               bootmap view
  kms.rs                  bootmap boot: main loop, following the boot from the initrd
  display.rs              KMS outputs
  vt.rs                   virtual terminal and keyboard
  ask.rs                  password agent
nix/                      package, NixOS module, VM test
samples/                  recorded boots used by tests and the demo
docs/demo/                the README animation and the script that makes it
```

## Tests

```sh
nix develop -c cargo test
nix build .#checks.aarch64-linux.vm -L     # or x86_64-linux
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
