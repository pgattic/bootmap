//! The display: every connected monitor, drawn on with KMS dumb buffers.
//!
//! No compositor exists this early, so we open /dev/dri/cardN, become DRM
//! master, and point a CRTC at a framebuffer of our own for each connected
//! output. The picture is rendered once, at the primary output's size (the
//! built-in panel if there is one), and scaled to fit any other output.
//!
//! Early in boot the only device may be simpledrm, which disappears when the
//! real GPU driver loads; `gone()` reports that so the caller can reopen.

use anyhow::{Context, Result};
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{connector, crtc, dumbbuffer::DumbBuffer, framebuffer, Device as ControlDevice, Mode};
use drm::Device;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

struct Card {
    file: File,
    path: PathBuf,
}
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}
impl Drop for Card {
    fn drop(&mut self) {
        let _ = self.release_master_lock();
    }
}

/// One monitor.
struct Head {
    card: Rc<Card>,
    name: String,
    internal: bool,
    conn: connector::Handle,
    crtc: crtc::Handle,
    saved: crtc::Info,
    db: Option<DumbBuffer>,
    fb: framebuffer::Handle,
    size: (u32, u32),
    /// Physical size in millimetres, as the display reports it (0 if unknown).
    mm: (u32, u32),
    /// The picture scaled to this head, when its size differs from the primary's.
    scaled: Option<Pixmap>,
    dirty_warned: bool,
}

impl Head {
    fn open(card: &Rc<Card>, info: &connector::Info, crtc: crtc::Handle) -> Result<Self> {
        let mode = preferred_mode(info.modes());
        let saved = card.get_crtc(crtc)?;
        let (w, h) = mode.size();
        let size = (w as u32, h as u32);
        let db = card.create_dumb_buffer(size, DrmFourcc::Xrgb8888, 32)?;
        let fb = match card.add_framebuffer(&db, 24, 32) {
            Ok(fb) => fb,
            Err(e) => {
                let _ = card.destroy_dumb_buffer(db);
                return Err(e).context("add_framebuffer");
            }
        };
        if let Err(e) = card.set_crtc(crtc, Some(fb), (0, 0), &[info.handle()], Some(mode)) {
            let _ = card.destroy_framebuffer(fb);
            let _ = card.destroy_dumb_buffer(db);
            return Err(e).context("set_crtc");
        }
        Ok(Self {
            card: card.clone(),
            name: format!("{}-{}", info.interface().as_str(), info.interface_id()),
            internal: matches!(
                info.interface(),
                connector::Interface::EmbeddedDisplayPort | connector::Interface::LVDS | connector::Interface::DSI
            ),
            conn: info.handle(),
            crtc,
            saved,
            db: Some(db),
            fb,
            size,
            mm: info.size().unwrap_or((0, 0)),
            scaled: None,
            dirty_warned: false,
        })
    }

    /// Copy `pm` (which must be this head's size) into the framebuffer.
    fn blit(&mut self, pm: &Pixmap) -> Result<()> {
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
            // ENOSYS: the driver scans out the buffer directly and needs no flush.
            if !self.dirty_warned && e.raw_os_error() != Some(38) {
                eprintln!("bootmap: dirty_framebuffer: {e}");
                self.dirty_warned = true;
            }
        }
        Ok(())
    }

    /// Show `pm`, scaled to fit and centered if it is not this head's size.
    fn present(&mut self, pm: &Pixmap) -> Result<()> {
        if (pm.width(), pm.height()) == self.size {
            return self.blit(pm);
        }
        let (w, h) = self.size;
        let mut scaled = match self.scaled.take() {
            Some(s) => s,
            None => Pixmap::new(w, h).context("pixmap")?,
        };
        let s = (w as f32 / pm.width() as f32).min(h as f32 / pm.height() as f32);
        let (dx, dy) = ((w as f32 - pm.width() as f32 * s) / 2.0, (h as f32 - pm.height() as f32 * s) / 2.0);
        scaled.fill(bootmap_core::render::background());
        let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() };
        scaled.draw_pixmap(0, 0, pm.as_ref(), &paint, Transform::from_scale(s, s).post_translate(dx, dy), None);
        let res = self.blit(&scaled);
        self.scaled = Some(scaled);
        res
    }
}

impl Drop for Head {
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
    }
}

fn preferred_mode(modes: &[Mode]) -> Mode {
    modes
        .iter()
        .find(|m| m.mode_type().contains(drm::control::ModeTypeFlags::PREFERRED))
        .copied()
        .unwrap_or(modes[0])
}

