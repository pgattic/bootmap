# Using bootmap

- [Reading the graph](#reading-the-graph)
- [The viewer](#the-viewer)
- [Command line](#command-line)
- [NixOS options](#nixos-options)
- [Starting in the initrd](#starting-in-the-initrd)
- [Encrypted disks](#encrypted-disks)
- [Login screens](#login-screens)
- [Shutdown](#shutdown)
- [Monitors and screen size](#monitors-and-screen-size)
- [Troubleshooting](#troubleshooting)

## Reading the graph

The white dot in the middle is the system itself. Every other dot is a systemd
unit (a service, mount, socket, target and so on), linked to the units it
depends on. Units that start early sit near the center; units that come later,
like `graphical.target`, drift outward. Bigger dots have more connections.

| Color | Meaning |
|---|---|
| grey | not started yet |
| amber, pulsing | starting |
| green | running |
| blue | target (a milestone of the boot) |
| hollow teal | ran and exited (one-shot jobs) |
| red | failed |
| small violet dots | processes of a running unit |

Labels appear where something is happening: on units that are starting,
stopping or failed, briefly on units that just finished starting, and on
targets. Once the boot has finished, the slowest units are labelled too.
Everything else is a dot; in the viewer, hover it to see its name.

Several instances of the same template, such as `modprobe@drm.service` and
`modprobe@fuse.service`, share one dot labelled `modprobe@ ×2`. An instance
that fails always gets a dot of its own.

The panel on the left shows the time since the boot started, how many units
are in each state, and either what is starting right now or, once the boot is
done, the slowest units. The bottom line shows how long each stage of the boot
took: firmware, boot loader, kernel, initrd and the rest.

## The viewer

`bootmap view` opens a window. With a file, it replays a recorded boot; without
one, it watches the running system live.

```sh
bootmap view /var/log/bootmap/last.json   # replay the last boot
bootmap view                              # watch live
```

| Input | Action |
|---|---|
| hover a dot | highlight it and its neighbors, show its details |
| drag a dot | move it; it springs back into place when released |
| drag the background, scroll | pan, zoom |
| `f` or `0` | fit the whole graph in the window again |
| `space` | pause or resume the replay |
| `←` / `→` | step a quarter second back or forward |
| `r` | restart the replay |
| `[` / `]` | loosen or tighten how strongly units keep to their depth |
| `l`, `e` | labels on or off, extra dependency lines on or off |
| `q` or `Esc` | quit |

## Command line

| Command | What it does |
|---|---|
| `bootmap view [FILE]` | Replay a recording, or watch live without one. `--speed 0.5` plays at half speed. |
| `bootmap capture -o FILE` | Save the current boot as a recording, from what systemd remembers. |
| `bootmap render FILE -o IMAGE.png` | Draw one frame of a recording to a PNG. `--at SECONDS` picks the moment (counted from the start of userspace; negative values reach into the initrd), `--size 1920x1080`, `--highlight UNIT` draws it as if hovered. |
| `bootmap boot` | The boot-time mode. The NixOS module runs it; you do not need to. |

Options shared by `view`, `render` and `boot`:

| Option | Effect |
|---|---|
| `--radial 0..1` | How strongly units keep to their dependency depth: 0 floats freely, 1 keeps early units central (default 0.5). |
| `--devices`, `--slices` | Also show device units, or slice and scope units (hidden by default; there are many). |
| `--all` | Include units that never started. |
| `--no-processes` | Hide the process dots. |
| `--no-extra-edges` | Hide the faint extra dependency lines. |

## NixOS options

All under `services.bootmap`.

| Option | Default | Description |
|---|---|---|
| `enable` | `false` | Show bootmap at boot, record every boot, and install the `bootmap` command. |
| `initrd` | `false` | Start in the initrd, so the graph covers the whole boot. See [starting in the initrd](#starting-in-the-initrd). |
| `radialPull` | `0.5` | How strongly units keep to their dependency depth (0 to 1). |
| `handoverUnits` | display manager, `getty@tty1`, `autovt@tty1` | Login screens to step aside for. See [login screens](#login-screens). |
| `holdSeconds` | `0` | With no login screen to hand over to: how long to keep the finished graph up. |
| `showShutdown` | `true` | Show the graph when shutting down or rebooting. |
| `showProcesses` | `true` | Draw the processes of running units. |
| `showDevices` | `false` | Draw device units too. |
| `recordPath` | `/var/log/bootmap/last.json` | Where each boot is recorded. |
| `quietKernel` | `true` | Add `quiet loglevel=3` to the kernel command line, so kernel messages do not cover the screen before bootmap starts. |
| `package` | bootmap | The bootmap build to install and run. |
| `bootPackage` | `package` | The build used at boot. Set it to `bootmap.packages.${system}.bootmap-minimal` for an initrd about 1 MiB smaller; that build only has `bootmap boot`. |

bootmap and Plymouth both want the screen during boot, so the module refuses
to build with both enabled.

## Starting in the initrd

Most of a boot can happen before the real system starts: the initrd finds and
mounts the root file system, and unlocks it if it is encrypted. With
`initrd = true`, bootmap starts there, well under a second into the boot, and
follows the boot into the real system without interruption.

This only shows more if the screen works in the initrd. Most machines that
boot with UEFI have a basic display from the firmware from the start. Machines
that boot through a legacy BIOS usually have no display until the graphics
driver loads, so bootmap would appear at the same time either way. You can
load the graphics driver in the initrd instead ("early KMS"), for example on
Intel graphics:

```nix
boot.initrd.kernelModules = [ "i915" ];
```

The recording covers the initrd either way, so replays include it.

Starting in the initrd adds bootmap to every generation's initrd in `/boot`,
about 2.4 MiB compressed (1.3 MiB with the minimal build in `bootPackage`).

## Encrypted disks

While bootmap has the screen, it asks for passphrases itself: the prompt
appears over the graph, Enter confirms, and Esc or Ctrl-U starts over. A wrong
passphrase is asked for again. This covers an encrypted root (with
`initrd = true`) as well as other encrypted volumes unlocked later in the
boot.

If bootmap cannot get the screen in time, systemd's usual text prompt is used
instead, and bootmap steps aside as soon as anything asks for a passphrase.

## Login screens

bootmap draws until just before your login screen starts, then gives the
screen to it. The boot keeps being recorded in the background until it has
finished.

Without a graphical login screen, it steps aside for the text login prompt on
the first terminal. To keep the finished graph on screen instead, set
`handoverUnits = [ ]` and `holdSeconds` to how long it should stay up.

## Shutdown

When you shut down or reboot, the graph comes back, even from a running
desktop: units fade out as they stop, and the panel lists the ones still
stopping, longest first. Turn this off with `showShutdown = false`.

## Monitors and screen size

At boot, bootmap draws on every connected monitor. The picture is laid out for
the built-in panel (or the largest monitor) and scaled to fit the others.
Monitors plugged in after bootmap has started are not picked up.

Text and dots keep the same physical size on any screen, so a high-resolution
laptop panel shows the same picture as a regular monitor, only sharper. In the
viewer, sizes follow your desktop's scale setting.

## Troubleshooting

Start with bootmap's log from the last boot:

```sh
journalctl -b -u bootmap.service
```

| Symptom | Likely cause |
|---|---|
| Nothing shows at boot; the log says `no usable display yet` | No graphics driver this early. It appears once the driver loads; for an earlier picture, see [starting in the initrd](#starting-in-the-initrd). |
| The graph shows briefly, then the login screen | Expected: bootmap steps aside for your login screen. Replay the boot with `bootmap view /var/log/bootmap/last.json`. |
| The build fails mentioning Plymouth | Disable `boot.plymouth.enable`; both want the screen. |
| `bootmap view` fails to open a window | It needs a running desktop (Wayland or X11). |
