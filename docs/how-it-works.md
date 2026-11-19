# How bootmap works

- [Reading the boot from systemd](#reading-the-boot-from-systemd)
- [Laying out the graph](#laying-out-the-graph)
- [Drawing](#drawing)
- [At boot](#at-boot)
- [Following the boot from the initrd](#following-the-boot-from-the-initrd)
- [Handing over to the login screen](#handing-over-to-the-login-screen)
- [Passphrases](#passphrases)
- [Shutdown](#shutdown)
- [Recordings](#recordings)
- [Quirks of systemd's private socket](#quirks-of-systemds-private-socket)

## Reading the boot from systemd

bootmap asks systemd (PID 1) directly, over D-Bus. Early in the boot the system
bus does not exist yet, so it connects to PID 1's private socket,
`/run/systemd/private`, the one `systemctl` uses as root; elsewhere it uses the
system bus.

On connecting it subscribes to change signals, then reads every loaded unit:
its state, its dependencies (`Requires`, `Wants`, `BindsTo`, `After`), when it
started and stopped, and the processes in its cgroup. After that it listens:
`UnitNew`, `JobNew`, `PropertiesChanged` and `StartupFinished` signals keep the
picture current. Subscribing first means changes that happen while the units
are being read are queued rather than lost.

systemd re-sends a unit's whole state whenever any property changes, and
signals queued during the initial read arrive after it already covers them.
An event therefore only counts if it moves the unit forward in time.

## Laying out the graph

Each unit's parent in the graph is its **critical-chain predecessor**: the
`After=` dependency that finished starting last before it. This is the rule
`systemd-analyze critical-chain` uses, so following parents from any unit
toward the center traces what it was waiting for. Units without one hang off
the center.

From these parents bootmap builds a radial tree: each unit sits at a distance
from the center given by its depth in the tree, and each subtree gets a wedge
of the circle. That tree is not drawn as such. It is the starting point of a
**force simulation**, in the spirit of d3-force and Obsidian's graph view:

- units repel each other;
- dependency links act as springs;
- weak gravity holds the graph together;
- a radial pull (`--radial`, `radialPull`) nudges each unit toward the
  distance of its depth.

Starting from the tree makes the simulation settle in a couple of seconds and
come out the same every time. Units that appear during a live boot start next
to their parent and drift into place.

**Template instances** (`modprobe@drm.service`, `modprobe@fuse.service`) share
one node. systemd's naming rule, `name@instance.type`, is part of the unit
model (every instance's unit file is the template's), so the grouping is exact
rather than a guess from similar names. A group orders by its last member to
finish, and links to and from its members become links to and from the group.
An instance that fails leaves its group, so a group never hides a problem.

## Drawing

Every frame is drawn on the CPU with [tiny-skia](https://github.com/RazrFalcon/tiny-skia),
with text from the bundled DejaVu Sans. No GPU API is involved: early in the
boot there is often no GPU driver yet, but there is always something that can
show a buffer of pixels. A full 2560×1600 frame takes about 9 ms.

**Labels** go to what tells the story, in this order: the hovered unit and its
neighbors (in the viewer), failures, units starting or stopping, units that
just finished starting (shown for about 1.5 s, fading), the slowest units once
the boot has finished, and targets. Lower-priority labels that would overlap
higher ones are skipped. Replays run a moment past the last event, so the
final picture is the settled one.

**Size.** All sizes are defined for a ~96 dpi screen and multiplied by a scale:
at boot from the physical size the display reports (falling back to its
height when it reports none), in the viewer from the desktop's scale factor.

## At boot

The NixOS module runs `bootmap boot` as `bootmap.service`, with no default
dependencies and wanted by `sysinit.target`, so it starts as early as
possible.

**The display.** With no compositor this early, bootmap uses the kernel's
modesetting interface (KMS) directly: it opens `/dev/dri/card*`, becomes DRM
master, and points each connected output at a framebuffer of its own (a "dumb
buffer"). The picture is rendered once, at the primary output's size (the
built-in panel, else the largest), and scaled for the others.

It looks for a display without pausing anything else, every 100 ms until the
boot is over. On a machine whose graphics driver only loads in the real
system, there is none in the initrd, and the boot still has to be followed.
Early in the boot the only device may be the firmware framebuffer
(`simpledrm`), which disappears when the real driver loads; bootmap then
switches to the new device.

**Its own virtual terminal.** bootmap brings tty12 to the front in graphics
mode and goes back to the previous terminal when done. If it drew on the
console's terminal, a login prompt starting there would reset it to text mode,
and the kernel would put the text console back on screen.

**Readiness.** The unit is `Type=notify`. It reports ready once it has a
display, or after 3 s without one; systemd's console password prompt waits
for that (see [passphrases](#passphrases)).

## Following the boot from the initrd

With `initrd = true` the same unit is also part of the initrd, and the same
process carries on into the real system:

- **Not killed at the switch.** Its `argv[0]` starts with `@`, which
  switch-root spares, and the unit has `IgnoreOnIsolate=yes`, so isolating
  `initrd-switch-root.target` does not stop it.
- **Still tracked.** The unit has the same name in both stages, so the
  re-executed systemd keeps tracking the process as `bootmap.service`, and
  `systemctl kill bootmap.service` still reaches it.
- **Into the real root.** Shortly before the switch, bootmap opens `/sysroot`
  and holds it. When PID 1 re-executes and the connection drops, it changes
  into that directory and makes it its root (`fchdir`, `chroot(".")`), then
  reconnects to the new PID 1. It keeps drawing on the same display
  throughout.
- **One timeline.** The new PID 1's view of the units is merged into what was
  watched in the initrd. Units that ran only in the initrd stay in the
  recording, and units that run in both stages (journald, udevd) keep both
  runs.

## Handing over to the login screen

`bootmap-quit.service`, ordered before the units in `handoverUnits`, works like
`plymouth-quit`: it asks bootmap to release the display and waits until it
has, then lets the login screen start. bootmap restores the console, drops
DRM master, switches back to the previous terminal, and keeps recording
off-screen until the boot finishes.

A simple `Conflicts=` between bootmap and the login screen does not work:
both start jobs are in the same boot transaction, and systemd resolves the
conflict by silently dropping the login screen's job.

## Passphrases

While it has the screen, bootmap is a systemd
[password agent](https://systemd.io/PASSWORD_AGENTS/): it watches
`/run/systemd/ask-password`, shows each request over the graph, and answers it
with what was typed on its terminal.

systemd's own console agent would read the same keyboard, so it is kept away
the way it is for Plymouth: its path unit is ordered after `bootmap.service`
and skipped while `/run/bootmap/agent` exists. If bootmap has no display when
it reports ready, the marker is never created and the console agent works as
usual; bootmap then steps aside the moment anything asks for a passphrase.

## Shutdown

`bootmap-shutdown.service` is pulled in by `shutdown.target`. It switches to
its own terminal, which makes a running compositor let go of the display, and
shows units stopping until systemd hands over to the final stage of the
shutdown, when the connection closes.

## Recordings

A recording is JSON: the boot's phase timestamps, every unit with its
dependencies and timestamps, and a time-ordered list of state changes. The
boot service saves one when the boot finishes; `bootmap capture` builds one
afterwards from what systemd still remembers. All timestamps are systemd's
monotonic clock, in microseconds since the kernel started.

## Quirks of systemd's private socket

Two surprises, as of systemd 262:

- **The sender is a well-known name.** PID 1 stamps its messages with
  `org.freedesktop.systemd1` where a bus would put a unique name such as
  `:1.42`. zbus rejects that, so bootmap uses libdbus (the `dbus` crate).
- **A short first message can go unanswered.** The client sends `BEGIN` and
  its first request together. While authenticating, PID 1 reads up to 256
  bytes at a time, and a short request swallowed by that read is not processed
  until more input arrives. It happens mostly while PID 1 is busy, which is
  exactly when bootmap connects, so bootmap opens with a deliberately long
  request.