/// Take every connected output of one card. `Err` if it has none we can use.
fn open_card(path: &Path, problems: &mut Vec<String>) -> Result<Vec<Head>> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let card = Rc::new(Card { file, path: path.to_path_buf() });
    let _ = card.acquire_master_lock();
    let res = card.resource_handles().context("not a KMS device")?;
    let mut taken: Vec<crtc::Handle> = vec![];
    let mut heads = vec![];
    for &c in res.connectors() {
        let Ok(info) = card.get_connector(c, true) else { continue };
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            continue;
        }
        // Each output needs a CRTC to itself: the one it already uses if
        // that is free, else any its encoders can drive.
        let current = info.current_encoder().and_then(|e| card.get_encoder(e).ok()).and_then(|e| e.crtc());
        let possible = info
            .encoders()
            .iter()
            .filter_map(|&e| card.get_encoder(e).ok())
            .flat_map(|e| res.filter_crtcs(e.possible_crtcs()));
        let Some(crtc) = current.into_iter().chain(possible).find(|c| !taken.contains(c)) else {
            problems.push(format!("{}: no free CRTC for {:?}-{}", path.display(), info.interface(), info.interface_id()));
            continue;
        };
        match Head::open(&card, &info, crtc) {
            Ok(h) => {
                taken.push(crtc);
                heads.push(h);
            }
            Err(e) => problems.push(format!("{}: {}-{}: {e:#}", path.display(), info.interface().as_str(), info.interface_id())),
        }
    }
    if heads.is_empty() {
        anyhow::bail!("no connected display");
    }
    Ok(heads)
}

fn is_simpledrm(card: &Path) -> bool {
    let name = card.file_name().unwrap_or_default().to_string_lossy().to_string();
    std::fs::read_link(format!("/sys/class/drm/{name}/device/driver"))
        .map(|d| d.to_string_lossy().contains("simple"))
        .unwrap_or(false)
}

pub struct Display {
    /// `heads[0]` is the primary: the picture is rendered at its size.
    heads: Vec<Head>,
}

impl Display {
    /// Take every connected output there is. Reasons for outputs or cards
    /// that could not be used are appended to `problems`; `None` means there
    /// is nothing to draw on (yet).
    ///
    /// simpledrm is the firmware's framebuffer for a panel that a real driver
    /// will drive too, so it is only used when no real driver has an output.
    pub fn open(problems: &mut Vec<String>) -> Option<Self> {
        let mut cards: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("card")))
                    .collect()
            })
            .unwrap_or_default();
        cards.sort_by_key(|p| (p.file_name().unwrap_or_default().len(), p.clone()));
        let (simple, real): (Vec<_>, Vec<_>) = cards.into_iter().partition(|p| is_simpledrm(p));

        let mut heads = vec![];
        for c in &real {
            match open_card(c, problems) {
                Ok(h) => heads.extend(h),
                Err(e) => problems.push(format!("{}: {e:#}", c.display())),
            }
        }
        for c in &simple {
            if !heads.is_empty() {
                break;
            }
            match open_card(c, problems) {
                Ok(h) => heads.extend(h),
                Err(e) => problems.push(format!("{}: {e:#}", c.display())),
            }
        }
        if heads.is_empty() {
            return None;
        }
        // Primary first: the built-in panel, else the largest.
        heads.sort_by_key(|h| (!h.internal, std::cmp::Reverse(h.size.0 as u64 * h.size.1 as u64)));
        let p = &heads[0];
        eprintln!("bootmap: drawing on {} at {}x{} ({}, {}x{} mm)", p.card.path.display(), p.size.0, p.size.1, p.name, p.mm.0, p.mm.1);
        for h in &heads[1..] {
            eprintln!("bootmap: mirroring to {} at {}x{} on {}", h.name, h.size.0, h.size.1, h.card.path.display());
        }
        Some(Self { heads })
    }

    /// Device pixels per layout pixel for the primary output: its density
    /// relative to a ~96 dpi screen. Displays that report no physical size
    /// (VMs, some projectors) fall back to their height relative to 1080.
    pub fn ui_scale(&self) -> f32 {
        let p = &self.heads[0];
        let scale = if p.mm.0 > 0 && p.mm.1 > 0 {
            let dpi = p.size.0 as f32 / (p.mm.0 as f32 / 25.4);
            dpi / 96.0
        } else {
            p.size.1 as f32 / 1080.0
        };
        scale.clamp(1.0, 4.0)
    }

    /// Size to render at.
    pub fn size(&self) -> (u32, u32) {
        self.heads[0].size
    }

    /// Show `pm` on every output. Fails only if the primary does.
    pub fn present(&mut self, pm: &Pixmap) -> Result<()> {
        let mut heads = self.heads.iter_mut();
        let primary = heads.next().context("no outputs")?;
        primary.present(pm)?;
        for h in heads {
            let _ = h.present(pm);
        }
        Ok(())
    }

    /// A card node vanished while its directory is still there. If
    /// `/dev/dri` itself is missing (`/dev` moved away in the switch to the
    /// real root, before we chroot), the device was not replaced.
    pub fn gone(&self) -> bool {
        self.heads.iter().any(|h| !h.card.path.exists() && h.card.path.parent().is_some_and(|d| d.exists()))
    }
}
