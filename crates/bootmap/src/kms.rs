//! Boot-time mode: follow the boot (or a shutdown) live on the display.
//!
//! Ties together the display (`display`), the virtual terminal we draw on
//! (`vt`), the password agent (`ask`) and the scene, and follows systemd from
//! the initrd into the real root.

use crate::ask::Agent;
use crate::display::Display;
use crate::live::{Scene, Source};
use crate::vt::Vt;
use crate::GraphArgs;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Signal flags. Installed first thing in `bootmap boot`: SIGUSR1's default
/// action is to terminate, so it must be caught before anything slow runs.
pub struct Signals {
    /// SIGTERM/SIGINT: stop now.
    stop: Arc<AtomicBool>,
    /// SIGUSR1: give the display back, keep recording until the boot finishes.
    release: Arc<AtomicBool>,
}

impl Signals {
    pub fn install() -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            signal_hook::flag::register(sig, stop.clone())?;
        }
        let release = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGUSR1, release.clone())?;
        Ok(Self { stop, release })
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn released(&self) -> bool {
        self.release.load(Ordering::Relaxed)
    }
}

/// Looking for a display without holding up the main loop. The boot must be
/// followed whether or not a display has shown up: on a machine whose GPU
/// driver only loads in the real system, there is none in the initrd, and
/// the switch to the real root has to be caught all the same.
struct DisplaySearch {
    started: Instant,
    next_try: Instant,
    /// Stop looking after this (the shutdown view, when a compositor never
    /// lets go). A boot keeps looking until it is over.
    give_up: Option<Instant>,
    /// Problems already logged: attempts repeat every 100 ms.
    seen: HashSet<String>,
    reported: bool,
}

impl DisplaySearch {
    /// How long until "no display yet" is logged.
    const NOTICE: Duration = Duration::from_secs(3);

    fn new(give_up_after: Option<Duration>) -> Self {
        let now = Instant::now();
        Self { started: now, next_try: now, give_up: give_up_after.map(|d| now + d), seen: HashSet::new(), reported: false }
    }

    /// One attempt, if it is time for one.
    fn try_open(&mut self) -> Option<Display> {
        let now = Instant::now();
        if now < self.next_try || self.give_up.is_some_and(|g| now >= g) {
            return None;
        }
        self.next_try = now + Duration::from_millis(100);
        let mut problems = vec![];
        let display = Display::open(&mut problems);
        for p in problems {
            if self.seen.insert(p.clone()) {
                eprintln!("bootmap: {p}");
            }
        }
        if display.is_none() && !self.reported && now.duration_since(self.started) >= Self::NOTICE {
            self.reported = true;
            eprintln!("bootmap: no usable display yet; recording, and still looking");
        }
        display
    }
}

extern "C" {
    fn fchdir(fd: i32) -> i32;
    fn chroot(path: *const std::ffi::c_char) -> i32;
}

/// Is the switch from the initrd to the real root about to happen?
fn switch_root_imminent(tl: &bootmap_core::BootTimeline) -> bool {
    let busy = |n: &str| tl.units.get(n).is_some_and(|u| u.job || u.t_activating.is_some());
    busy("initrd-switch-root.target")
        || busy("initrd-switch-root.service")
        || tl.units.get("initrd-cleanup.service").is_some_and(|u| u.t_activating.is_some())
}

/// Open `/sysroot` to hold it, if it is a mount point (its device differs from `/`'s).
fn open_sysroot() -> Option<File> {
    use std::os::unix::fs::MetadataExt;
    let root = std::fs::metadata("/").ok()?;
    let sysroot = std::fs::metadata("/sysroot").ok()?;
    if sysroot.dev() == root.dev() {
        return None;
    }
    File::open("/sysroot").ok()
}

/// Enter the directory held in `fd` as our new root.
fn enter_root(fd: &File) -> std::io::Result<()> {
    // SAFETY: plain syscalls on a valid directory fd and a NUL-terminated path.
    unsafe {
        if fchdir(fd.as_raw_fd()) != 0 || chroot(c".".as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    std::env::set_current_dir("/")
}

fn sd_notify(msg: &str) {
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else { return };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else { return };
    if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name) {
            let _ = sock.send_to_addr(msg.as_bytes(), &addr);
        }
    } else {
        let _ = sock.send_to(msg.as_bytes(), path);
    }
}

fn save(record: &Path, scene: &Scene) {
    if let Some(dir) = record.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(record, scene.source.tl.to_json()) {
        Ok(()) => eprintln!("bootmap: saved {}", record.display()),
        Err(e) => eprintln!("bootmap: could not save {}: {e}", record.display()),
    }
}

/// What to do about passphrase requests while we have the screen.
#[derive(Clone, Copy, PartialEq)]
enum Prompts {
    /// Be the password agent: show the prompt and answer from the keyboard.
    Answer,
    /// Not a boot (a replay, the shutdown view): nothing will ask.
    Ignore,
    /// systemd's console agent is in charge (we got the display too late to
    /// keep it away): if anything asks, give the screen back so its prompt
    /// can be seen.
    Yield,
}

