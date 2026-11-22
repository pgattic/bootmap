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
- [Appearance](#appearance)
- [Troubleshooting](#troubleshooting)

## Reading the graph

The white dot in the middle is the goal of the boot. Every other dot is a
systemd unit (a service, mount, socket, target and so on), linked to the
units it depends on. Each unit hangs off the nearest unit that waits for it,
so the goal's direct dependencies, like `graphical.target`, are close to the
middle, and the units that start first are at the edge. Bigger dots have
more connections.

The graph works like a burndown chart. It shows everything systemd is going
to start, and a unit fades out once it has started and nothing below it is
left. The graph is cut back from the edge toward the middle: what is still
on screen is the work left to do, and its outer edge is what is running
right now. The units that are left close in on the space that opens up, and
the view zooms in on them, up to three times closer than at the start.

Failed units are never removed, and neither is the chain from a failure to
the middle, so a boot that went wrong ends with a picture of what failed and
what it held up. A boot that went well ends with the middle on its own, plus
the slowest units in the panel on the left.

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

"Everything systemd is going to start" is what it knows about so far. In
the initrd that is the initrd's own boot. The real system's units appear
after the switch to the real root, and units pulled in later, for example by
hardware being found, join the graph when systemd queues them.

### Grow mode

With `mode = "grow"` (see [appearance](#appearance)) nothing is removed, and
the graph is arranged by time instead. The middle is the start of the boot,
and each unit hangs off the unit it waited for last, the same rule as
`systemd-analyze critical-chain`. Units that start early sit near the middle,
and units that come later drift outward. Each unit lights up as it starts
and stays, so the finished graph shows the whole boot, and following a unit
toward the middle traces what it was waiting for.

The [shutdown view](#shutdown) never removes units, in either mode: they fade
out as they stop.

### The panel

The panel on the left shows the time since the boot started, how many units
are in each state, and either what is starting right now or, once the boot is
done, the slowest units. The bottom line shows how long each stage of the boot
took: firmware, boot loader, kernel, initrd and the rest.

## The viewer

Two commands open a window:

```sh
bootmap view                    # watch the running system live
bootmap replay                  # replay this boot
bootmap replay recording.json   # replay a recorded boot
```

`bootmap view` shows the whole graph, with nothing pruned, so that a finished
boot can be looked at in full. The arrangement still follows `mode`: in prune
mode the goal is in the middle.

`bootmap replay` plays a boot back as it was drawn at boot, pruning included.
Without a file it replays this boot: from bootmap's own recording,
`/var/log/bootmap/last.json`, when that is this boot's (it covers the initrd
too), otherwise from what systemd still knows about the boot. `--speed 0.5`
plays at half speed.

In either window, `p` switches pruning on or off (prune mode).

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
| `p` | prune finished units, as at boot, or show them all again (prune mode) |
| `q` or `Esc` | quit |

## Command line

| Command | What it does |
|---|---|
| `bootmap view` | Watch the running system live, in a window. |
| `bootmap replay [FILE]` | Replay a recording in a window, or this boot without one. `--speed 0.5` plays at half speed. |
| `bootmap capture -o FILE` | Save the current boot as a recording, from what systemd remembers. |
| `bootmap render FILE -o IMAGE.png` | Draw one frame of a recording to a PNG. `--at SECONDS` picks the moment (counted from the start of userspace; negative values reach into the initrd), `--size 1920x1080`, `--highlight UNIT` draws it as if hovered. With `--frames DIR` (and `--fps`, default 12) it writes the whole replay as numbered PNGs instead, moving as it does in the viewer: ready to turn into a video or GIF. |
| `bootmap boot` | The boot-time mode. The NixOS module runs it; you do not need to. |

Options shared by `view`, `replay`, `render` and `boot`:

| Option | Effect |
|---|---|
| `--radial 0..1` | How strongly units keep to their depth in the tree: 0 floats freely, 1 keeps each unit on the ring of its depth (default 0.5). |
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
| `radialPull` | `0.5` | How strongly units keep to their depth in the tree (0 to 1). |
| `handoverUnits` | display manager, `getty@tty1`, `autovt@tty1` | Login screens to step aside for. See [login screens](#login-screens). |
| `holdSeconds` | `0` | With no login screen to hand over to: how long to keep the finished graph up. |
| `showShutdown` | `true` | Show the graph when shutting down or rebooting. |
| `showProcesses` | `true` | Draw the processes of running units. |
| `showDevices` | `false` | Draw device units too. |
| `recordPath` | `/var/log/bootmap/last.json` | Where each boot is recorded. |
| `settings` | `{ }` | The look of the graph: theme, colors, text size, font, title, center image, hidden panels. Written to `/etc/bootmap.toml`; see [appearance](#appearance). |
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

On machines with two graphics chips (hybrid graphics), bootmap uses the
monitors of both. A laptop docked with its lid closed counts as having a
built-in panel if the panel still reports being connected; the picture is then
laid out for the panel and scaled onto the external monitor.

Text and dots keep the same physical size on any screen, so a high-resolution
laptop panel shows the same picture as a regular monitor, only sharper. In the
viewer, sizes follow your desktop's scale setting.

## Appearance

Theme, colors, text size, font, title and a picture in the middle can be
changed, and each panel can be hidden. The settings live in a TOML file,
`/etc/bootmap.toml`, which the NixOS module writes from
`services.bootmap.settings`:

```nix
services.bootmap.settings = {
  theme = "black";
  title = "Welcome back";
  text-scale = 1.2;
  center-image = ./logo.png;
  colors = {
    active = "#7ebae4";
    failed = "#f55";
  };
  show.legend = false;
};
```

Every key is optional; what you leave out keeps its default. The names are
the ones in the file, so `text-scale` and `center-image` keep their dashes,
and the `colors` and `show` groups are nested sets.

| Key | Default | Meaning |
|---|---|---|
| `theme` | `"dark"` | `"dark"` or `"black"`. |
| `text-scale` | `1` | Text size, from 0.5 to 3. It is applied on top of the automatic scaling for the screen (see [monitors and screen size](#monitors-and-screen-size)). |
| `font` | bundled DejaVu Sans | A TrueType or OpenType file, used for all text. |
| `center-image` | none | A PNG drawn in the middle of the graph. |
| `title` | `"bootmap · <hostname>"` | The heading at the top left. `""` hides it. |
| `mode` | `"prune"` | `"prune"` or `"grow"`. See [reading the graph](#reading-the-graph) and [grow mode](#grow-mode). |
| `colors.*` | from the theme | Overrides for single colors, as `"#rrggbb"` or `"#rgb"`. See below. |
| `show.status` | `true` | The elapsed time and the number of units in each state. |
| `show.activity` | `true` | What is starting right now, or the slowest units once the boot is done. |
| `show.legend` | `true` | The color key. |
| `show.phases` | `true` | The bottom line with the time per boot stage and the progress bar. |
| `show.labels` | `true` | Unit names next to the dots. |
| `show.extra-edges` | `true` | The faint lines for dependencies that are not part of the tree. |

Hidden panels take no space: what comes after them moves up.

**Themes.** `dark` has a dark blue-grey background. `black` is true black,
which suits OLED panels and a boot that starts from a black screen.

**Colors.** The names are the same for both themes; setting one replaces the
theme's value.

| Name | Colors |
|---|---|
| `background` | the background |
| `panel` | behind the details shown when hovering a unit, and the passphrase prompt |
| `text` | titles, labels and counts |
| `muted` | secondary text: headings, the legend, timings |
| `track` | the empty part of the progress bar |
| `pending` | units not started yet |
| `starting` | units that are starting or stopping, and the passphrase prompt's accent |
| `active` | running units |
| `target` | targets |
| `exited` | units that ran and exited |
| `failed` | failed units and errors |
| `process` | the processes of running units |

**Center image.** It must be a PNG (transparency works). It is scaled to fit
inside the center of the graph, so any size will do; a square image of 256 by
256 pixels or more stays sharp. With `initrd = true` the image, the font and
the settings file are included in the initrd, so they apply from the first
moment.

**Mistakes fail the rebuild, not the boot.** The module runs
`bootmap check-config` on the generated file while building, so an unknown key
(`tehme`), a bad value or a color that is not `#rrggbb` stops `nixos-rebuild`
with a message, and so does a font or image that cannot be loaded. If a file
is broken anyway, bootmap logs `ignoring configuration` and uses the defaults.

`bootmap view`, `bootmap replay` and `bootmap render` read the same file, so a
replay looks as the boot did. To try settings without rebuilding, point them
at another file with `--config PATH`:

```sh
bootmap replay --config ./try.toml
bootmap check-config ./try.toml
```

## Troubleshooting

Start with bootmap's log from the last boot:

```sh
journalctl -b -u bootmap.service
```

| Symptom | Likely cause |
|---|---|
| Nothing shows at first; the log says `no graphics device yet` | No graphics driver this early. The graph appears once the driver loads (the log then says `drawing on`); for an earlier picture, see [starting in the initrd](#starting-in-the-initrd). |
| Nothing shows at all on an NVIDIA machine | The proprietary driver only offers the screen to programs like bootmap with kernel modesetting: set `hardware.nvidia.modesetting.enable = true;` (the default with driver 535 or newer), which adds `nvidia-drm.modeset=1`. |
| The log says `graphics device(s), none usable` | A driver is loaded, but bootmap could not use any output; the lines just above it in the log say why for each one (for example, no monitor connected). |
| The graph shows briefly, then the login screen | Expected: bootmap steps aside for your login screen. Replay the boot with `bootmap replay`. |
| The build fails mentioning Plymouth | Disable `boot.plymouth.enable`; both want the screen. |
| `bootmap view` or `bootmap replay` fails to open a window | It needs a running desktop (Wayland or X11). |
