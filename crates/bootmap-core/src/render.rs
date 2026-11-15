//! Software renderer: Layout + timeline at time `t` -> tiny-skia Pixmap.

use crate::layout::{radius, Layout, ROOT};
use crate::model::{BootTimeline, UnitKind, UnitState, Usec};
use fontdue::{Font, FontSettings};
use std::collections::HashMap;
use std::f32::consts::{PI, TAU};
use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke, Transform,
};

static FONT_BYTES: &[u8] = include_bytes!("../../../assets/DejaVuSans.ttf");

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

const BG: Rgb = Rgb(0x0e, 0x10, 0x16);
const RING: Rgb = Rgb(0x23, 0x27, 0x33);
const GHOST: Rgb = Rgb(0x3a, 0x40, 0x4e);
const TEXT: Rgb = Rgb(0xd8, 0xdc, 0xe6);
const MUTED: Rgb = Rgb(0x8a, 0x91, 0xa3);
const ACTIVATING: Rgb = Rgb(0xf5, 0xb9, 0x42);
const ACTIVE: Rgb = Rgb(0x4c, 0xc3, 0x8a);
const TARGET: Rgb = Rgb(0x6a, 0xa9, 0xff);
const EXITED: Rgb = Rgb(0x5f, 0x8f, 0xa3);
const FAILED: Rgb = Rgb(0xff, 0x5c, 0x5c);
const PROC: Rgb = Rgb(0xc7, 0x9b, 0xff);

fn color(c: Rgb, a: f32) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

fn paint(c: Rgb, a: f32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(color(c, a));
    p.anti_alias = true;
    p
}

/// What a node looks like at a given instant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visual {
    /// Not started yet (or never during this boot).
    Ghost,
    Activating,
    Active,
    /// Ran and exited (oneshot) or was stopped again.
    Exited,
    Failed,
}

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub extra_edges: bool,
    pub labels: bool,
    pub hud: bool,
    /// Title shown in the HUD (defaults to the host name).
    pub title: Option<String>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { extra_edges: true, labels: true, hud: true, title: None }
    }
}

pub struct Renderer {
    font: Font,
    text_cache: HashMap<(String, u32, [u8; 3]), Option<Pixmap>>,
    pub opts: RenderOptions,
}

/// Unit visual states at time `t`.
pub fn visuals_at(tl: &BootTimeline, t: Usec) -> HashMap<&str, Visual> {
    let mut m: HashMap<&str, Visual> =
        tl.units.keys().map(|k| (k.as_str(), Visual::Ghost)).collect();
    for e in tl.events.iter().take_while(|e| e.t <= t) {
        let v = match e.state {
            UnitState::Activating => Visual::Activating,
            UnitState::Active => Visual::Active,
            UnitState::Failed => Visual::Failed,
            UnitState::Inactive | UnitState::Deactivating => match m.get(e.unit.as_str()) {
                Some(Visual::Ghost) | None => Visual::Ghost,
                _ => Visual::Exited,
            },
        };
        m.insert(e.unit.as_str(), v);
    }
    m
}

impl Renderer {
    pub fn new(opts: RenderOptions) -> Self {
        let font = Font::from_bytes(FONT_BYTES, FontSettings::default()).expect("bundled font parses");
        Self { font, text_cache: HashMap::new(), opts }
    }

    /// Rasterize a single line of text into a cached pixmap.
    fn text(&mut self, s: &str, px: f32, c: Rgb) -> Option<&Pixmap> {
        let key = (s.to_string(), (px * 10.0) as u32, [c.0, c.1, c.2]);
        if !self.text_cache.contains_key(&key) {
            let pm = rasterize_text(&self.font, s, px, c);
            if self.text_cache.len() > 4096 {
                self.text_cache.clear();
            }
            self.text_cache.insert(key.clone(), pm);
        }
        self.text_cache.get(&key).and_then(|p| p.as_ref())
    }

    fn draw_text(&mut self, pm: &mut Pixmap, s: &str, x: f32, y: f32, px: f32, c: Rgb) -> f32 {
        let Some(t) = self.text(s, px, c) else { return 0.0 };
        let w = t.width() as f32;
        let t = t.clone();
        pm.draw_pixmap(
            0,
            0,
            t.as_ref(),
            &PixmapPaint::default(),
            Transform::from_translate(x.round(), (y - px).round()),
            None,
        );
        w
    }

