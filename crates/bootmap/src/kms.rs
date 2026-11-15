//! Boot-time backend: draw straight to the display with KMS dumb buffers.
//!
//! No compositor exists this early, so we open /dev/dri/cardN, become DRM
//! master, point a CRTC at our own framebuffer and copy each rendered frame
//! into it. Early in boot the only device may be simpledrm, which disappears
//! when the real GPU driver loads; in that case we reopen.

use crate::live::{Scene, Source};
use crate::GraphArgs;
use anyhow::{bail, Context, Result};
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{connector, crtc, dumbbuffer::DumbBuffer, framebuffer, Device as ControlDevice, Mode};
use drm::Device;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Card(File, PathBuf);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

struct Output {
    card: Card,
    conn: connector::Handle,
    crtc: crtc::Handle,
    saved: crtc::Info,
    db: Option<DumbBuffer>,
    fb: framebuffer::Handle,
    size: (u32, u32),
    dirty_warned: bool,
}

impl Output {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let card = Card(file, path.to_path_buf());
        let _ = card.acquire_master_lock();
        let res = card.resource_handles().context("not a KMS device")?;
        let (conn, mode) = res
            .connectors()
            .iter()
            .filter_map(|&c| card.get_connector(c, true).ok())
            .filter(|i| i.state() == connector::State::Connected && !i.modes().is_empty())
            .map(|i| (i.handle(), preferred_mode(i.modes())))
            .next()
            .context("no connected display")?;
        let info = card.get_connector(conn, false)?;
        let crtc = info
            .current_encoder()
            .and_then(|e| card.get_encoder(e).ok())
            .and_then(|e| e.crtc())
            .or_else(|| {
                info.encoders().iter().filter_map(|&e| card.get_encoder(e).ok()).find_map(|e| {
                    res.filter_crtcs(e.possible_crtcs()).into_iter().next()
                })
            })
            .context("no usable CRTC")?;
        let saved = card.get_crtc(crtc)?;
        let (w, h) = mode.size();
        let size = (w as u32, h as u32);
        let db = card.create_dumb_buffer(size, DrmFourcc::Xrgb8888, 32)?;
        let fb = card.add_framebuffer(&db, 24, 32)?;
        card.set_crtc(crtc, Some(fb), (0, 0), &[conn], Some(mode)).context("set_crtc")?;
        eprintln!("bootmap: drawing on {} at {}x{}", path.display(), size.0, size.1);
        Ok(Self { card, conn, crtc, saved, db: Some(db), fb, size, dirty_warned: false })
    }

    fn present(&mut self, pm: &tiny_skia::Pixmap) -> Result<()> {
        let db = self.db.as_mut().context("buffer released")?;
        let pitch = db.pitch() as usize;
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        {
            let mut map = self.card.map_dumb_buffer(db)?;
            let dst = map.as_mut();
            let src = pm.data();
            for y in 0..h {
                let s = &src[y * w * 4..(y + 1) * w * 4];
                let d = &mut dst[y * pitch..y * pitch + w * 4];
                for (dp, sp) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                    // RGBA (opaque) -> XRGB8888 little endian = B, G, R, X
                    dp[0] = sp[2];
                    dp[1] = sp[1];
                    dp[2] = sp[0];
                    dp[3] = 0xff;
                }
            }
        }

        // One full-screen clip: an empty list is rejected with EINVAL
        // (the kernel wants num_clips and clips_ptr both zero or both set).
        let full = drm::control::ClipRect::new(0, 0, self.size.0.min(u16::MAX as u32) as u16, self.size.1.min(u16::MAX as u32) as u16);
        if let Err(e) = self.card.dirty_framebuffer(self.fb, &[full]) {
            if !self.dirty_warned {
                eprintln!("bootmap: dirty_framebuffer: {e}");
                self.dirty_warned = true;
            }
        }
        Ok(())
    }

    fn gone(&self) -> bool {
        !self.card.1.exists()
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.card.set_crtc(
            self.crtc,
            self.saved.framebuffer(),
            self.saved.position(),
            &[self.conn],
            self.saved.mode(),
        );
        let _ = self.card.destroy_framebuffer(self.fb);
        if let Some(db) = self.db.take() {
            let _ = self.card.destroy_dumb_buffer(db);
        }
        let _ = self.card.release_master_lock();
    }
}