/// The screen and keyboard, held together: the display, the VT in front of
/// it, and (during a boot) the password agent that uses both.
///
/// systemd's console password agent would read from the same terminal and
/// take keystrokes meant for our prompt. While `marker` exists it is not
/// started: the NixOS module orders its path unit after bootmap.service and
/// gives it that condition, the arrangement it has with Plymouth.
struct Console {
    // Field order is drop order: give up the display before the VT.
    display: Option<Display>,
    vt: Option<Vt>,
    agent: Option<Agent>,
    prompts: Prompts,
    marker: PathBuf,
}

impl Console {
    /// `display` is already open. `vt` may have been entered earlier (the
    /// shutdown view switches first, to make a compositor let go).
    fn new(display: Display, vt: Option<Vt>, vt_number: u32, prompts: Prompts, marker: PathBuf) -> Self {
        // Our own VT in every case, so that nothing else on the console's VT
        // (a login prompt starting there) takes the screen back. When the
        // console agent is in charge, nobody reads the keyboard here, and the
        // moment it asks for anything we switch back (`must_yield`), so its
        // prompt is seen and typed into as usual.
        let vt = vt.or_else(|| Vt::enter(vt_number));
        let agent = (prompts == Prompts::Answer && vt.is_some()).then(Agent::new);
        if agent.is_some() {
            if let Some(dir) = marker.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&marker, b"");
        }
        Self { display: Some(display), vt, agent, prompts, marker }
    }

    fn present(&mut self, pm: &tiny_skia::Pixmap) -> Result<()> {
        self.display.as_mut().context("no display")?.present(pm)
    }

    fn display_gone(&self) -> bool {
        self.display.as_ref().is_some_and(Display::gone)
    }

    /// Read the keyboard and look for password requests; returns the prompt to draw.
    fn prompt(&mut self) -> Option<bootmap_core::render::Prompt> {
        let agent = self.agent.as_mut()?;
        let mut keys = [0u8; 64];
        let n = self.vt.as_mut().map_or(0, |vt| vt.read_keys(&mut keys));
        agent.poll();
        agent.keys(&keys[..n]);
        keys.fill(0);
        agent.prompt()
    }

    /// Someone is asking for a passphrase and it is not ours to answer.
    fn must_yield(&self) -> bool {
        self.prompts == Prompts::Yield && crate::ask::pending()
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if self.agent.is_some() {
            let _ = std::fs::remove_file(&self.marker);
        }
    }
}

/// How long to keep listening (off-screen) after releasing the display, for
/// the rest of the boot to finish so the recording is complete.
const RECORD_AFTER_RELEASE: Duration = Duration::from_secs(60);

pub struct BootOptions {
    /// Where to save the recorded timeline once the boot finishes.
    pub record: PathBuf,
    /// Keep showing the finished graph this many seconds (0 = until stopped).
    pub hold: f32,
    /// File created once the display is released (on SIGUSR1). Its directory
    /// also holds the marker that keeps the console password agent away.
    pub released_marker: PathBuf,
    /// Follow a shutdown instead of a boot.
    pub shutdown: bool,
    /// Virtual terminal to draw on.
    pub vt: u32,
}

