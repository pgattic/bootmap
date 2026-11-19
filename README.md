# bootmap

A graphical systemd boot visualizer. It draws the boot as a living graph, in
the spirit of Obsidian's graph view: the system sits at the center, units
float around it connected by their dependencies, and processes orbit their
units. Units light up as they start. It is inspired by
[bootvine](https://codeberg.org/wvhulle/bootvine), which shows the same idea as
a TUI tree.

## The graph

The layout is a physics simulation:

- units repel each other;
- dependency edges act as springs;
- weak gravity holds the graph together;
- a soft **radial pull** nudges each unit toward a distance from the center
  that matches its dependency depth.

`--radial 0` lets the graph float freely. `--radial 1` holds units close to
their depth, so early units stay near the center and `graphical.target` drifts
outward. When new units appear during a live boot, they spawn next to their
parent and drift into place. Nodes are sized by how many connections they
have.

Each unit's parent is its critical-chain predecessor: the `After=` dependency
that finished starting last before it (the rule `systemd-analyze
critical-chain` uses). The simulation starts from a radial tree built on
those parents, so it settles in a couple of seconds and comes out the same
every time.

Colors show state:

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
| drag a unit | move it; it rejoins the physics on release |
| drag background / scroll | pan / zoom at cursor |
| `f` or `0` | fit the graph to the window again |
| `[` / `]` | weaken / strengthen the radial pull |
| `space`, `←`/`→`, `r` | pause, scrub, restart the replay |
| `l`, `e` | toggle labels, toggle extra dependency edges |
| `q` / `Esc` | quit |

Graph flags:

- `--radial <0..1>`
- `--devices` and `--slices` to also show those unit types
- `--all` to include units that never started
- `--no-processes` and `--no-extra-edges`

`samples/mbair-boot.json` is a real boot (NixOS on Apple Silicon, 384 units)
used by the layout and physics tests.

## At boot (NixOS)

```nix
{
  inputs.bootmap.url = "github:pgattic/bootmap";
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
- takes over the display with KMS dumb buffers;
- reopens the display if simpledrm hands over to the real GPU driver.

**Monitors.** It draws on every connected output. The picture is rendered
once, at the size of the built-in panel (or the largest output when there is
none), and scaled to fit the others. Outputs connected after it has started
are not picked up.

**Its own virtual terminal.** It draws on tty12 (`bootmap boot --vt`) and goes
back to the terminal that was in front when it is done. On the console's own
terminal, a login prompt starting there would put the text console back on
screen while bootmap is still drawing.

**Passphrases.** While it has the screen, bootmap is a systemd
[password agent](https://systemd.io/PASSWORD_AGENTS/). A request, such as
unlocking an encrypted disk, is shown over the graph and answered from the
keyboard: Enter confirms, Esc or Ctrl-U starts over. A wrong passphrase is
asked for again. systemd's console agent would read the same keyboard, so it
is left out while bootmap handles prompts, the way it is for Plymouth: its
path unit is ordered after `bootmap.service` (`Type=notify`, ready once it has
the display or after 3 s without one) and skipped while `/run/bootmap/agent`
exists. If bootmap has no display by then, the console agent runs as usual,
and bootmap gives the screen back as soon as something asks for a passphrase.

With `services.bootmap.initrd = true` it starts in the systemd initrd instead,
so the picture begins well under a second into the boot. The same process
follows the boot into the real system:

- its `argv[0]` starts with `@` and the unit has `IgnoreOnIsolate=yes`, so
  switch-root does not kill it;
- the unit has the same name (`bootmap.service`) in the initrd and in the real
  system, so systemd keeps tracking the process, and `systemctl kill` still
  reaches it;
- shortly before the switch it opens `/sysroot`; when PID 1 re-executes and the
  connection drops, it does `fchdir`, `chroot` and `chdir` into that directory,
  reconnects to the new PID 1 and keeps drawing on the same display.

This needs a systemd initrd, which current NixOS always uses (on older
releases, set `boot.initrd.systemd.enable`). An encrypted root is unlocked
from bootmap's passphrase prompt. The recording then includes the initrd, and
`bootmap render --at -1` shows a frame from it (`--at` counts seconds after
userspace start).

When the boot finishes it saves the timeline to `/var/log/bootmap/last.json`.
Replay it from your desktop with `bootmap view /var/log/bootmap/last.json`.

The module installs two builds. The boot-time services run one with only
`bootmap boot` and no windowing libraries (`package`, which is also what goes
into the initrd). Users get the full one with `view`, `render` and `capture`
(`userPackage`; set it to `null` on a machine without a desktop).

It keeps drawing until just before the login screen starts (the units in
`services.bootmap.handoverUnits`: by default the display manager and
`getty@tty1`). At that point `bootmap-quit.service`, which works like
`plymouth-quit`, signals bootmap to release the display and waits until it
has. bootmap then keeps recording off-screen until the boot finishes, so the
saved timeline is complete. With `handoverUnits = [ ]` it keeps drawing and
exits `holdSeconds` after the boot finishes instead; a text login prompt is
then waiting on the console's terminal when it switches back.

At shutdown or reboot, `bootmap-shutdown.service` (pulled in by
`shutdown.target`) brings the graph back. It switches to its own virtual
terminal, which makes a running compositor let go of the display, then shows
units fading out as they stop and lists the ones holding the shutdown up. It
runs until systemd hands over to the final shutdown stage. Disable it with
`showShutdown = false`.

Options: `enable`, `package`, `userPackage`, `radialPull`,
`holdSeconds`, `handoverUnits`, `initrd`, `showShutdown`, `showDevices`,
`showProcesses`, `recordPath`, `quietKernel` (adds `quiet loglevel=3`). Plymouth
must be disabled (asserted).

## Tests

```sh
nix develop -c cargo test                     # layout tests on the sample boot
nix build .#checks.aarch64-linux.vm -L        # boots a NixOS VM with the module
```

The VM test boots four machines:

- `held` has no login screen and a second graphics card standing in for a
  second monitor. It checks that bootmap draws during boot, mirrors to the
  second output, keeps the finished graph up on tty12 while a login prompt
  runs on tty1, goes back to tty1 afterwards, and saves a complete recording.
- `luks` has an encrypted volume to unlock in the initrd. It types a wrong
  passphrase, then the right one, into bootmap's prompt, and checks that the
  volume is open and that the console agent stayed out without failing.
- `initrd` starts bootmap in the systemd initrd and checks that the same
  process survives the switch to the real root, on the same display, and that
  the recording includes the initrd.
- `greeter` runs a Wayland compositor as its greeter (with bootmap also
  started in the initrd) and checks the handover to it, and that the shutdown
  view takes the display back from it.

It leaves QEMU screenshots in `result/` (`bootmap-during-boot.png`,
`held-after-getty.png`, `second-monitor.png`, `passphrase-prompt.png`,
`passphrase-retry.png`, `initrd.png`, `after-switch.png`, `greeter.png`,
`shutdown.png` and others), plus renders of the finished recordings.

## Notes on systemd's private socket

Two quirks found while testing, as of systemd 262:

- PID 1 stamps messages with the sender `org.freedesktop.systemd1`, which is not
  a unique name. zbus asserts on that, so bootmap uses libdbus (the `dbus` crate).
- A short first message on a new connection can go unanswered. libdbus sends
  it right after `BEGIN`; sd-bus in PID 1 reads up to 256 bytes while
  authenticating, and a message swallowed by that read is not processed until
  more input arrives. It happens mostly while PID 1 is busy, which is exactly
  when bootmap connects. bootmap therefore opens with a deliberately long call.

## Layout

```
crates/bootmap-core   model (JSON timeline), systemd client, dependency tree, force simulation, tiny-skia renderer
crates/bootmap        CLI: capture / render / view (winit+softbuffer) / boot
                      (kms: main loop, display: KMS outputs, vt: terminal and
                      keyboard, ask: password agent)
nix/                  package, NixOS module, VM test
```
