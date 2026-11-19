//! Software renderer: Layout + timeline at time `t` -> tiny-skia Pixmap.

use crate::config::{Look, Palette, Rgb8};
use crate::layout::{Layout, ROOT};
use crate::model::{BootTimeline, UnitKind, UnitState, Usec};
use fontdue::{Font, FontSettings};
use std::collections::{HashMap, HashSet};
use std::f32::consts::TAU;
use tiny_skia::{
    Color, FillRule, FilterQuality, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke,
    Transform,
};

static FONT_BYTES: &[u8] = include_bytes!("../../../assets/DejaVuSans.ttf");

fn color(c: Rgb8, a: f32) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

fn paint(c: Rgb8, a: f32) -> Paint<'static> {
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
        (
            w / 2.0 + (x - self.cx) * self.scale,
            h / 2.0 + (y - self.cy) * self.scale,
        )
    }

    pub fn to_world(&self, w: f32, h: f32, px: f32, py: f32) -> (f32, f32) {
        (
            self.cx + (px - w / 2.0) / self.scale,
            self.cy + (py - h / 2.0) / self.scale,
        )
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
            return Self {
                cx: 0.0,
                cy: 0.0,
                scale: 50.0,
            };
        }
        let bw = (x1 - x0).max(1.0);
        let bh = (y1 - y0).max(1.0);
        let scale = ((w - 2.0 * margin).max(50.0) / bw).min((h - 2.0 * margin).max(50.0) / bh);
        Self {
            cx: (x0 + x1) / 2.0,
            cy: (y0 + y1) / 2.0,
            scale,
        }
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
    pub cam: Camera,
    pub hover: Option<usize>,
}

impl Frame<'_> {
    /// Index of the node within `reach` pixels of (px, py), if any.
    pub fn pick(&self, w: f32, h: f32, px: f32, py: f32, reach: f32) -> Option<usize> {
        let mut best: Option<(f32, usize)> = None;
        for (i, &(x, y)) in self.pos.iter().enumerate().skip(1) {
            let (sx, sy) = self.cam.to_px(w, h, x, y);
            let d2 = (sx - px).powi(2) + (sy - py).powi(2);
            if d2 < reach * reach && best.is_none_or(|(b, _)| d2 < b) {
                best = Some((d2, i));
            }
        }
        best.map(|(_, i)| i)
    }
}

/// How much a state stands out: a group shows the most notable state among
/// its members.
fn notability(v: Visual) -> u8 {
    match v {
        Visual::Ghost => 0,
        Visual::Exited => 1,
        Visual::Active => 2,
        Visual::Activating => 3,
        Visual::Stopping => 4,
        Visual::Failed => 5,
    }
}

/// Label for a node: the unit's short name, or a group's template and count.
pub fn node_label(n: &crate::layout::Node) -> String {
    match n.units.len() {
        0 | 1 => short_name(&n.name),
        k => format!("{} ×{k}", short_name(&n.name)),
    }
}

/// A label stays fully visible this long after its unit finished starting...
const LABEL_HOLD: f32 = 0.9;
/// ...and is gone this long after.
pub const LABEL_FADE: f32 = 1.5;

/// Size of the shared UI pieces at a given screen scale.
struct Sizes {
    big: f32,
    small: f32,
    line: f32,
    margin: f32,
}

impl Sizes {
    /// `s`: the screen scale; `text`: the configured text scale (text only:
    /// the margin follows the screen).
    fn at(s: f32, text: f32) -> Self {
        let small = 13.5 * s * text;
        Self {
            big: 22.0 * s * text,
            small,
            line: small * 1.45,
            margin: 18.0 * s,
        }
    }
}

fn label_color(pal: &Palette, v: Visual, is_target: bool) -> Rgb8 {
    match v {
        Visual::Activating | Visual::Stopping => pal.starting,
        Visual::Failed => pal.failed,
        _ if is_target => pal.target,
        Visual::Exited | Visual::Ghost => pal.muted,
        _ => pal.text,
    }
}

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub extra_edges: bool,
    pub labels: bool,
    pub hud: bool,
    /// Shutdown view: when the shutdown began. Units that stop after this
    /// fade out instead of showing as "exited", and the HUD counts down.
    pub shutdown_from: Option<Usec>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            extra_edges: true,
            labels: true,
            hud: true,
            shutdown_from: None,
        }
    }
}

pub struct Renderer {
    font: Font,
    /// Colors, text scale, title, panels and center image.
    pub look: Look,
    text_cache: HashMap<(String, u32, [u8; 3]), Option<Pixmap>>,
    pub opts: RenderOptions,
    /// A passphrase request to draw over everything else.
    pub prompt: Option<Prompt>,
    /// Device pixels per layout pixel: text, dots and lines are sized for a
    /// ~96 dpi screen and multiplied by this, so they keep their physical
    /// size on dense screens. Set by the backend from the display.
    pub ui_scale: f32,
}

/// A passphrase request, as shown to the user.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    /// The question, as the requester worded it.
    pub message: String,
    /// What to show in the input field: the text itself, or one dot per character.
    pub entered: String,
    /// Shown under the field, e.g. after a wrong passphrase.
    pub note: Option<String>,
}

