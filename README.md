# bootmap

A graphical systemd boot visualizer. It draws the boot as a living graph, in
the spirit of Obsidian's graph view: the system sits at the center, units
float around it connected by their dependencies, and processes orbit their
units. Units light up as they start. It is inspired by
[bootvine](https://codeberg.org/wvhulle/bootvine), which shows the same idea as
a TUI tree.

## Layouts

**`force`** (default) is a physics simulation:

- units repel each other;
- dependency edges act as springs;
- weak gravity holds the graph together;
- a soft **radial pull** nudges each unit toward the ring of its dependency depth.

`--radial 0` lets the graph float freely. `--radial 1` keeps it close to
rings, so early units stay near the center and `graphical.target` drifts
outward. The simulation starts from the radial layout, so it settles in a
couple of seconds and comes out the same every time. When new units appear
during a live boot, they spawn next to their parent and drift into place.
Nodes are sized by how many connections they have.

**`radial`** is a rigid radial tree where distance from the center is exactly
the dependency depth. Each unit's parent is its critical-chain predecessor:
the `After=` dependency that finished starting last before it (the rule
`systemd-analyze critical-chain` uses). Each subtree gets its own wedge.

In both layouts, colors show state:

- grey: pending
- amber: activating (pulsing)
- green: active
- blue: targets
- hollow teal: exited (oneshots)
- red: failed
- violet: processes

## Usage

```sh
nix develop                                   # toolchain
cargo run -- capture -o boot.json             # snapshot this boot's timeline
cargo run -- view samples/mbair-boot.json     # replay in a window
cargo run -- view                             # watch systemd live
cargo run -- render boot.json --at 1.5 -o f.png   # headless frame (seconds after userspace start, or "end")
cargo run -- render boot.json --highlight basic.target -o f.png   # as if hovering a unit
```

In the viewer:

| Input | Action |
|---|---|
| hover | highlight a unit and its neighbors; details panel |
| drag a unit | move it (force layout); it rejoins the physics on release |
| drag background / scroll | pan / zoom at cursor |
| `f` or `0` | fit the graph to the window again |
| `m` | switch between force and radial layouts |
| `[` / `]` | weaken / strengthen the radial pull |
| `space`, `←`/`→`, `r` | pause, scrub, restart the replay |
| `l`, `e` | toggle labels, toggle extra dependency edges |
| `q` / `Esc` | quit |

Graph flags:

- `--layout force|radial` and `--radial <0..1>`
- `--devices` and `--slices` to also show those unit types
- `--all` to include units that never started
- `--no-processes` and `--no-extra-edges`

`samples/mbair-boot.json` is a real boot (NixOS on Apple Silicon, 384 units)
used by the layout and physics tests.

## At boot (NixOS)

```nix
{
  inputs.bootmap.url = "path:/home/pgattic/git/bootmap";  # or a git URL
  outputs = { nixpkgs, bootmap, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      modules = [
        bootmap.nixosModules.default
        { services.bootmap.enable = true; }
      ];
    };
  };
}
```

`bootmap boot` starts early in stage 2 (`DefaultDependencies=no`, wanted by
`sysinit.target`). It:

- talks to PID 1 over its private socket `/run/systemd/private`, because dbus
  isn't running yet;
- takes over the display with KMS dumb buffers and puts the VT in graphics mode;
- reopens the display if simpledrm hands over to the real GPU driver.

When the boot finishes it saves the timeline to `/var/log/bootmap/last.json`,
so you can replay it later with `bootmap view`.

It keeps drawing until a unit in `services.bootmap.handoverUnits` starts. By
default that list is the display manager and `getty@tty1`. The module sets
`Conflicts=` on those units, so systemd stops bootmap, which restores the
console, right before they start. Alternatively, set `holdSeconds` to exit a
fixed time after boot finishes.

Options: `enable`, `package`, `layout` (`force`/`radial`), `radialPull`,
`holdSeconds`, `handoverUnits`, `showDevices`,
`showProcesses`, `recordPath`, `quietKernel` (adds `quiet loglevel=3`). Plymouth
must be disabled (asserted).

## Tests

```sh
nix develop -c cargo test                     # layout tests on the sample boot
nix build .#checks.aarch64-linux.vm -L        # boots a NixOS VM with the module
```

The VM test checks that bootmap draws on `/dev/dri/card0` during boot, saves a
recording with the finish timestamp, and exits. It also leaves
`bootmap-during-boot.png` (a QEMU screenshot of the live graph) and
`final.png` in `result/`.

## Notes on systemd's private socket

Two quirks found while testing, as of systemd 262:

- PID 1 stamps messages with the sender `org.freedesktop.systemd1`, which is not
  a unique name. zbus asserts on that, so bootmap uses libdbus (the `dbus` crate).
- `ListUnits` and `GetAll` on the Manager never get a reply over the private
  socket, while `ListUnitsByPatterns([], [])` and per-property `Get` work. This
  happens with sd-bus's `busctl` too.

## Layout

```
crates/bootmap-core   model (JSON timeline), systemd client, radial layout, tiny-skia renderer
crates/bootmap        CLI: capture / render / view (winit+softbuffer) / boot (KMS)
nix/                  package, NixOS module, VM test
```
