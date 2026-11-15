//! Software renderer: Layout + timeline at time `t` -> tiny-skia Pixmap.

use crate::layout::{radius, Layout, ROOT};
use crate::model::{BootTimeline, UnitKind, UnitState, Usec};
use fontdue::{Font, FontSettings};
use std::collections::{HashMap, HashSet};
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
    /// Being stopped (shutdown).
    Stopping,
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Rigid radial tree (radius = dependency depth).
    Radial,
    /// Force-directed with a soft pull toward the depth rings.
    Force,
}

/// Maps world units (one ring = 1.0) to pixels.
#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// World point shown at the center of the screen.
    pub cx: f32,
    pub cy: f32,
    /// Pixels per world unit.
    pub scale: f32,
}

impl Camera {
    pub fn to_px(&self, w: f32, h: f32, x: f32, y: f32) -> (f32, f32) {
        (w / 2.0 + (x - self.cx) * self.scale, h / 2.0 + (y - self.cy) * self.scale)
    }

    pub fn to_world(&self, w: f32, h: f32, px: f32, py: f32) -> (f32, f32) {
        (self.cx + (px - w / 2.0) / self.scale, self.cy + (py - h / 2.0) / self.scale)
    }

    /// Fit `points` (world) into a `w`×`h` screen with `margin` px around them.
    pub fn fit(points: &[(f32, f32)], w: f32, h: f32, margin: f32) -> Self {
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in points {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        if x0 > x1 {
            return Self { cx: 0.0, cy: 0.0, scale: 50.0 };
        }
        let bw = (x1 - x0).max(1.0);
        let bh = (y1 - y0).max(1.0);
        let scale = ((w - 2.0 * margin).max(50.0) / bw).min((h - 2.0 * margin).max(50.0) / bh);
        Self { cx: (x0 + x1) / 2.0, cy: (y0 + y1) / 2.0, scale }
    }

    /// Fit symmetric around the origin (keeps the center node centered).
    pub fn fit_centered(points: &[(f32, f32)], w: f32, h: f32, margin: f32) -> Self {
        let ext = points.iter().fold(1.0f32, |m, &(x, y)| m.max(x.abs()).max(y.abs()));
        let scale = ((w.min(h) / 2.0 - margin).max(25.0)) / ext;
        Self { cx: 0.0, cy: 0.0, scale }
    }

    /// Smoothly move toward `o` (scale interpolated geometrically).
    pub fn lerp(self, o: Camera, f: f32) -> Camera {
        Camera {
            cx: self.cx + (o.cx - self.cx) * f,
            cy: self.cy + (o.cy - self.cy) * f,
            scale: self.scale * (o.scale / self.scale).powf(f),
        }
    }

    /// Zoom by `factor` keeping the world point under pixel (px, py) fixed.
    pub fn zoom_at(&mut self, w: f32, h: f32, px: f32, py: f32, factor: f32) {
        let (wx, wy) = self.to_world(w, h, px, py);
        self.scale = (self.scale * factor).clamp(2.0, 5000.0);
        let (nx, ny) = self.to_world(w, h, px, py);
        self.cx += wx - nx;
        self.cy += wy - ny;
    }
}

/// Everything that varies per frame besides the timeline.
pub struct Frame<'a> {
    pub lay: &'a Layout,
    /// World position of each layout node.
    pub pos: &'a [(f32, f32)],
    pub style: Style,
    pub cam: Camera,
    pub hover: Option<usize>,
    /// Opacity of the depth-ring guides, 0..=1.
    pub rings: f32,
}

impl Frame<'_> {
    /// Index of the node under pixel (px, py), if any.
    pub fn pick(&self, w: f32, h: f32, px: f32, py: f32) -> Option<usize> {
        let mut best: Option<(f32, usize)> = None;
        for (i, &(x, y)) in self.pos.iter().enumerate().skip(1) {
            let (sx, sy) = self.cam.to_px(w, h, x, y);
            let d2 = (sx - px).powi(2) + (sy - py).powi(2);
            if d2 < 14.0 * 14.0 && best.is_none_or(|(b, _)| d2 < b) {
                best = Some((d2, i));
            }
        }
        best.map(|(_, i)| i)
    }
}