    /// Draw text rotated about its anchor (left-middle, or right-middle if `flip`).
    fn draw_text_rotated(&mut self, pm: &mut Pixmap, s: &str, x: f32, y: f32, angle: f32, px: f32, c: Rgb) {
        let Some(t) = self.text(s, px, c) else { return };
        let t = t.clone();
        let (w, h) = (t.width() as f32, t.height() as f32);
        // Keep text upright: flip on the left half of the circle.
        let a = angle.rem_euclid(TAU);
        let flip = a > PI / 2.0 && a < 3.0 * PI / 2.0;
        let (rot, dx) = if flip { (a - PI, -w) } else { (a, 0.0) };
        let tr = Transform::from_translate(x, y)
            .pre_rotate(rot.to_degrees())
            .pre_translate(dx, -h / 2.0);
        let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bilinear, ..Default::default() };
        pm.draw_pixmap(0, 0, t.as_ref(), &paint, tr, None);
    }

    /// Render a frame. `t` is absolute monotonic µs; `wall` is seconds of wall
    /// time used for animation (pulsing).
    pub fn render(&mut self, pm: &mut Pixmap, lay: &Layout, tl: &BootTimeline, t: Usec, wall: f32) {
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        pm.fill(color(BG, 1.0));
        let vis = visuals_at(tl, t);

        // Fit the outermost ring (+ room for labels) in the shorter side.
        let label_room = if self.opts.labels { 1.6 } else { 0.6 };
        let scale = (w.min(h) / 2.0 - 8.0) / (radius(lay.max_depth) + label_room);
        let (cx, cy) = (w / 2.0, h / 2.0);
        let to_px = |r: f32, th: f32| (cx + r * scale * th.cos(), cy + r * scale * th.sin());
        let node_vis = |i: usize| -> Visual {
            if i == ROOT {
                return Visual::Active;
            }
            *vis.get(lay.nodes[i].name.as_str()).unwrap_or(&Visual::Ghost)
        };

        // Depth rings.
        let ring_stroke = Stroke { width: 1.0, ..Default::default() };
        for d in 1..=lay.max_depth {
            if let Some(c) = PathBuilder::from_circle(cx, cy, radius(d) * scale) {
                pm.stroke_path(&c, &paint(RING, 0.6), &ring_stroke, Transform::identity(), None);
            }
        }

        // Non-tree dependency edges: faint straight chords, only between started units.
        if self.opts.extra_edges {
            let mut pb = PathBuilder::new();
            for &(a, b) in &lay.extra_edges {
                if node_vis(a) == Visual::Ghost || node_vis(b) == Visual::Ghost {
                    continue;
                }
                let (na, nb) = (&lay.nodes[a], &lay.nodes[b]);
                let (x0, y0) = to_px(na.r, na.theta);
                let (x1, y1) = to_px(nb.r, nb.theta);
                // Bow the chord towards the center.
                let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
                pb.move_to(x0, y0);
                pb.quad_to(mx + (cx - mx) * 0.35, my + (cy - my) * 0.35, x1, y1);
            }
            if let Some(p) = pb.finish() {
                let s = Stroke { width: 0.6, ..Default::default() };
                pm.stroke_path(&p, &paint(MUTED, 0.07), &s, Transform::identity(), None);
            }
        }

        // Tree edges, grouped by color so each group is one path.
        let mut groups: HashMap<u8, PathBuilder> = HashMap::new();
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            let p = &lay.nodes[n.parent.unwrap_or(ROOT)];
            let key = node_vis(i) as u8;
            let pb = groups.entry(key).or_default();
            let (x0, y0) = to_px(p.r, p.theta);
            let (x1, y1) = to_px(n.r, n.theta);
            let mid = (p.r + n.r) / 2.0;
            let (c1x, c1y) = to_px(mid, p.theta);
            let (c2x, c2y) = to_px(mid, n.theta);
            pb.move_to(x0, y0);
            if p.r == 0.0 {
                pb.quad_to(c2x, c2y, x1, y1);
            } else {
                pb.cubic_to(c1x, c1y, c2x, c2y, x1, y1);
            }
        }
        for (key, pb) in groups {
            let (c, a, wdt) = match key {
                k if k == Visual::Ghost as u8 => (GHOST, 0.35, 0.7),
                k if k == Visual::Activating as u8 => (ACTIVATING, 0.8, 1.4),
                k if k == Visual::Active as u8 => (ACTIVE, 0.45, 1.1),
                k if k == Visual::Exited as u8 => (EXITED, 0.4, 1.0),
                _ => (FAILED, 0.8, 1.4),
            };
            if let Some(p) = pb.finish() {
                let s = Stroke { width: wdt, ..Default::default() };
                pm.stroke_path(&p, &paint(c, a), &s, Transform::identity(), None);
            }
        }

        // Processes: tiny satellites on started units.
        for p in &lay.procs {
            if !matches!(node_vis(p.unit), Visual::Active | Visual::Activating) {
                continue;
            }
            let n = &lay.nodes[p.unit];
            let (x0, y0) = to_px(n.r, n.theta);
            let (x, y) = to_px(p.r, p.theta);
            let mut pb = PathBuilder::new();
            pb.move_to(x0, y0);
            pb.line_to(x, y);
            if let Some(l) = pb.finish() {
                let s = Stroke { width: 0.6, ..Default::default() };
                pm.stroke_path(&l, &paint(PROC, 0.35), &s, Transform::identity(), None);
            }
            if let Some(c) = PathBuilder::from_circle(x, y, 1.8) {
                pm.fill_path(&c, &paint(PROC, 0.85), FillRule::Winding, Transform::identity(), None);
            }
        }

        // Nodes.
        let pulse = 0.5 + 0.5 * (wall * TAU * 1.2).sin();
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            let v = node_vis(i);
            let (x, y) = to_px(n.r, n.theta);
            let is_target = n.kind == Some(UnitKind::Target);
            let base = if is_target { 4.5 } else { 2.8 };
            let (c, a, rad) = match v {
                Visual::Ghost => (GHOST, 0.8, base * 0.6),
                Visual::Activating => (ACTIVATING, 1.0, base * (1.0 + 0.35 * pulse)),
                Visual::Active if is_target => (TARGET, 1.0, base),
                Visual::Active => (ACTIVE, 1.0, base),
                Visual::Exited => (EXITED, 0.9, base * 0.85),
                Visual::Failed => (FAILED, 1.0, base * 1.2),
            };
            if v == Visual::Activating {
                if let Some(halo) = PathBuilder::from_circle(x, y, rad * 2.6) {
                    pm.fill_path(&halo, &paint(c, 0.18 * (0.4 + pulse)), FillRule::Winding, Transform::identity(), None);
                }
            }
            if let Some(dot) = PathBuilder::from_circle(x, y, rad) {
                if v == Visual::Exited {
                    let s = Stroke { width: 1.2, ..Default::default() };
                    pm.stroke_path(&dot, &paint(c, a), &s, Transform::identity(), None);
                } else {
                    pm.fill_path(&dot, &paint(c, a), FillRule::Winding, Transform::identity(), None);
                }
            }
        }

        // Center node.
        let core_r = (scale * 0.22).clamp(6.0, 26.0);
        if let Some(c) = PathBuilder::from_circle(cx, cy, core_r * 1.8) {
            pm.fill_path(&c, &paint(TARGET, 0.10), FillRule::Winding, Transform::identity(), None);
        }
        if let Some(c) = PathBuilder::from_circle(cx, cy, core_r) {
            pm.fill_path(&c, &paint(TEXT, 0.95), FillRule::Winding, Transform::identity(), None);
        }

        // Radial labels where there is room.
        if self.opts.labels {
            let px = (scale * 0.13).clamp(9.0, 13.0);
            for (i, n) in lay.nodes.iter().enumerate().skip(1) {
                let v = node_vis(i);
                if v == Visual::Ghost {
                    continue;
                }
                let arc = (n.wedge.1 - n.wedge.0) * n.r * scale;
                let is_target = n.kind == Some(UnitKind::Target);
                let room = arc >= px * 0.95 || (is_target && arc >= px * 0.6) || v == Visual::Activating;
                let leaf = n.children.is_empty();
                if !room || !(leaf || is_target || v != Visual::Active) {
                    continue;
                }
                let c = match v {
                    Visual::Activating => ACTIVATING,
                    Visual::Failed => FAILED,
                    _ if is_target => TARGET,
                    Visual::Exited => MUTED,
                    _ => TEXT,
                };
                let name = short_name(&n.name);
                let off = if leaf { 0.12 } else { 0.0 };
                let (x, y) = to_px(n.r + off + 0.06, n.theta);
                if leaf {
                    self.draw_text_rotated(pm, &name, x, y, n.theta, px, c);
                } else {
                    // Interior nodes: horizontal label beside the dot.
                    self.draw_text(pm, &name, x + 6.0, y + px * 0.35, px, c);
                }
            }
        }

        if self.opts.hud {
            self.hud(pm, lay, tl, t, &vis);
        }
    }

    fn hud(&mut self, pm: &mut Pixmap, lay: &Layout, tl: &BootTimeline, t: Usec, vis: &HashMap<&str, Visual>) {
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        let big = (h / 40.0).clamp(14.0, 28.0);
        let small = (big * 0.62).max(10.0);
        let line = small * 1.45;
        let x = 18.0;
        let mut y = 18.0 + big;

        let title = self.opts.title.clone().unwrap_or_else(|| {
            if tl.host.is_empty() { "bootmap".into() } else { format!("bootmap · {}", tl.host) }
        });
        self.draw_text(pm, &title, x, y, big, TEXT);
        y += big * 1.25;
        let (start, end) = tl.span();
        let rel = t.saturating_sub(start) as f32 / 1e6;
        let finished = tl.phases.finish > 0 && t >= tl.phases.finish;
        let status = if finished {
            format!("userspace +{:.3}s · boot finished", (tl.phases.finish - start) as f32 / 1e6)
        } else {
            format!("userspace +{rel:.3}s")
        };
        self.draw_text(pm, &status, x, y, small * 1.15, if finished { ACTIVE } else { ACTIVATING });
        y += line * 1.2;

        let count = |want: Visual| lay.nodes.iter().skip(1).filter(|n| vis.get(n.name.as_str()) == Some(&want)).count();
        let (n_act, n_ing, n_ex, n_fail) =
            (count(Visual::Active), count(Visual::Activating), count(Visual::Exited), count(Visual::Failed));
        let total = lay.nodes.len() - 1;
        for (label, n, c) in [
            ("active", n_act, ACTIVE),
            ("activating", n_ing, ACTIVATING),
            ("exited", n_ex, EXITED),
            ("failed", n_fail, FAILED),
            ("pending", total - n_act - n_ing - n_ex - n_fail, GHOST),
        ] {
            if let Some(d) = PathBuilder::from_circle(x + 4.0, y - small * 0.35, 4.0) {
                pm.fill_path(&d, &paint(c, 1.0), FillRule::Winding, Transform::identity(), None);
            }
            self.draw_text(pm, &format!("{n:>4}  {label}"), x + 14.0, y, small, TEXT);
            y += line;
        }

        // In-flight jobs, or the slowest units once finished.
        y += line * 0.5;
        if !finished && n_ing > 0 {
            self.draw_text(pm, "starting", x, y, small, MUTED);
            y += line;
            let mut ing: Vec<(&str, Usec)> = lay
                .nodes
                .iter()
                .skip(1)
                .filter(|n| vis.get(n.name.as_str()) == Some(&Visual::Activating))
                .filter_map(|n| Some((n.name.as_str(), tl.units.get(&n.name)?.t_activating?)))
                .collect();
            ing.sort_by_key(|(_, a)| *a);
            for (name, a) in ing.iter().take(8) {
                let s = format!("{:>6.2}s  {}", t.saturating_sub(*a) as f32 / 1e6, short_name(name));
                self.draw_text(pm, &s, x, y, small, ACTIVATING);
                y += line;
            }
        } else if finished || t >= end {
            self.draw_text(pm, "slowest", x, y, small, MUTED);
            y += line;
            let mut slow: Vec<(&str, Usec)> = lay
                .nodes
                .iter()
                .skip(1)
                .filter_map(|n| Some((n.name.as_str(), tl.units.get(&n.name)?.duration()?)))
                .filter(|(_, d)| *d > 0)
                .collect();
            slow.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
            for (name, d) in slow.iter().take(8) {
                let s = format!("{:>6.3}s  {}", *d as f32 / 1e6, short_name(name));
                self.draw_text(pm, &s, x, y, small, TEXT);
                y += line;
            }
        }

        // Phase bar along the bottom.
        let ph = &tl.phases;
        let mut parts = vec![];
        if ph.firmware > ph.loader && ph.loader > 0 {
            parts.push(format!("firmware {:.1}s", (ph.firmware - ph.loader) as f32 / 1e6));
        }
        if ph.loader > 0 {
            parts.push(format!("loader {:.1}s", ph.loader as f32 / 1e6));
        }
        if ph.initrd > 0 {
            parts.push(format!("kernel {:.2}s", ph.initrd as f32 / 1e6));
            parts.push(format!("initrd {:.2}s", ph.userspace.saturating_sub(ph.initrd) as f32 / 1e6));
        } else if ph.userspace > 0 {
            parts.push(format!("kernel {:.2}s", ph.userspace as f32 / 1e6));
        }
        if ph.finish > 0 {
            parts.push(format!("userspace {:.2}s", ph.finish.saturating_sub(ph.userspace) as f32 / 1e6));
        }
        let footer = parts.join("   ·   ");
        self.draw_text(pm, &footer, x, h - 16.0, small, MUTED);

        // Timeline scrubber.
        if end > start {
            let bar_y = h - 16.0 - small - 10.0;
            let bar_w = (w * 0.3).min(360.0);
            let f = (t.saturating_sub(start) as f32 / (end - start) as f32).clamp(0.0, 1.0);
            if let Some(r) = Rect::from_xywh(x, bar_y, bar_w, 3.0) {
                pm.fill_rect(r, &paint(RING, 1.0), Transform::identity(), None);
            }
            if let Some(r) = Rect::from_xywh(x, bar_y, (bar_w * f).max(1.0), 3.0) {
                pm.fill_rect(r, &paint(if finished { ACTIVE } else { ACTIVATING }, 1.0), Transform::identity(), None);
            }
        }

        // Legend, top right.
        let mut ly = 18.0 + small;
        for (label, c) in [("target", TARGET), ("active", ACTIVE), ("activating", ACTIVATING), ("exited", EXITED), ("failed", FAILED), ("process", PROC)] {
            let tw = self.text(label, small, MUTED).map(|p| p.width() as f32).unwrap_or(0.0);
            let lx = w - 18.0 - tw;
            self.draw_text(pm, label, lx, ly, small, MUTED);
            if let Some(d) = PathBuilder::from_circle(lx - 10.0, ly - small * 0.35, 3.5) {
                pm.fill_path(&d, &paint(c, 1.0), FillRule::Winding, Transform::identity(), None);
            }
            ly += line;
        }
    }
}