fn preferred_mode(modes: &[Mode]) -> Mode {
    modes
        .iter()
        .find(|m| m.mode_type().contains(drm::control::ModeTypeFlags::PREFERRED))
        .copied()
        .unwrap_or(modes[0])
}

/// Try every card, preferring ones that are not simpledrm.
fn open_any(deadline: Instant) -> Result<Output> {
    loop {
        let mut cards: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("card")))
                    .collect()
            })
            .unwrap_or_default();
        cards.sort_by_key(|p| (is_simpledrm(p), p.file_name().unwrap_or_default().len(), p.clone()));
        for c in &cards {
            match Output::open(c) {
                Ok(o) => return Ok(o),
                Err(e) => eprintln!("bootmap: {}: {e:#}", c.display()),
            }
        }
        if Instant::now() > deadline {
            bail!("no usable display found");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_simpledrm(card: &Path) -> bool {
    let name = card.file_name().unwrap_or_default().to_string_lossy().to_string();
    std::fs::read_link(format!("/sys/class/drm/{name}/device/driver"))
        .map(|d| d.to_string_lossy().contains("simple"))
        .unwrap_or(false)
}

/// Put the active VT in graphics mode so the console cursor/text stays hidden.
struct VtGraphics(Option<File>);
const KDSETMODE: u64 = 0x4B3A;
extern "C" {
    fn ioctl(fd: i32, req: u64, ...) -> i32;
}
impl VtGraphics {
    fn enter() -> Self {
        let f = OpenOptions::new().read(true).write(true).open("/dev/tty0").ok();
        if let Some(f) = &f {
            // SAFETY: KDSETMODE takes an int argument.
            unsafe { ioctl(f.as_raw_fd(), KDSETMODE, 1i32) };
        }
        Self(f)
    }
}
impl Drop for VtGraphics {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            // SAFETY: as above; 0 = KD_TEXT.
            unsafe { ioctl(f.as_raw_fd(), KDSETMODE, 0i32) };
        }
    }
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

pub fn run(source: Source, graph: GraphArgs, record: PathBuf, hold: f32) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(sig, stop.clone())?;
    }
    let replay = matches!(source.mode, crate::live::Mode::Replay { .. });
    let mut scene = Scene::new(source, &graph);
    let _vt = VtGraphics::enter();
    let mut out = open_any(Instant::now() + Duration::from_secs(20))?;
    sd_notify("READY=1");
    let mut pm = tiny_skia::Pixmap::new(out.size.0, out.size.1).context("pixmap")?;
    let frame = Duration::from_millis(33);
    let mut saved = false;
    let mut last_check = Instant::now();
    let mut frames: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        scene.frame(&mut pm);
        let res = out.present(&pm);
        if frames == 0 {
            eprintln!("bootmap: first frame in {:?} ({:?})", t0.elapsed(), res.as_ref().err());
        }
        frames += 1;
        let failed = res.is_err();
        if failed || (last_check.elapsed() > Duration::from_millis(500) && out.gone()) {
            // The device went away (simpledrm handed over to the real driver).
            last_check = Instant::now();
            drop(out);
            out = open_any(Instant::now() + Duration::from_secs(10))?;
            pm = tiny_skia::Pixmap::new(out.size.0, out.size.1).context("pixmap")?;
            continue;
        }
        if let Some(done) = scene.source.finished_for() {
            if !saved && !replay {
                save(&record, &scene);
                saved = true;
            }
            if hold > 0.0 && done >= hold {
                break;
            }
        }
        if let crate::live::Mode::Live { disconnected: true, .. } = scene.source.mode {
            // PID 1 went away (shutdown/switch-root); nothing more to show.
            break;
        }
        std::thread::sleep(frame.saturating_sub(t0.elapsed()));
    }
    if !saved && !replay {
        save(&record, &scene);
    }
    sd_notify("STOPPING=1");
    Ok(())
}