pub fn run(sig: Signals, mut source: Source, graph: GraphArgs, opts: BootOptions) -> Result<()> {
    let BootOptions { record, hold, released_marker, shutdown, vt: vt_number } = opts;
    let shutdown_from = shutdown.then(|| source.begin_shutdown());
    // A shutdown is not a boot recording: never overwrite the saved timeline.
    let replay = shutdown || matches!(source.mode, crate::live::Mode::Replay { .. });
    let mut scene = Scene::new(source, &graph);
    scene.renderer.opts.shutdown_from = shutdown_from;
    let agent_marker = released_marker.with_file_name("agent");

    // At shutdown a compositor holds the display: switching to our VT first
    // makes it let go, which may take a moment. During a boot the VT is only
    // taken once there is a display, so a machine without one keeps its
    // text console.
    let mut early_vt = if shutdown { Vt::enter(vt_number) } else { None };
    let mut search = DisplaySearch::new(shutdown.then_some(Duration::from_secs(30)));
    let mut console: Option<Console> = None;
    // systemd holds its console password agent back until we report ready,
    // so that happens once we have a display (and answer passphrases
    // ourselves) or after a short wait without one (then it answers them).
    let mut ready = false;
    let mut pm = tiny_skia::Pixmap::new(1, 1).context("pixmap")?;
    let frame = Duration::from_millis(33);
    let mut saved = false;
    let mut last_check = Instant::now();
    let mut frames: u64 = 0;
    let mut released_at: Option<Instant> = None;
    // When the display went away without a replacement yet.
    let mut lost_at: Option<Instant> = None;
    // Started in the initrd: hold /sysroot so we can follow the switch to the real root.
    let mut in_initrd = !replay && scene.source.tl.phases.in_initrd;
    let mut sysroot: Option<File> = None;

    while !sig.stopped() {
        let t0 = Instant::now();

        let yielding = console.as_ref().is_some_and(Console::must_yield);
        if released_at.is_none() && (sig.released() || yielding) {
            // Restore the text console's CRTC, drop DRM master, go back to the VT
            // that was in front, then tell bootmap-quit the display is free.
            drop(console.take());
            scene.renderer.prompt = None;
            if let Some(dir) = released_marker.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&released_marker, b"");
            if yielding {
                eprintln!("bootmap: a passphrase is being asked for on the console; getting out of its way");
            }
            eprintln!("bootmap: released display; recording until boot finishes");
            released_at = Some(Instant::now());
        }

        // Find a display if we have none: the first one, or the replacement
        // after a driver swap (simpledrm handing over to the real driver).
        let has_display = console.as_ref().is_some_and(|c| c.display.is_some());
        if !has_display && released_at.is_none() {
            if let Some(d) = search.try_open() {
                let (w, h) = d.size();
                pm = tiny_skia::Pixmap::new(w, h).context("pixmap")?;
                lost_at = None;
                match &mut console {
                    Some(c) => c.display = Some(d),
                    None => {
                        let prompts = match (replay, ready) {
                            (true, _) => Prompts::Ignore,
                            (false, false) => Prompts::Answer,
                            (false, true) => Prompts::Yield,
                        };
                        console = Some(Console::new(d, early_vt.take(), vt_number, prompts, agent_marker.clone()));
                        if shutdown {
                            eprintln!("bootmap: showing shutdown");
                        }
                    }
                }
            }
        }
        if !ready && (shutdown || console.is_some() || search.started.elapsed() >= DisplaySearch::NOTICE) {
            sd_notify("READY=1");
            ready = true;
        }

        match console.as_mut().filter(|c| c.display.is_some()) {
            Some(c) => {
                scene.renderer.prompt = c.prompt();
                scene.frame(&mut pm);
                let res = c.present(&pm);
                if frames == 0 {
                    eprintln!("bootmap: first frame in {:?} ({:?})", t0.elapsed(), res.as_ref().err());
                }
                frames += 1;
                if res.is_err() || (last_check.elapsed() > Duration::from_millis(500) && c.display_gone()) {
                    // The device went away. Let go of it; the search takes the
                    // replacement, and the VT and a half-typed passphrase stay.
                    last_check = Instant::now();
                    c.display = None;
                    lost_at = Some(Instant::now());
                }
            }
            None => {
                // Headless: just keep collecting events.
                scene.source.poll();
            }
        }
        if lost_at.is_some_and(|t| t.elapsed() > Duration::from_secs(10)) {
            // No replacement: give up the VT and the passphrase prompt too,
            // so the text console can ask instead.
            console = None;
            scene.renderer.prompt = None;
            lost_at = None;
        }
        let has_display = console.as_ref().is_some_and(|c| c.display.is_some());

        if let Some(done) = scene.source.finished_for().filter(|_| !shutdown) {
            if !saved && !replay {
                save(&record, &scene);
                saved = true;
            }
            // Done once released or headless (nothing left to show), or after the hold.
            if released_at.is_some() || !has_display || (hold > 0.0 && done >= hold) {
                break;
            }
        }
        if released_at.is_some_and(|t| t.elapsed() > RECORD_AFTER_RELEASE) {
            break;
        }
        if in_initrd && sysroot.is_none() && switch_root_imminent(&scene.source.tl) {
            sysroot = open_sysroot();
            if sysroot.is_some() {
                eprintln!("bootmap: holding /sysroot for the switch to the real root");
            }
        }
        if let crate::live::Mode::Live { disconnected: true, .. } = scene.source.mode {
            // PID 1 went away. In the initrd that is the switch to the real
            // root: follow it and keep drawing on the same output. Anywhere
            // else (shutdown) there is nothing more to show.
            let followed = match (in_initrd, sysroot.take()) {
                (true, Some(fd)) => enter_root(&fd)
                    .inspect_err(|e| eprintln!("bootmap: could not enter the real root: {e}"))
                    .is_ok(),
                (true, None) => {
                    eprintln!("bootmap: lost systemd in the initrd without holding /sysroot; stopping");
                    false
                }
                (false, _) => false,
            };
            if !followed {
                break;
            }
            eprintln!("bootmap: followed systemd into the real root");
            in_initrd = false;
            scene.source.reconnect();
        }
        let pace = if has_display { frame } else { Duration::from_millis(100) };
        std::thread::sleep(pace.saturating_sub(t0.elapsed()));
    }
    // Never write a recording from inside the initrd.
    if !saved && !replay && !in_initrd {
        save(&record, &scene);
    }
    sd_notify("STOPPING=1");
    Ok(())
}