/// Unit visual states at time `t`. With `shutdown_from`, units that stop at
/// or after that time fade back to `Ghost` instead of showing as `Exited`.
pub fn visuals_at(
    tl: &BootTimeline,
    t: Usec,
    shutdown_from: Option<Usec>,
) -> HashMap<&str, Visual> {
    let mut m: HashMap<&str, Visual> = tl
        .units
        .keys()
        .map(|k| (k.as_str(), Visual::Ghost))
        .collect();
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

/// The slowest units to start, longest first (not bootmap's own units: its
/// "start" is the wait for a display, and bootmap-quit's the handover).
fn slowest(lay: &Layout, tl: &BootTimeline) -> Vec<(String, Usec)> {
    let mut slow: Vec<(String, Usec)> = lay
        .nodes
        .iter()
        .flat_map(|n| &n.units)
        .filter_map(|u| Some((u.clone(), tl.units.get(u)?.duration()?)))
        .filter(|(name, d)| *d > 0 && !name.starts_with("bootmap"))
        .collect();
    slow.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
    slow
}

/// Which nodes get a label right now, by priority (lower sorts first), with
/// their opacity. Only what tells the story is labelled:
/// - while hovering: the hovered unit and its neighbors, nothing else;
/// - failures, and units starting or stopping;
/// - a unit that just finished starting, for a moment, fading out;
/// - once the boot has finished, the slowest units;
/// - targets, the boot's milestones.
///
/// Everything else stays a dot.
fn label_candidates(
    f: &Frame,
    tl: &BootTimeline,
    t: Usec,
    node_vis: &dyn Fn(usize) -> Visual,
    focus: &Option<HashSet<usize>>,
) -> Vec<(i64, usize, f32)> {
    let lay = f.lay;
    let mut changed: HashMap<&str, Usec> = HashMap::new();
    for e in tl.events.iter().take_while(|e| e.t <= t) {
        changed.insert(e.unit.as_str(), e.t);
    }
    let finished = tl.phases.finish > 0 && t >= tl.phases.finish;
    let slow: HashSet<usize> = if finished {
        slowest(lay, tl)
            .iter()
            .take(8)
            .filter_map(|(u, _)| lay.index.get(u).copied())
            .collect()
    } else {
        HashSet::new()
    };
    let mut out = vec![];
    for (i, n) in lay.nodes.iter().enumerate().skip(1) {
        let v = node_vis(i);
        let since = n
            .units
            .iter()
            .filter_map(|u| changed.get(u.as_str()))
            .max()
            .map(|&c| t.saturating_sub(c) as f32 / 1e6);
        let is_target = n.kind == Some(UnitKind::Target);
        let (p, alpha) = match focus {
            Some(set) if f.hover == Some(i) => (100_000, 1.0),
            Some(set) if set.contains(&i) => (10_000, 1.0),
            Some(_) => continue,
            None if v == Visual::Failed => (5_000, 1.0),
            None if matches!(v, Visual::Activating | Visual::Stopping) => (4_000, 1.0),
            None if matches!(v, Visual::Active | Visual::Exited)
                && since.is_some_and(|d| d < LABEL_FADE) =>
            {
                let d = since.unwrap_or(0.0);
                (
                    3_000,
                    if d <= LABEL_HOLD {
                        1.0
                    } else {
                        1.0 - (d - LABEL_HOLD) / (LABEL_FADE - LABEL_HOLD)
                    },
                )
            }
            // Once finished, the slowest units explain the boot: they win over targets.
            None if slow.contains(&i) => (2_500, 1.0),
            None if is_target && v != Visual::Ghost => (2_000, 1.0),
            None => continue,
        };
        // Among equals, busier nodes first.
        out.push((-(p + lay.degree(i) as i64), i, alpha));
    }
    out
}

impl Renderer {
    /// `opts.labels` and `opts.extra_edges` can only turn off what the
    /// look's `show` already allows.
    pub fn new(mut opts: RenderOptions, look: Look) -> Self {
        let font = look
            .font
            .as_deref()
            .and_then(|b| Font::from_bytes(b, FontSettings::default()).ok())
            .unwrap_or_else(|| {
                Font::from_bytes(FONT_BYTES, FontSettings::default()).expect("bundled font parses")
            });
        opts.labels &= look.show.labels;
        opts.extra_edges &= look.show.extra_edges;
        Self {
            font,
            look,
            text_cache: HashMap::new(),
            opts,
            prompt: None,
            ui_scale: 1.0,
        }
    }

    /// The colour behind everything, for filling areas the picture does not cover.
    pub fn background(&self) -> Color {
        color(self.look.palette.background, 1.0)
    }

    fn sizes(&self) -> Sizes {
        Sizes::at(self.ui_scale, self.look.text_scale)
    }

    /// Rasterize a single line of text into a cached pixmap.
    fn text(&mut self, s: &str, px: f32, c: Rgb8) -> Option<&Pixmap> {
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

    fn draw_text(&mut self, pm: &mut Pixmap, s: &str, x: f32, y: f32, px: f32, c: Rgb8) -> f32 {
        self.draw_text_faded(pm, s, x, y, px, c, 1.0)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text_faded(
        &mut self,
        pm: &mut Pixmap,
        s: &str,
        x: f32,
        y: f32,
        px: f32,
        c: Rgb8,
        opacity: f32,
    ) -> f32 {
        let Some(t) = self.text(s, px, c) else {
            return 0.0;
        };
        let w = t.width() as f32;
        let t = t.clone();
        pm.draw_pixmap(
            0,
            0,
            t.as_ref(),
            &PixmapPaint {
                opacity,
                ..Default::default()
            },
            Transform::from_translate(x.round(), (y - px).round()),
            None,
        );
        w
    }

    /// Render a frame. `t` is absolute monotonic µs; `wall` is seconds of wall
    /// time used for animation (pulsing).
    pub fn render(&mut self, pm: &mut Pixmap, f: &Frame, tl: &BootTimeline, t: Usec, wall: f32) {
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        let pal = self.look.palette;
        pm.fill(color(pal.background, 1.0));
        let vis = visuals_at(tl, t, self.opts.shutdown_from);
        let lay = f.lay;
        let cam = f.cam;
        let px = |i: usize| -> (f32, f32) {
            let (x, y) = f.pos.get(i).copied().unwrap_or((0.0, 0.0));
            cam.to_px(w, h, x, y)
        };
        let s = self.ui_scale;
        let node_vis = |i: usize| -> Visual {
            if i == ROOT {
                return Visual::Active;
            }
            lay.nodes[i]
                .units
                .iter()
                .map(|u| *vis.get(u.as_str()).unwrap_or(&Visual::Ghost))
                .max_by_key(|v| notability(*v))
                .unwrap_or(Visual::Ghost)
        };
        // Hover focus: the hovered node and its neighbors stay bright.
        let focus: Option<HashSet<usize>> = f.hover.map(|h| {
            let mut s: HashSet<usize> = lay
                .adj
                .get(h)
                .map(|a| a.iter().copied().collect())
                .unwrap_or_default();
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

        // Non-tree dependency edges between started units.
        if self.opts.extra_edges {
            let mut faint = PathBuilder::new();
            let mut lit = PathBuilder::new();
            for &(a, b) in &lay.extra_edges {
                if node_vis(a) == Visual::Ghost || node_vis(b) == Visual::Ghost {
                    continue;
                }
                let pb = if edge_focus(a, b) > 1.0 {
                    &mut lit
                } else {
                    &mut faint
                };
                let ((x0, y0), (x1, y1)) = (px(a), px(b));
                pb.move_to(x0, y0);
                pb.line_to(x1, y1);
            }
            let stroke = Stroke {
                width: 0.6 * s,
                ..Default::default()
            };
            let base = if focus.is_some() { 0.03 } else { 0.10 };
            if let Some(p) = faint.finish() {
                pm.stroke_path(
                    &p,
                    &paint(pal.muted, base),
                    &stroke,
                    Transform::identity(),
                    None,
                );
            }
            if let Some(p) = lit.finish() {
                let stroke = Stroke {
                    width: 1.0 * s,
                    ..Default::default()
                };
                pm.stroke_path(
                    &p,
                    &paint(pal.muted, 0.55),
                    &stroke,
                    Transform::identity(),
                    None,
                );
            }
        }

        // Tree edges, grouped by (state, focus) so each group is one path.
        // In a fixed order: overlapping translucent strokes blend by draw order,
        // and a HashMap's order changes between runs.
        let mut groups: std::collections::BTreeMap<(u8, u8), PathBuilder> = Default::default();
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            let p = n.parent.unwrap_or(ROOT);
            let fo = edge_focus(p, i);
            let fkey = if fo > 1.0 {
                2
            } else if fo < 1.0 {
                0
            } else {
                1
            };
            let pb = groups.entry((node_vis(i) as u8, fkey)).or_default();
            let ((x0, y0), (x1, y1)) = (px(p), px(i));
            pb.move_to(x0, y0);
            pb.line_to(x1, y1);
        }
        for ((key, fkey), pb) in groups {
            let (c, a, wdt) = match key {
                k if k == Visual::Ghost as u8 => (pal.pending, 0.35, 0.7),
                k if k == Visual::Activating as u8 || k == Visual::Stopping as u8 => {
                    (pal.starting, 0.8, 1.4)
                }
                k if k == Visual::Active as u8 => (pal.active, 0.45, 1.1),
                k if k == Visual::Exited as u8 => (pal.exited, 0.4, 1.0),
                _ => (pal.failed, 0.8, 1.4),
            };
            let (a, wdt) = match fkey {
                2 => ((a * 2.0f32).min(1.0), wdt * 1.6),
                0 => (a * 0.15, wdt),
                _ => (a, wdt),
            };
            if let Some(p) = pb.finish() {
                let stroke = Stroke {
                    width: wdt * s,
                    ..Default::default()
                };
                pm.stroke_path(&p, &paint(c, a), &stroke, Transform::identity(), None);
            }
        }

        // Processes: tiny satellites on running units.
        for (pi, p) in lay.procs.iter().enumerate() {
            if !matches!(
                node_vis(p.unit),
                Visual::Active | Visual::Activating | Visual::Stopping
            ) {
                continue;
            }
            let (x0, y0) = px(p.unit);
            // Orbit the unit, fanning away from the center.
            let siblings: Vec<usize> = lay
                .procs
                .iter()
                .enumerate()
                .filter(|(_, q)| q.unit == p.unit)
                .map(|(j, _)| j)
                .collect();
            let k = siblings.len() as f32;
            let j = siblings.iter().position(|&j| j == pi).unwrap_or(0) as f32;
            let out = (y0 - oy).atan2(x0 - ox);
            let angle = out + (j - (k - 1.0) / 2.0) * 0.6;
            let d = (cam.scale * 0.22).clamp(9.0 * s, 22.0 * s);
            let (x, y) = (x0 + d * angle.cos(), y0 + d * angle.sin());
            let a = dim(p.unit);
            let mut pb = PathBuilder::new();
            pb.move_to(x0, y0);
            pb.line_to(x, y);
            if let Some(l) = pb.finish() {
                let stroke = Stroke {
                    width: 0.6 * s,
                    ..Default::default()
                };
                pm.stroke_path(
                    &l,
                    &paint(pal.process, 0.35 * a),
                    &stroke,
                    Transform::identity(),
                    None,
                );
            }
            if let Some(c) = PathBuilder::from_circle(x, y, 1.8 * s) {
                pm.fill_path(
                    &c,
                    &paint(pal.process, 0.85 * a),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }

        // Nodes.
        let pulse = 0.5 + 0.5 * (wall * TAU * 1.2).sin();
        let mut radii = vec![0.0f32; lay.nodes.len()];
        for (i, n) in lay.nodes.iter().enumerate().skip(1) {
            let v = node_vis(i);
            let (x, y) = px(i);
            let is_target = n.kind == Some(UnitKind::Target);
            // Size by connectedness; targets a little larger.
            let base = (2.2 + 0.85 * (lay.degree(i) as f32).sqrt())
                * if is_target { 1.25 } else { 1.0 }
                * s;
            let (c, a, rad) = match v {
                Visual::Ghost => (pal.pending, 0.8, base * 0.6),
                Visual::Activating | Visual::Stopping => {
                    (pal.starting, 1.0, base * (1.0 + 0.35 * pulse))
                }
                Visual::Active if is_target => (pal.target, 1.0, base),
                Visual::Active => (pal.active, 1.0, base),
                Visual::Exited => (pal.exited, 0.9, base * 0.85),
                Visual::Failed => (pal.failed, 1.0, base * 1.2),
            };
            radii[i] = rad;
            let a = a * dim(i);
            if matches!(v, Visual::Activating | Visual::Stopping) {
                if let Some(halo) = PathBuilder::from_circle(x, y, rad * 2.6) {
                    pm.fill_path(
                        &halo,
                        &paint(c, 0.18 * (0.4 + pulse) * dim(i)),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }
            if f.hover == Some(i) {
                if let Some(ring) = PathBuilder::from_circle(x, y, rad + 4.0 * s) {
                    let stroke = Stroke {
                        width: 1.5 * s,
                        ..Default::default()
                    };
                    pm.stroke_path(
                        &ring,
                        &paint(pal.text, 0.9),
                        &stroke,
                        Transform::identity(),
                        None,
                    );
                }
            }
            if let Some(dot) = PathBuilder::from_circle(x, y, rad) {
                if v == Visual::Exited {
                    let stroke = Stroke {
                        width: 1.2 * s,
                        ..Default::default()
                    };
                    pm.stroke_path(&dot, &paint(c, a), &stroke, Transform::identity(), None);
                } else {
                    pm.fill_path(
                        &dot,
                        &paint(c, a),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }
        }

        // Center node.
        let core_r = (cam.scale * 0.22).clamp(6.0 * s, 26.0 * s);
        if let Some(c) = PathBuilder::from_circle(ox, oy, core_r * 1.8) {
            pm.fill_path(
                &c,
                &paint(pal.target, 0.10),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        match &self.look.center_image {
            Some(img) => {
                // Scaled to fit a square, aspect kept, centered on the node.
                let k = 2.8 * core_r / img.width().max(img.height()) as f32;
                let (dx, dy) = (
                    ox - img.width() as f32 * k / 2.0,
                    oy - img.height() as f32 * k / 2.0,
                );
                let ipaint = PixmapPaint {
                    opacity: 0.95 * dim(ROOT),
                    quality: FilterQuality::Bilinear,
                    ..Default::default()
                };
                pm.draw_pixmap(
                    0,
                    0,
                    img.as_ref(),
                    &ipaint,
                    Transform::from_scale(k, k).post_translate(dx, dy),
                    None,
                );
            }
            None => {
                if let Some(c) = PathBuilder::from_circle(ox, oy, core_r) {
                    pm.fill_path(
                        &c,
                        &paint(pal.text, 0.95 * dim(ROOT)),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
            }
        }

        if self.opts.labels {
            let cands = label_candidates(f, tl, t, &node_vis, &focus);
            self.labels(pm, f, cands, &node_vis, &radii, w, h);
        }

        if self.opts.hud {
            match self.opts.shutdown_from {
                Some(from) => self.hud_shutdown(pm, lay, tl, t, &vis, from),
                None => self.hud(pm, lay, tl, t, &vis),
            }
            if let Some(hv) = f.hover {
                self.info_panel(pm, lay, tl, hv, node_vis(hv), t, &vis);
            }
        }
        if let Some(prompt) = self.prompt.clone() {
            self.draw_prompt(pm, &prompt, wall);
        }
    }

    /// The passphrase request: dims the graph and draws a centered panel.
    fn draw_prompt(&mut self, pm: &mut Pixmap, prompt: &Prompt, wall: f32) {
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        let s = self.ui_scale;
        let pal = self.look.palette;
        let px = 18.0 * s * self.look.text_scale;
        let line = px * 1.5;
        let pad = px * 1.4;
        if let Some(r) = Rect::from_xywh(0.0, 0.0, w, h) {
            pm.fill_rect(r, &paint(pal.background, 0.6), Transform::identity(), None);
        }
        let text_w = |me: &mut Self, s: &str, size: f32| {
            me.text(s, size, pal.text)
                .map(|p| p.width() as f32)
                .unwrap_or(0.0)
        };
        let hint = "Enter to confirm  ·  Esc to start over";
        let small = px * 0.8;
        let inner = text_w(self, &prompt.message, px)
            .max(text_w(self, hint, small))
            .max(
                prompt
                    .note
                    .as_deref()
                    .map_or(0.0, |n| text_w(self, n, small)),
            )
            .max(px * 22.0)
            .min(w - 4.0 * pad);
        let rows = 3.6 + if prompt.note.is_some() { 1.0 } else { 0.0 };
        let (bw, bh) = (inner + 2.0 * pad, rows * line + 2.0 * pad);
        let (bx, by) = ((w - bw) / 2.0, (h - bh) / 2.0);
        if let Some(r) = Rect::from_xywh(bx, by, bw, bh) {
            pm.fill_rect(r, &paint(pal.panel, 0.97), Transform::identity(), None);
        }
        if let Some(r) = Rect::from_xywh(bx, by, bw, 2.0 * s) {
            pm.fill_rect(r, &paint(pal.starting, 1.0), Transform::identity(), None);
        }
        let x = bx + pad;
        let mut y = by + pad + px;
        self.draw_text(pm, &prompt.message, x, y, px, pal.text);

        // Input field: what was entered (clipped to its tail) and a blinking caret.
        y += line * 0.6;
        let field_h = line * 1.1;
        if let Some(r) = Rect::from_xywh(x, y, inner, field_h) {
            pm.fill_rect(r, &paint(pal.background, 1.0), Transform::identity(), None);
        }
        let mut shown: &str = &prompt.entered;
        while text_w(self, shown, px) > inner - px * 1.5 {
            let Some((i, _)) = shown.char_indices().nth(1) else {
                break;
            };
            shown = &shown[i..];
        }
        let end = x
            + px * 0.5
            + self.draw_text(
                pm,
                shown,
                x + px * 0.5,
                y + field_h * 0.5 + px * 0.38,
                px,
                pal.text,
            );
        if (wall * 2.0) as i64 % 2 == 0 {
            if let Some(r) =
                Rect::from_xywh(end + 2.0 * s, y + field_h * 0.2, 2.0 * s, field_h * 0.6)
            {
                pm.fill_rect(r, &paint(pal.text, 0.9), Transform::identity(), None);
            }
        }
        y += field_h + line * 0.9;
        if let Some(note) = &prompt.note {
            self.draw_text(pm, note, x, y, small, pal.failed);
            y += line;
        }
        self.draw_text(pm, hint, x, y, small, pal.muted);
    }

    /// Horizontal labels, in priority order, skipping any that would overlap
    /// one already placed (the hovered unit's label is always placed).
    #[allow(clippy::too_many_arguments)]
    fn labels(
        &mut self,
        pm: &mut Pixmap,
        f: &Frame,
        mut cands: Vec<(i64, usize, f32)>,
        node_vis: &dyn Fn(usize) -> Visual,
        radii: &[f32],
        w: f32,
        h: f32,
    ) {
        let s = self.ui_scale;
        let px = 12.5 * s * self.look.text_scale;
        let pal = self.look.palette;
        cands.sort_by_key(|c| c.0);
        let mut placed: Vec<(f32, f32, f32, f32)> = vec![];
        for (_, i, alpha) in cands {
            let n = &f.lay.nodes[i];
            let v = node_vis(i);
            let is_target = n.kind == Some(UnitKind::Target);
            let size = if f.hover == Some(i) { px * 1.15 } else { px };
            let name = node_label(n);
            let tw = self
                .text(&name, size, pal.text)
                .map(|p| p.width() as f32)
                .unwrap_or(0.0);
            let (x, y) = f.cam.to_px(w, h, f.pos[i].0, f.pos[i].1);
            let lx = x + radii[i] + 4.0 * s;
            let pad = 2.0 * s;
            let rect = (lx - pad, y - size * 0.7, lx + tw + pad, y + size * 0.5);
            if rect.2 < 0.0 || rect.0 > w || rect.3 < 0.0 || rect.1 > h {
                continue;
            }
            let hit = placed
                .iter()
                .any(|r| rect.0 < r.2 && rect.2 > r.0 && rect.1 < r.3 && rect.3 > r.1);
            if hit && f.hover != Some(i) {
                continue;
            }
            placed.push(rect);
            let c = if f.hover == Some(i) {
                pal.text
            } else {
                label_color(&pal, v, is_target)
            };
            self.draw_text_faded(pm, &name, lx, y + size * 0.35, size, c, alpha);
        }
    }

    /// Details of the hovered unit (or group), bottom right.
    #[allow(clippy::too_many_arguments)]
    fn info_panel(
        &mut self,
        pm: &mut Pixmap,
        lay: &Layout,
        tl: &BootTimeline,
        i: usize,
        v: Visual,
        t: Usec,
        vis: &HashMap<&str, Visual>,
    ) {
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        let s = self.ui_scale;
        let small = self.sizes().small;
        let pal = self.look.palette;
        let line = small * 1.45;
        let node = &lay.nodes[i];
        let state_word = |v: Visual| match v {
            Visual::Ghost => "pending",
            Visual::Activating => "activating",
            Visual::Stopping => "stopping",
            Visual::Active => "active",
            Visual::Exited => "exited",
            Visual::Failed => "failed",
        };
        let mut lines: Vec<(String, Rgb8)> = vec![];
        if node.units.len() > 1 {
            // A group: what it is, then its instances and their states.
            lines.push((node_label(node), pal.text));
            lines.push((format!("instances of {}", node.name), pal.muted));
            const SHOWN: usize = 8;
            for u in node.units.iter().take(SHOWN) {
                let uv = *vis.get(u.as_str()).unwrap_or(&Visual::Ghost);
                let instance = u.split_once('@').map_or(u.as_str(), |(_, rest)| {
                    rest.rsplit_once('.').map_or(rest, |(i, _)| i)
                });
                let took = tl
                    .units
                    .get(u)
                    .and_then(|u| u.duration())
                    .map(|d| format!(" · {:.3}s", d as f32 / 1e6))
                    .unwrap_or_default();
                lines.push((
                    format!("  {instance}  {}{took}", state_word(uv)),
                    label_color(&pal, uv, false),
                ));
            }
            if node.units.len() > SHOWN {
                lines.push((
                    format!("  and {} more", node.units.len() - SHOWN),
                    pal.muted,
                ));
            }
        } else {
            lines.push((node.name.clone(), pal.text));
        }
        if let Some(u) = tl.units.get(&node.name).filter(|_| node.units.len() == 1) {
            if !u.description.is_empty() {
                lines.push((u.description.clone(), pal.muted));
            }
            let (start, _) = tl.span();
            let mut st = state_word(v).to_string();
            if let Some(a) = u.t_activating {
                st += &format!(" · started +{:.3}s", a.saturating_sub(start) as f32 / 1e6);
            }
            match (u.duration(), v) {
                (Some(d), Visual::Active | Visual::Exited | Visual::Failed) => {
                    st += &format!(" · took {:.3}s", d as f32 / 1e6)
                }
                (_, Visual::Activating) => {
                    st += &format!(
                        " · {:.2}s so far",
                        t.saturating_sub(u.t_activating.unwrap_or(t)) as f32 / 1e6
                    )
                }
                _ => {}
            }
            lines.push((st, label_color(&pal, v, false)));
            if !u.processes.is_empty() {
                let procs: Vec<String> = u
                    .processes
                    .iter()
                    .map(|p| format!("{} ({})", p.comm, p.pid))
                    .collect();
                lines.push((procs.join(", "), pal.process));
            }
        }
        if let Some(p) = node.parent.filter(|&p| p != ROOT) {
            lines.push((format!("after {}", node_label(&lay.nodes[p])), pal.muted));
        }
        lines.push((format!("{} connections", lay.degree(i)), pal.muted));
        let tw = lines
            .iter()
            .map(|(txt, c)| {
                self.text(txt, small, *c)
                    .map(|p| p.width() as f32)
                    .unwrap_or(0.0)
            })
            .fold(0.0, f32::max);
        let (pad, edge) = (14.0 * s, 16.0 * s);
        let bw = (tw + 2.0 * pad).min(w - 2.0 * edge);
        let bh = lines.len() as f32 * line + edge;
        let (bx, by) = (w - bw - edge, h - bh - edge);
        if let Some(r) = Rect::from_xywh(bx, by, bw, bh) {
            pm.fill_rect(r, &paint(pal.panel, 0.92), Transform::identity(), None);
        }
        let mut y = by + edge / 2.0 + small;
        for (txt, c) in lines {
            self.draw_text(pm, &txt, bx + pad, y, small, c);
            y += line;
        }
    }

    /// The title line, top left; returns the baseline of the next line (of
    /// the status text, which is a little larger than `small`).
    fn hud_title(&mut self, pm: &mut Pixmap, tl: &BootTimeline, z: &Sizes) -> f32 {
        let title = match &self.look.title {
            Some(t) => t.clone(),
            None if tl.host.is_empty() => "bootmap".into(),
            None => format!("bootmap · {}", tl.host),
        };
        if title.is_empty() {
            return z.margin + z.small * 1.15;
        }
        let y = z.margin + z.big;
        let text = self.look.palette.text;
        self.draw_text(pm, &title, z.margin, y, z.big, text);
        y + z.big * 1.25
    }

    /// Lowest baseline the HUD's left column may use: above the phase line
    /// and its bar when shown, else the bottom margin.
    fn hud_floor(&self, h: f32, z: &Sizes) -> f32 {
        let s = self.ui_scale;
        if self.look.show.phases {
            h - 16.0 * s - z.small - 10.0 * s - 6.0 * s
        } else {
            h - z.margin
        }
    }

    /// One "dot  count  label" row; returns the y of the next row.
    fn hud_count(
        &mut self,
        pm: &mut Pixmap,
        z: &Sizes,
        y: f32,
        n: usize,
        label: &str,
        c: Rgb8,
    ) -> f32 {
        let s = self.ui_scale;
        let text = self.look.palette.text;
        if let Some(d) = PathBuilder::from_circle(z.margin + 4.0 * s, y - z.small * 0.35, 4.0 * s) {
            pm.fill_path(
                &d,
                &paint(c, 1.0),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        self.draw_text(
            pm,
            &format!("{n:>4}  {label}"),
            z.margin + 14.0 * s,
            y,
            z.small,
            text,
        );
        y + z.line
    }

    /// Colour legend, top right.
    fn hud_legend(&mut self, pm: &mut Pixmap, z: &Sizes, entries: &[(&str, Rgb8)]) {
        let (w, s) = (pm.width() as f32, self.ui_scale);
        let muted = self.look.palette.muted;
        let mut y = z.margin + z.small;
        for &(label, c) in entries {
            let tw = self
                .text(label, z.small, muted)
                .map(|p| p.width() as f32)
                .unwrap_or(0.0);
            let x = w - z.margin - tw;
            self.draw_text(pm, label, x, y, z.small, muted);
            if let Some(d) = PathBuilder::from_circle(x - 10.0 * s, y - z.small * 0.35, 3.5 * s) {
                pm.fill_path(
                    &d,
                    &paint(c, 1.0),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            y += z.line;
        }
    }

    /// HUD for the shutdown view: what is still running and what is holding it up.
    fn hud_shutdown(
        &mut self,
        pm: &mut Pixmap,
        lay: &Layout,
        tl: &BootTimeline,
        t: Usec,
        vis: &HashMap<&str, Visual>,
        from: Usec,
    ) {
        let z = self.sizes();
        let pal = self.look.palette;
        let x = z.margin;
        let show = self.look.show;
        let floor = pm.height() as f32 - z.margin;
        let mut y = self.hud_title(pm, tl, &z);
        // Which shutdown is it? The target with a queued job tells us.
        let queued = |name: &str| {
            tl.units
                .get(name)
                .is_some_and(|u| u.job || u.state == UnitState::Active)
        };
        let what =
            if queued("reboot.target") || queued("kexec.target") || queued("soft-reboot.target") {
                "rebooting"
            } else if queued("poweroff.target") {
                "powering off"
            } else if queued("halt.target") {
                "halting"
            } else {
                "shutting down"
            };
        let status = format!("{what} +{:.1}s", t.saturating_sub(from) as f32 / 1e6);
        if show.status {
            self.draw_text(pm, &status, x, y, z.small * 1.15, pal.starting);
            y += z.line * 1.2;
        }

        let units: Vec<&str> = lay
            .nodes
            .iter()
            .flat_map(|n| n.units.iter().map(|u| u.as_str()))
            .collect();
        let count = |want: &[Visual]| {
            units
                .iter()
                .filter(|u| vis.get(*u).is_some_and(|v| want.contains(v)))
                .count()
        };
        let running = count(&[Visual::Active, Visual::Activating]);
        let stopping = count(&[Visual::Stopping]);
        if show.status {
            y = self.hud_count(pm, &z, y, running, "running", pal.active);
            y = self.hud_count(pm, &z, y, stopping, "stopping", pal.starting);
            y = self.hud_count(
                pm,
                &z,
                y,
                units.len() - running - stopping,
                "stopped",
                pal.pending,
            );
        }

        // Units being stopped right now, longest-waiting first.
        if show.activity && stopping > 0 {
            if show.status {
                y += z.line * 0.5;
            }
            self.draw_text(pm, "stopping", x, y, z.small, pal.muted);
            y += z.line;
            let mut since: HashMap<&str, Usec> = HashMap::new();
            for e in tl
                .events
                .iter()
                .filter(|e| e.state == UnitState::Deactivating && e.t <= t)
            {
                since.insert(e.unit.as_str(), e.t);
            }
            let mut ing: Vec<(&str, Usec)> = units
                .iter()
                .filter(|u| vis.get(*u) == Some(&Visual::Stopping))
                .filter_map(|u| Some((*u, *since.get(u)?)))
                .collect();
            ing.sort_by_key(|(_, a)| *a);
            for (name, a) in ing.iter().take(8) {
                if y > floor {
                    break;
                }
                let line = format!(
                    "{:>6.2}s  {}",
                    t.saturating_sub(*a) as f32 / 1e6,
                    short_name(name)
                );
                self.draw_text(pm, &line, x, y, z.small, pal.starting);
                y += z.line;
            }
        }

        if show.legend {
            self.hud_legend(
                pm,
                &z,
                &[
                    ("target", pal.target),
                    ("running", pal.active),
                    ("stopping", pal.starting),
                    ("stopped", pal.pending),
                    ("process", pal.process),
                ],
            );
        }
    }

    fn hud(
        &mut self,
        pm: &mut Pixmap,
        lay: &Layout,
        tl: &BootTimeline,
        t: Usec,
        vis: &HashMap<&str, Visual>,
    ) {
        let (w, h) = (pm.width() as f32, pm.height() as f32);
        let s = self.ui_scale;
        let z = self.sizes();
        let pal = self.look.palette;
        let x = z.margin;
        let show = self.look.show;
        let floor = self.hud_floor(h, &z);
        let mut y = self.hud_title(pm, tl, &z);
        let (start, end) = tl.span();
        let finished = tl.phases.finish > 0 && t >= tl.phases.finish;
        let ph = &tl.phases;
        let initrd_origin = if ph.initrd > 0 {
            ph.initrd
        } else {
            ph.userspace
        };
        // Live in the initrd, or replaying the part of a recording before the switch.
        let status = if ph.in_initrd || (ph.initrd > 0 && t < ph.userspace) {
            format!(
                "initrd +{:.3}s",
                t.saturating_sub(initrd_origin) as f32 / 1e6
            )
        } else if finished {
            format!(
                "userspace +{:.3}s · boot finished",
                ph.finish.saturating_sub(ph.userspace) as f32 / 1e6
            )
        } else {
            format!(
                "userspace +{:.3}s",
                t.saturating_sub(ph.userspace) as f32 / 1e6
            )
        };
        if show.status {
            self.draw_text(
                pm,
                &status,
                x,
                y,
                z.small * 1.15,
                if finished { pal.active } else { pal.starting },
            );
            y += z.line * 1.2;
        }

        // Counts are of units: a group of five instances counts five.
        let units: Vec<&str> = lay
            .nodes
            .iter()
            .flat_map(|n| n.units.iter().map(|u| u.as_str()))
            .collect();
        let count = |want: Visual| units.iter().filter(|u| vis.get(*u) == Some(&want)).count();
        let (n_act, n_ing, n_ex, n_fail) = (
            count(Visual::Active),
            count(Visual::Activating),
            count(Visual::Exited),
            count(Visual::Failed),
        );
        if show.status {
            y = self.hud_count(pm, &z, y, n_act, "active", pal.active);
            y = self.hud_count(pm, &z, y, n_ing, "activating", pal.starting);
            y = self.hud_count(pm, &z, y, n_ex, "exited", pal.exited);
            y = self.hud_count(pm, &z, y, n_fail, "failed", pal.failed);
            y = self.hud_count(
                pm,
                &z,
                y,
                units.len() - n_act - n_ing - n_ex - n_fail,
                "pending",
                pal.pending,
            );
        }

        // In-flight jobs, or the slowest units once finished.
        if show.status {
            y += z.line * 0.5;
        }
        if !show.activity {
            // hidden
        } else if !finished && n_ing > 0 {
            self.draw_text(pm, "starting", x, y, z.small, pal.muted);
            y += z.line;
            let mut ing: Vec<(&str, Usec)> = units
                .iter()
                .filter(|u| vis.get(*u) == Some(&Visual::Activating))
                .filter_map(|u| Some((*u, tl.units.get(*u)?.t_activating?)))
                .collect();
            ing.sort_by_key(|(_, a)| *a);
            for (name, a) in ing.iter().take(8) {
                if y > floor {
                    break;
                }
                let line = format!(
                    "{:>6.2}s  {}",
                    t.saturating_sub(*a) as f32 / 1e6,
                    short_name(name)
                );
                self.draw_text(pm, &line, x, y, z.small, pal.starting);
                y += z.line;
            }
        } else if finished || t >= end {
            self.draw_text(pm, "slowest", x, y, z.small, pal.muted);
            y += z.line;
            for (name, d) in slowest(lay, tl).iter().take(8) {
                if y > floor {
                    break;
                }
                let line = format!("{:>6.3}s  {}", *d as f32 / 1e6, short_name(name));
                self.draw_text(pm, &line, x, y, z.small, pal.text);
                y += z.line;
            }
        }

        if show.phases {
            // Phase bar along the bottom.
            let mut parts = vec![];
            if ph.firmware > ph.loader && ph.loader > 0 {
                parts.push(format!(
                    "firmware {:.1}s",
                    (ph.firmware - ph.loader) as f32 / 1e6
                ));
            }
            if ph.loader > 0 {
                parts.push(format!("loader {:.1}s", ph.loader as f32 / 1e6));
            }
            if ph.in_initrd {
                // The initrd's duration is not known yet.
                parts.push(format!("kernel {:.2}s", initrd_origin as f32 / 1e6));
            } else if ph.initrd > 0 {
                parts.push(format!("kernel {:.2}s", ph.initrd as f32 / 1e6));
                parts.push(format!(
                    "initrd {:.2}s",
                    ph.userspace.saturating_sub(ph.initrd) as f32 / 1e6
                ));
            } else if ph.userspace > 0 {
                parts.push(format!("kernel {:.2}s", ph.userspace as f32 / 1e6));
            }
            if ph.finish > 0 {
                parts.push(format!(
                    "userspace {:.2}s",
                    ph.finish.saturating_sub(ph.userspace) as f32 / 1e6
                ));
            }
            let bottom = h - 16.0 * s;
            self.draw_text(pm, &parts.join("   ·   "), x, bottom, z.small, pal.muted);

            // Timeline scrubber.
            if end > start {
                let bar_y = bottom - z.small - 10.0 * s;
                let bar_w = (w * 0.3).min(360.0 * s);
                let f = (t.saturating_sub(start) as f32 / (end - start) as f32).clamp(0.0, 1.0);
                if let Some(r) = Rect::from_xywh(x, bar_y, bar_w, 3.0 * s) {
                    pm.fill_rect(r, &paint(pal.track, 1.0), Transform::identity(), None);
                }
                if let Some(r) = Rect::from_xywh(x, bar_y, (bar_w * f).max(1.0), 3.0 * s) {
                    pm.fill_rect(
                        r,
                        &paint(if finished { pal.active } else { pal.starting }, 1.0),
                        Transform::identity(),
                        None,
                    );
                }
            }
        }

        if show.legend {
            self.hud_legend(
                pm,
                &z,
                &[
                    ("target", pal.target),
                    ("active", pal.active),
                    ("activating", pal.starting),
                    ("exited", pal.exited),
                    ("failed", pal.failed),
                    ("process", pal.process),
                ],
            );
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

fn rasterize_text(font: &Font, s: &str, px: f32, c: Rgb8) -> Option<Pixmap> {
    let lm = font.horizontal_line_metrics(px)?;
    let ascent = lm.ascent.ceil();
    let height = (lm.ascent - lm.descent).ceil() as i32 + 1;
    let glyphs: Vec<_> = s.chars().map(|ch| font.rasterize(ch, px)).collect();
    let width = glyphs
        .iter()
        .map(|(m, _)| m.advance_width)
        .sum::<f32>()
        .ceil() as i32
        + 2;
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

/// How long past a recording's last event its picture keeps changing (labels
/// still fading). A replay runs this much longer, so it ends settled.
pub const SETTLE_USEC: Usec = (LABEL_FADE as Usec + 1) * 1_000_000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Show};
    use crate::{ForceParams, Layout, LayoutOptions, Sim};

    const W: u32 = 640;
    const H: u32 = 400;

    /// The sample boot, finished, drawn with `look`.
    fn draw(look: Look) -> Pixmap {
        draw_with_origin(look).0
    }

    /// Also returns where the center node is, in pixels.
    fn draw_with_origin(look: Look) -> (Pixmap, (u32, u32)) {
        let tl = BootTimeline::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../samples/mbair-boot.json"
        )))
        .expect("sample loads");
        let lay = Layout::compute(&tl, &LayoutOptions::default());
        let mut sim = Sim::new(&lay, ForceParams::default());
        sim.settle(300);
        let pos = sim.positions();
        let cam = Camera::fit(&pos, W as f32, H as f32, 60.0);
        let mut r = Renderer::new(RenderOptions::default(), look);
        let mut pm = Pixmap::new(W, H).unwrap();
        let t = tl.span().1 + SETTLE_USEC;
        r.render(
            &mut pm,
            &Frame {
                lay: &lay,
                pos: &pos,
                cam,
                hover: None,
            },
            &tl,
            t,
            0.0,
        );
        let (ox, oy) = cam.to_px(W as f32, H as f32, 0.0, 0.0);
        (pm, (ox.round() as u32, oy.round() as u32))
    }

    fn rgb(pm: &Pixmap, x: u32, y: u32) -> [u8; 3] {
        let p = pm.pixel(x, y).unwrap();
        [p.red(), p.green(), p.blue()]
    }

    fn close(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
        a.iter()
            .zip(b)
            .all(|(x, y)| (*x as i32 - y as i32).abs() <= tol)
    }

    #[test]
    fn background_comes_from_the_palette() {
        let mut look = Look::default();
        look.palette = Palette {
            background: Rgb8(10, 20, 30),
            ..look.palette
        };
        let pm = draw(look);
        assert_eq!(rgb(&pm, W - 1, H - 1), [10, 20, 30]);
        assert_eq!(rgb(&pm, W - 1, H / 2), [10, 20, 30]);
    }

    #[test]
    fn center_image_replaces_the_disc() {
        let mut img = Pixmap::new(8, 8).unwrap();
        img.fill(Color::from_rgba8(200, 30, 60, 255));
        let look = Look {
            center_image: Some(img),
            ..Look::default()
        };
        let (pm, (ox, oy)) = draw_with_origin(look);
        let got = rgb(&pm, ox, oy);
        // The node is drawn at 95% opacity over a faint halo.
        assert!(close(got, [200, 30, 60], 25), "center pixel is {got:?}");
        let (disc, _) = draw_with_origin(Look::default());
        assert!(
            !close(rgb(&disc, ox, oy), [200, 30, 60], 60),
            "the default disc is not the image"
        );
    }

    #[test]
    fn empty_title_draws_no_heading() {
        let only_title = Show {
            status: false,
            activity: false,
            legend: false,
            phases: false,
            ..Show::default()
        };
        let strip = |pm: &Pixmap| {
            let bg = rgb(pm, W - 1, H - 1);
            (18..44)
                .flat_map(|y| (18..260).map(move |x| (x, y)))
                .filter(|&(x, y)| rgb(pm, x, y) != bg)
                .count()
        };
        let mut look = Look {
            show: only_title,
            ..Look::default()
        };
        assert!(
            strip(&draw(look.clone())) > 0,
            "the default heading is drawn"
        );
        look.title = Some(String::new());
        assert_eq!(
            strip(&draw(look.clone())),
            0,
            "no heading, nothing else in its place"
        );
        look.title = Some("My boot".into());
        assert!(strip(&draw(look)) > 0);
    }

    #[test]
    fn hidden_panels_leave_only_the_graph() {
        let parsed = Config::parse("title = \"\"\n[show]\nstatus = false\nactivity = false\nlegend = false\nphases = false").unwrap();
        let pm = draw(parsed.resolve().unwrap());
        let bg = rgb(&pm, W - 1, H - 1);
        // Top-left corner and bottom line are empty.
        for (x, y) in [
            (20, 20),
            (100, 30),
            (30, H - 20),
            (200, H - 20),
            (W - 40, 30),
        ] {
            assert_eq!(rgb(&pm, x, y), bg, "({x},{y})");
        }
    }
}