/// Drop the `.service` suffix (other kinds keep theirs, they carry meaning).
pub fn short_name(name: &str) -> String {
    let n = name.strip_suffix(".service").unwrap_or(name);
    if n.chars().count() > 38 {
        let mut s: String = n.chars().take(36).collect();
        s.push('…');
        s
    } else {
        n.to_string()
    }
}

fn rasterize_text(font: &Font, s: &str, px: f32, c: Rgb) -> Option<Pixmap> {
    let lm = font.horizontal_line_metrics(px)?;
    let ascent = lm.ascent.ceil();
    let height = (lm.ascent - lm.descent).ceil() as i32 + 1;
    let glyphs: Vec<_> = s.chars().map(|ch| font.rasterize(ch, px)).collect();
    let width = glyphs.iter().map(|(m, _)| m.advance_width).sum::<f32>().ceil() as i32 + 2;
    if width <= 2 || height <= 0 {
        return None;
    }
    let mut pm = Pixmap::new(width as u32, height as u32)?;
    let stride = width as usize;
    let data = pm.data_mut();
    let mut pen = 0.0f32;
    for (m, bmp) in &glyphs {
        let gx = (pen + m.xmin as f32).round() as i32;
        let gy = (ascent - m.height as f32 - m.ymin as f32).round() as i32;
        for row in 0..m.height {
            for col in 0..m.width {
                let cov = bmp[row * m.width + col] as u32;
                let (px_x, px_y) = (gx + col as i32, gy + row as i32);
                if cov == 0 || px_x < 0 || px_y < 0 || px_x >= width || px_y >= height {
                    continue;
                }
                let o = (px_y as usize * stride + px_x as usize) * 4;
                let a = cov.max(data[o + 3] as u32);
                data[o] = (c.0 as u32 * a / 255) as u8;
                data[o + 1] = (c.1 as u32 * a / 255) as u8;
                data[o + 2] = (c.2 as u32 * a / 255) as u8;
                data[o + 3] = a as u8;
            }
        }
        pen += m.advance_width;
    }
    Some(pm)
}