fn polar(x: f32, y: f32) -> (f32, f32) {
    ((x * x + y * y).sqrt(), y.atan2(x))
}

fn label_color(v: Visual, is_target: bool) -> Rgb {
    match v {
        Visual::Activating | Visual::Stopping => ACTIVATING,
        Visual::Failed => FAILED,
        _ if is_target => TARGET,
        Visual::Exited | Visual::Ghost => MUTED,
        _ => TEXT,
    }
}

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub extra_edges: bool,
    pub labels: bool,
    pub hud: bool,
    /// Title shown in the HUD (defaults to the host name).
    pub title: Option<String>,
    /// Shutdown view: when the shutdown began. Units that stop after this
    /// fade out instead of showing as "exited", and the HUD counts down.
    pub shutdown_from: Option<Usec>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { extra_edges: true, labels: true, hud: true, title: None, shutdown_from: None }
    }
}

pub struct Renderer {
    font: Font,
    text_cache: HashMap<(String, u32, [u8; 3]), Option<Pixmap>>,
    pub opts: RenderOptions,
}

/// Unit visual states at time `t`. With `shutdown_from`, units that stop at
/// or after that time fade back to `Ghost` instead of showing as `Exited`.
pub fn visuals_at(tl: &BootTimeline, t: Usec, shutdown_from: Option<Usec>) -> HashMap<&str, Visual> {
    let mut m: HashMap<&str, Visual> =
        tl.units.keys().map(|k| (k.as_str(), Visual::Ghost)).collect();
    for e in tl.events.iter().take_while(|e| e.t <= t) {
        let started = !matches!(m.get(e.unit.as_str()), Some(Visual::Ghost) | None);
        let v = match e.state {
            UnitState::Activating => Visual::Activating,
            UnitState::Active => Visual::Active,
            UnitState::Failed => Visual::Failed,
            _ if !started => Visual::Ghost,
            UnitState::Deactivating => Visual::Stopping,
            _ if shutdown_from.is_some_and(|s| e.t >= s) => Visual::Ghost,
            _ => Visual::Exited,
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
    pub fn render(&mut self, pm: &mut Pixmap, f: &Frame, tl: &BootTimeline, t: Usec, wall: f32) {
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        pm.fill(color(BG, 1.0));
        let vis = visuals_at(tl, t, self.opts.shutdown_from);
        let lay = f.lay;
        let cam = f.cam;
        let px = |i: usize| -> (f32, f32) {
            let (x, y) = f.pos.get(i).copied().unwrap_or((0.0, 0.0));
            cam.to_px(w, h, x, y)
        };
        let node_vis = |i: usize| -> Visual {
            if i == ROOT {
                return Visual::Active;
            }
            *vis.get(lay.nodes[i].name.as_str()).unwrap_or(&Visual::Ghost)
        };
        // Hover focus: the hovered node and its neighbors stay bright.
        let focus: Option<HashSet<usize>> = f.hover.map(|h| {
            let mut s: HashSet<usize> = lay.adj.get(h).map(|a| a.iter().copied().collect()).unwrap_or_default();
            s.insert(h);
            s
        });
        let dim = |i: usize| -> f32 {
            match &focus {
                Some(s) if !s.contains(&i) => 0.18,
                _ => 1.0,
            }
        };
        let edge_focus = |a: usize, b: usize| -> f32 {
            match (f.hover, &focus) {
                (Some(h), _) if a == h || b == h => 1.6,
                (Some(_), _) => 0.15,
                _ => 1.0,
            }
        };
        let (ox, oy) = cam.to_px(w, h, 0.0, 0.0);
        let force = f.style == Style::Force;

        // Depth rings: solid guides in radial mode, faint hints in force mode.
        if f.rings > 0.01 {
            let ring_stroke = Stroke { width: 1.0, ..Default::default() };
            for d in 1..=lay.max_depth {
                if let Some(c) = PathBuilder::from_circle(ox, oy, radius(d) * cam.scale) {
                    pm.stroke_path(&c, &paint(RING, 0.6 * f.rings), &ring_stroke, Transform::identity(), None);
                }
            }
        }

        // Non-tree dependency edges between started units.
        if self.opts.extra_edges {
            let mut faint = PathBuilder::new();
            let mut lit = PathBuilder::new();
            for &(a, b) in &lay.extra_edges {
                if node_vis(a) == Visual::Ghost || node_vis(b) == Visual::Ghost {
                    continue;
                }
                let pb = if edge_focus(a, b) > 1.0 { &mut lit } else { &mut faint };
                let ((x0, y0), (x1, y1)) = (px(a), px(b));
                pb.move_to(x0, y0);
                if force {
                    pb.line_to(x1, y1);
                } else {
                    // Bow the chord towards the center.
                    let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
                    pb.quad_to(mx + (ox - mx) * 0.35, my + (oy - my) * 0.35, x1, y1);
                }
            }
            let s = Stroke { width: 0.6, ..Default::default() };
            let base = if focus.is_some() { 0.03 } else if force { 0.10 } else { 0.07 };
            if let Some(p) = faint.finish() {
                pm.stroke_path(&p, &paint(MUTED, base), &s, Transform::identity(), None);
            }
            if let Some(p) = lit.finish() {
                let s = Stroke { width: 1.0, ..Default::default() };
                pm.stroke_path(&p, &paint(MUTED, 0.55), &s, Transform::identity(), None);
            }
        }

        // Tree edges, grouped by (state, focus) so each group is one path.
        let mut groups: HashMap<(u8, u8), PathBuilder> = HashMap::new();
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            let p = n.parent.unwrap_or(ROOT);
            let fo = edge_focus(p, i);
            let fkey = if fo > 1.0 { 2 } else if fo < 1.0 { 0 } else { 1 };
            let pb = groups.entry((node_vis(i) as u8, fkey)).or_default();
            let ((x0, y0), (x1, y1)) = (px(p), px(i));
            pb.move_to(x0, y0);
            if force {
                pb.line_to(x1, y1);
            } else {
                // Radial: leave the parent outward, arrive at the child radially.
                let (r0, t0) = polar(x0 - ox, y0 - oy);
                let (r1, t1) = polar(x1 - ox, y1 - oy);
                let mid = (r0 + r1) / 2.0;
                let (c2x, c2y) = (ox + mid * t1.cos(), oy + mid * t1.sin());
                if p == ROOT {
                    pb.quad_to(c2x, c2y, x1, y1);
                } else {
                    pb.cubic_to(ox + mid * t0.cos(), oy + mid * t0.sin(), c2x, c2y, x1, y1);
                }
            }
        }
        for ((key, fkey), pb) in groups {
            let (c, a, wdt) = match key {
                k if k == Visual::Ghost as u8 => (GHOST, 0.35, 0.7),
                k if k == Visual::Activating as u8 || k == Visual::Stopping as u8 => (ACTIVATING, 0.8, 1.4),
                k if k == Visual::Active as u8 => (ACTIVE, 0.45, 1.1),
                k if k == Visual::Exited as u8 => (EXITED, 0.4, 1.0),
                _ => (FAILED, 0.8, 1.4),
            };
            let (a, wdt) = match fkey {
                2 => ((a * 2.0f32).min(1.0), wdt * 1.6),
                0 => (a * 0.15, wdt),
                _ => (a, wdt),
            };
            if let Some(p) = pb.finish() {
                let s = Stroke { width: wdt, ..Default::default() };
                pm.stroke_path(&p, &paint(c, a), &s, Transform::identity(), None);
            }
        }

        // Processes: tiny satellites on running units.
        for (pi, p) in lay.procs.iter().enumerate() {
            if !matches!(node_vis(p.unit), Visual::Active | Visual::Activating | Visual::Stopping) {
                continue;
            }
            let (x0, y0) = px(p.unit);
            let (x, y) = if force {
                // Orbit, fanning away from the center.
                let siblings: Vec<usize> =
                    lay.procs.iter().enumerate().filter(|(_, q)| q.unit == p.unit).map(|(j, _)| j).collect();
                let k = siblings.len() as f32;
                let j = siblings.iter().position(|&j| j == pi).unwrap_or(0) as f32;
                let (_, out) = polar(x0 - ox, y0 - oy);
                let a = out + (j - (k - 1.0) / 2.0) * 0.6;
                let d = (cam.scale * 0.22).clamp(9.0, 22.0);
                (x0 + d * a.cos(), y0 + d * a.sin())
            } else {
                let (wx, wy) = (p.r * p.theta.cos(), p.r * p.theta.sin());
                cam.to_px(w, h, wx, wy)
            };
            let a = dim(p.unit);
            let mut pb = PathBuilder::new();
            pb.move_to(x0, y0);
            pb.line_to(x, y);
            if let Some(l) = pb.finish() {
                let s = Stroke { width: 0.6, ..Default::default() };
                pm.stroke_path(&l, &paint(PROC, 0.35 * a), &s, Transform::identity(), None);
            }
            if let Some(c) = PathBuilder::from_circle(x, y, 1.8) {
                pm.fill_path(&c, &paint(PROC, 0.85 * a), FillRule::Winding, Transform::identity(), None);
            }
        }

        // Nodes.
        let pulse = 0.5 + 0.5 * (wall * TAU * 1.2).sin();
        let mut radii = vec![0.0f32; lay.nodes.len()];
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            let v = node_vis(i);
            let (x, y) = px(i);
            let is_target = n.kind == Some(UnitKind::Target);
            let base = if force {
                // Obsidian-style: size by connectedness.
                let b = 2.2 + 0.85 * (lay.degree(i) as f32).sqrt();
                if is_target { b * 1.25 } else { b }
            } else if is_target {
                4.5
            } else {
                2.8
            };
            let (c, a, rad) = match v {
                Visual::Ghost => (GHOST, 0.8, base * 0.6),
                Visual::Activating | Visual::Stopping => (ACTIVATING, 1.0, base * (1.0 + 0.35 * pulse)),
                Visual::Active if is_target => (TARGET, 1.0, base),
                Visual::Active => (ACTIVE, 1.0, base),
                Visual::Exited => (EXITED, 0.9, base * 0.85),
                Visual::Failed => (FAILED, 1.0, base * 1.2),
            };
            radii[i] = rad;
            let a = a * dim(i);
            if matches!(v, Visual::Activating | Visual::Stopping) {
                if let Some(halo) = PathBuilder::from_circle(x, y, rad * 2.6) {
                    pm.fill_path(&halo, &paint(c, 0.18 * (0.4 + pulse) * dim(i)), FillRule::Winding, Transform::identity(), None);
                }
            }
            if f.hover == Some(i) {
                if let Some(ring) = PathBuilder::from_circle(x, y, rad + 4.0) {
                    let s = Stroke { width: 1.5, ..Default::default() };
                    pm.stroke_path(&ring, &paint(TEXT, 0.9), &s, Transform::identity(), None);
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
        let core_r = (cam.scale * 0.22).clamp(6.0, 26.0);
        if let Some(c) = PathBuilder::from_circle(ox, oy, core_r * 1.8) {
            pm.fill_path(&c, &paint(TARGET, 0.10), FillRule::Winding, Transform::identity(), None);
        }
        if let Some(c) = PathBuilder::from_circle(ox, oy, core_r) {
            pm.fill_path(&c, &paint(TEXT, 0.95 * dim(ROOT)), FillRule::Winding, Transform::identity(), None);
        }

        if self.opts.labels {
            if force {
                self.labels_force(pm, f, &node_vis, &radii, &focus, w, h);
            } else {
                self.labels_radial(pm, f, &node_vis, &dim, w, h);
            }
        }

        if self.opts.hud {
            match self.opts.shutdown_from {
                Some(from) => self.hud_shutdown(pm, lay, tl, t, &vis, from),
                None => self.hud(pm, lay, tl, t, &vis),
            }
            if let Some(hv) = f.hover {
                self.info_panel(pm, lay, tl, hv, node_vis(hv), t);
            }
        }
    }

    /// Radial mode: labels run outward along the spoke where the wedge has room.
    fn labels_radial(
        &mut self,
        pm: &mut Pixmap,
        f: &Frame,
        node_vis: &dyn Fn(usize) -> Visual,
        dim: &dyn Fn(usize) -> f32,
        w: f32,
        h: f32,
    ) {
        let lay = f.lay;
        let scale = f.cam.scale;
        let px = (scale * 0.13).clamp(9.0, 13.0);
        let (ox, oy) = f.cam.to_px(w, h, 0.0, 0.0);
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            let v = node_vis(i);
            if v == Visual::Ghost && f.hover != Some(i) {
                continue;
            }
            if dim(i) < 1.0 {
                continue;
            }
            let (x, y) = f.cam.to_px(w, h, f.pos[i].0, f.pos[i].1);
            let (r, theta) = polar(x - ox, y - oy);
            let arc = (n.wedge.1 - n.wedge.0) * r;
            let is_target = n.kind == Some(UnitKind::Target);
            let room = arc >= px * 0.95 || (is_target && arc >= px * 0.6) || matches!(v, Visual::Activating | Visual::Stopping) || f.hover.is_some();
            let leaf = n.children.is_empty();
            if !room || !(leaf || is_target || v != Visual::Active || f.hover.is_some()) {
                continue;
            }
            let c = label_color(v, is_target);
            let name = short_name(&n.name);
            if leaf {
                let off = 0.18 * scale;
                self.draw_text_rotated(pm, &name, ox + (r + off) * theta.cos(), oy + (r + off) * theta.sin(), theta, px, c);
            } else {
                self.draw_text(pm, &name, x + 6.0, y + px * 0.35, px, c);
            }
        }
    }

    /// Force mode: horizontal labels, placed by priority, skipping overlaps.
    #[allow(clippy::too_many_arguments)]
    fn labels_force(
        &mut self,
        pm: &mut Pixmap,
        f: &Frame,
        node_vis: &dyn Fn(usize) -> Visual,
        radii: &[f32],
        focus: &Option<HashSet<usize>>,
        w: f32,
        h: f32,
    ) {
        let lay = f.lay;
        let px = (f.cam.scale * 0.16).clamp(9.5, 14.0);
        // Priority: hover focus, activating, failed, targets, then busiest nodes.
        let mut cand: Vec<(i64, usize)> = vec![];
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            let v = node_vis(i);
            let in_focus = focus.as_ref().map(|s| s.contains(&i));
            if in_focus == Some(false) || (v == Visual::Ghost && in_focus.is_none()) {
                continue;
            }
            let is_target = n.kind == Some(UnitKind::Target);
            let mut p = lay.degree(i) as i64;
            if is_target {
                p += 1000;
            }
            if v == Visual::Failed {
                p += 3000;
            }
            if matches!(v, Visual::Activating | Visual::Stopping) {
                p += 4000;
            }
            if in_focus == Some(true) {
                p += 10_000;
            }
            if f.hover == Some(i) {
                p += 100_000;
            }
            cand.push((-p, i));
        }
        cand.sort();
        let mut placed: Vec<(f32, f32, f32, f32)> = vec![];
        for (_, i) in cand {
            let n = &lay.nodes[i];
            let v = node_vis(i);
            let is_target = n.kind == Some(UnitKind::Target);
            let size = if f.hover == Some(i) { px * 1.2 } else { px };
            let name = short_name(&n.name);
            let tw = self.text(&name, size, TEXT).map(|p| p.width() as f32).unwrap_or(0.0);
            let (x, y) = f.cam.to_px(w, h, f.pos[i].0, f.pos[i].1);
            let lx = x + radii[i] + 4.0;
            let rect = (lx - 2.0, y - size * 0.7, lx + tw + 2.0, y + size * 0.5);
            if rect.2 < 0.0 || rect.0 > w || rect.3 < 0.0 || rect.1 > h {
                continue;
            }
            let hit = placed.iter().any(|r| rect.0 < r.2 && rect.2 > r.0 && rect.1 < r.3 && rect.3 > r.1);
            if hit && f.hover != Some(i) {
                continue;
            }
            placed.push(rect);
            let c = if f.hover == Some(i) { TEXT } else { label_color(v, is_target) };
            self.draw_text(pm, &name, lx, y + size * 0.35, size, c);
        }
    }

    /// Details of the hovered unit, bottom right.
    fn info_panel(&mut self, pm: &mut Pixmap, lay: &Layout, tl: &BootTimeline, i: usize, v: Visual, t: Usec) {
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        let small = (h / 40.0).clamp(14.0, 28.0) * 0.62;
        let line = small * 1.45;
        let name = &lay.nodes[i].name;
        let mut lines: Vec<(String, Rgb)> = vec![(name.clone(), TEXT)];
        if let Some(u) = tl.units.get(name) {
            if !u.description.is_empty() {
                lines.push((u.description.clone(), MUTED));
            }
            let (start, _) = tl.span();
            let state = match v {
                Visual::Ghost => "pending",
                Visual::Activating => "activating",
                Visual::Stopping => "stopping",
                Visual::Active => "active",
                Visual::Exited => "exited",
                Visual::Failed => "failed",
            };
            let mut s = state.to_string();
            if let Some(a) = u.t_activating {
                s += &format!(" · started +{:.3}s", a.saturating_sub(start) as f32 / 1e6);
            }
            match (u.duration(), v) {
                (Some(d), Visual::Active | Visual::Exited | Visual::Failed) => s += &format!(" · took {:.3}s", d as f32 / 1e6),
                (_, Visual::Activating) => {
                    s += &format!(" · {:.2}s so far", t.saturating_sub(u.t_activating.unwrap_or(t)) as f32 / 1e6)
                }
                _ => {}
            }
            lines.push((s, label_color(v, false)));
            if let Some(p) = lay.nodes[i].parent.filter(|&p| p != ROOT) {
                lines.push((format!("after {}", lay.nodes[p].name), MUTED));
            }
            if !u.processes.is_empty() {
                let procs: Vec<String> = u.processes.iter().map(|p| format!("{} ({})", p.comm, p.pid)).collect();
                lines.push((procs.join(", "), PROC));
            }
        }
        lines.push((format!("{} connections", lay.degree(i)), MUTED));
        let tw = lines
            .iter()
            .map(|(s, c)| self.text(s, small, *c).map(|p| p.width() as f32).unwrap_or(0.0))
            .fold(0.0, f32::max);
        let bw = tw + 28.0;
        let bh = lines.len() as f32 * line + 16.0;
        let (bx, by) = (w - bw - 16.0, h - bh - 16.0);
        if let Some(r) = Rect::from_xywh(bx, by, bw, bh) {
            pm.fill_rect(r, &paint(Rgb(0x16, 0x19, 0x22), 0.92), Transform::identity(), None);
        }
        let mut y = by + 8.0 + small;
        for (s, c) in lines {
            self.draw_text(pm, &s, bx + 14.0, y, small, c);
            y += line;
        }
    }

    /// HUD for the shutdown view: what is still running and what is holding it up.
    fn hud_shutdown(&mut self, pm: &mut Pixmap, lay: &Layout, tl: &BootTimeline, t: Usec, vis: &HashMap<&str, Visual>, from: Usec) {
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
        // Which shutdown is it? The target with a queued job tells us.
        let queued = |name: &str| tl.units.get(name).is_some_and(|u| u.job || u.state == UnitState::Active);
        let what = if queued("reboot.target") || queued("kexec.target") || queued("soft-reboot.target") {
            "rebooting"
        } else if queued("poweroff.target") {
            "powering off"
        } else if queued("halt.target") {
            "halting"
        } else {
            "shutting down"
        };
        let status = format!("{what} +{:.1}s", t.saturating_sub(from) as f32 / 1e6);
        self.draw_text(pm, &status, x, y, small * 1.15, ACTIVATING);
        y += line * 1.2;

        let count = |want: &[Visual]| {
            lay.nodes.iter().skip(1).filter(|n| vis.get(n.name.as_str()).is_some_and(|v| want.contains(v))).count()
        };
        let running = count(&[Visual::Active, Visual::Activating]);
        let stopping = count(&[Visual::Stopping]);
        let total = lay.nodes.len() - 1;
        for (label, n, c) in [
            ("running", running, ACTIVE),
            ("stopping", stopping, ACTIVATING),
            ("stopped", total - running - stopping, GHOST),
        ] {
            if let Some(d) = PathBuilder::from_circle(x + 4.0, y - small * 0.35, 4.0) {
                pm.fill_path(&d, &paint(c, 1.0), FillRule::Winding, Transform::identity(), None);
            }
            self.draw_text(pm, &format!("{n:>4}  {label}"), x + 14.0, y, small, TEXT);
            y += line;
        }

        // Units being stopped right now, longest-waiting first.
        if stopping > 0 {
            y += line * 0.5;
            self.draw_text(pm, "stopping", x, y, small, MUTED);
            y += line;
            let mut since: HashMap<&str, Usec> = HashMap::new();
            for e in tl.events.iter().filter(|e| e.state == UnitState::Deactivating && e.t <= t) {
                since.insert(e.unit.as_str(), e.t);
            }
            let mut ing: Vec<(&str, Usec)> = lay
                .nodes
                .iter()
                .skip(1)
                .filter(|n| vis.get(n.name.as_str()) == Some(&Visual::Stopping))
                .filter_map(|n| Some((n.name.as_str(), *since.get(n.name.as_str())?)))
                .collect();
            ing.sort_by_key(|(_, a)| *a);
            for (name, a) in ing.iter().take(8) {
                let s = format!("{:>6.2}s  {}", t.saturating_sub(*a) as f32 / 1e6, short_name(name));
                self.draw_text(pm, &s, x, y, small, ACTIVATING);
                y += line;
            }
        }

        // Legend, top right.
        let mut ly = 18.0 + small;
        for (label, c) in [("target", TARGET), ("running", ACTIVE), ("stopping", ACTIVATING), ("stopped", GHOST), ("process", PROC)] {
            let tw = self.text(label, small, MUTED).map(|p| p.width() as f32).unwrap_or(0.0);
            let lx = w - 18.0 - tw;
            self.draw_text(pm, label, lx, ly, small, MUTED);
            if let Some(d) = PathBuilder::from_circle(lx - 10.0, ly - small * 0.35, 3.5) {
                pm.fill_path(&d, &paint(c, 1.0), FillRule::Winding, Transform::identity(), None);
            }
            ly += line;
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
        let finished = tl.phases.finish > 0 && t >= tl.phases.finish;
        let ph = &tl.phases;
        let initrd_origin = if ph.initrd > 0 { ph.initrd } else { ph.userspace };
        // Live in the initrd, or replaying the part of a recording before the switch.
        let status = if ph.in_initrd || (ph.initrd > 0 && t < ph.userspace) {
            format!("initrd +{:.3}s", t.saturating_sub(initrd_origin) as f32 / 1e6)
        } else if finished {
            format!("userspace +{:.3}s · boot finished", ph.finish.saturating_sub(ph.userspace) as f32 / 1e6)
        } else {
            format!("userspace +{:.3}s", t.saturating_sub(ph.userspace) as f32 / 1e6)
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
        let mut parts = vec![];
        if ph.firmware > ph.loader && ph.loader > 0 {
            parts.push(format!("firmware {:.1}s", (ph.firmware - ph.loader) as f32 / 1e6));
        }
        if ph.loader > 0 {
            parts.push(format!("loader {:.1}s", ph.loader as f32 / 1e6));
        }
        if ph.in_initrd {
            // The initrd's duration is not known yet.
            parts.push(format!("kernel {:.2}s", initrd_origin as f32 / 1e6));
        } else if ph.initrd > 0 {
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
    // systemd escapes "-" inside path-derived names (mounts, devices).
    let n = &n.replace("\\x2d", "-");
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
