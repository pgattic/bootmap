//! Data sources (replay / live) and the scene driver shared by all backends.

use crate::GraphArgs;
use anyhow::Result;
use bootmap_core::model::Usec;
use bootmap_core::systemd::{monotonic_now, LiveMsg, Systemd};
use bootmap_core::render::{Camera, Frame, Style};
use bootmap_core::{BootTimeline, ForceParams, Layout, LayoutOptions, Renderer, Sim};
use std::collections::HashMap;
use std::f32::consts::{PI, TAU};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Instant;

pub enum Mode {
    Replay { speed: f32, base: Usec, started: Instant, paused_at: Option<Usec> },
    Live { rx: Receiver<LiveMsg>, finished_at: Option<Instant>, disconnected: bool },
}

pub struct Source {
    pub tl: BootTimeline,
    pub mode: Mode,
}

impl Source {
    pub fn replay(tl: BootTimeline, speed: f32) -> Self {
        let (start, _) = tl.span();
        // Lead in slightly before userspace starts.
        let base = start.saturating_sub(200_000);
        Self { tl, mode: Mode::Replay { speed, base, started: Instant::now(), paused_at: None } }
    }

    pub fn live() -> Result<Self> {
        let mut sd = Systemd::connect()?;
        let tl = sd.snapshot()?;
        let rx = crate::spawn_watcher(sd);
        Ok(Self { tl, mode: Mode::Live { rx, finished_at: None, disconnected: false } })
    }

    /// Drain pending changes. Returns true if the set of units changed
    /// (a re-layout is needed).
    pub fn poll(&mut self) -> bool {
        let Mode::Live { rx, finished_at, disconnected } = &mut self.mode else { return false };
        let mut structural = false;
        loop {
            match rx.try_recv() {
                Ok(LiveMsg::UnitNew(u)) => {
                    if !self.tl.units.contains_key(&u.name) {
                        self.tl.units.insert(u.name.clone(), u);
                        structural = true;
                    }
                }
                Ok(LiveMsg::Job(name)) => {
                    let u = self.tl.units.entry(name.clone()).or_insert_with(|| bootmap_core::Unit::new(name));
                    if !u.job && u.t_activating.is_none() {
                        structural = true;
                    }
                    u.job = true;
                }
                Ok(LiveMsg::Event(ev)) => {
                    let first_start = self.tl.units.get(&ev.unit).is_none_or(|u| u.t_activating.is_none());
                    self.tl.apply(ev);
                    // Timestamps feed the layout ordering; re-layout on first start.
                    structural |= first_start;
                }
                Ok(LiveMsg::Finished(ph)) => {
                    self.tl.phases = ph;
                    finished_at.get_or_insert_with(Instant::now);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    *disconnected = true;
                    break;
                }
            }
        }
        structural
    }

    /// Current time on the boot clock (monotonic µs).
    pub fn now(&self) -> Usec {
        match &self.mode {
            Mode::Replay { speed, base, started, paused_at } => paused_at.unwrap_or_else(|| {
                let (_, end) = self.tl.span();
                (base + (started.elapsed().as_secs_f64() * *speed as f64 * 1e6) as Usec).min(end)
            }),
            Mode::Live { .. } => monotonic_now(),
        }
    }

    /// Seconds since the boot finished (None if not finished yet).
    pub fn finished_for(&self) -> Option<f32> {
        match &self.mode {
            Mode::Live { finished_at, .. } => finished_at.map(|t| t.elapsed().as_secs_f32()),
            Mode::Replay { .. } => {
                let (_, end) = self.tl.span();
                let now = self.now();
                (now >= end).then_some(0.0)
            }
        }
    }

    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub fn toggle_pause(&mut self) {
        let now = self.now();
        if let Mode::Replay { speed, base, started, paused_at } = &mut self.mode {
            match paused_at.take() {
                Some(t) => {
                    *base = t;
                    *started = Instant::now();
                }
                None => *paused_at = Some(now),
            }
            let _ = speed;
        }
    }

    /// Jump by `secs` of boot time (replay only).
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub fn seek(&mut self, secs: f32) {
        let now = self.now();
        let (start, end) = self.tl.span();
        let lo = start.saturating_sub(200_000);
        if let Mode::Replay { base, started, paused_at, .. } = &mut self.mode {
            let t = (now as i64 + (secs * 1e6) as i64).clamp(lo as i64, end as i64) as Usec;
            match paused_at {
                Some(p) => *p = t,
                None => {
                    *base = t;
                    *started = Instant::now();
                }
            }
        }
    }

    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub fn restart(&mut self) {
        if let Mode::Replay { base, started, paused_at, .. } = &mut self.mode {
            let (start, _) = self.tl.span();
            *base = start.saturating_sub(200_000);
            *started = Instant::now();
            if paused_at.is_some() {
                *paused_at = Some(*base);
            }
        }
    }
}

/// Owns layout, physics, camera and renderer; one `frame()` per redraw.
pub struct Scene {
    pub source: Source,
    opts: LayoutOptions,
    pub style: Style,
    pub layout: Layout,
    pub sim: Sim,
    pos: Vec<(f32, f32)>,
    /// Radial mode: polar positions to tween from after a re-layout.
    from: HashMap<String, (f32, f32)>,
    tween_start: Instant,
    pub renderer: Renderer,
    pub cam: Camera,
    /// Keep the whole graph in view (off once the user pans or zooms).
    pub auto_fit: bool,
    snap: bool,
    /// Hovered / dragged unit, by name so they survive re-layouts.
    pub hover: Option<String>,
    pub dragging: Option<String>,
    epoch: Instant,
}

const TWEEN_SECS: f32 = 0.6;
/// Physics steps per drawn frame (settles in ~2.5 s at 60 fps).
const TICKS_PER_FRAME: usize = 2;

impl Scene {
    pub fn new(source: Source, graph: &GraphArgs) -> Self {
        let opts = graph.layout_options();
        let layout = Layout::compute(&source.tl, &opts);
        let sim = Sim::new(&layout, ForceParams { radial: graph.radial, ..Default::default() });
        let pos = layout.nodes.iter().map(|n| n.xy()).collect();
        Self {
            style: graph.style(),
            sim,
            pos,
            layout,
            from: HashMap::new(),
            tween_start: Instant::now(),
            renderer: Renderer::new(graph.render_options()),
            cam: Camera { cx: 0.0, cy: 0.0, scale: 50.0 },
            auto_fit: true,
            snap: true,
            hover: None,
            dragging: None,
            source,
            opts,
            epoch: Instant::now(),
        }
    }

    fn relayout(&mut self) {
        self.from = self
            .layout
            .nodes
            .iter()
            .zip(&self.pos)
            .map(|(n, &(x, y))| (n.name.clone(), ((x * x + y * y).sqrt(), y.atan2(x))))
            .collect();
        self.layout = Layout::compute(&self.source.tl, &self.opts);
        self.sim.rebuild(&self.layout);
        self.tween_start = Instant::now();
    }

    fn index(&self, name: &Option<String>) -> Option<usize> {
        name.as_ref().and_then(|n| self.layout.index.get(n).copied())
    }

    /// Run physics to rest (for still renders).
    pub fn settle(&mut self) {
        if self.style == Style::Force {
            self.sim.settle(2000);
        }
        self.from.clear();
        self.update_positions();
        self.snap = true;
    }

    fn update_positions(&mut self) {
        match self.style {
            Style::Force => self.pos = self.sim.positions(),
            Style::Radial => {
                let f = (self.tween_start.elapsed().as_secs_f32() / TWEEN_SECS).min(1.0);
                let e = 1.0 - (1.0 - f).powi(3); // ease-out cubic
                self.pos = self
                    .layout
                    .nodes
                    .iter()
                    .map(|n| match self.from.get(&n.name) {
                        Some(&(r0, t0)) if f < 1.0 => {
                            let mut dt = (n.theta - t0).rem_euclid(TAU);
                            if dt > PI {
                                dt -= TAU;
                            }
                            let (r, th) = (r0 + (n.r - r0) * e, t0 + dt * e);
                            (r * th.cos(), r * th.sin())
                        }
                        _ => n.xy(),
                    })
                    .collect();
            }
        }
    }

    pub fn set_style(&mut self, style: Style) {
        if style == self.style {
            return;
        }
        // Tween (radial) or relax (force) from wherever nodes are now.
        self.from = self
            .layout
            .nodes
            .iter()
            .zip(&self.pos)
            .map(|(n, &(x, y))| (n.name.clone(), ((x * x + y * y).sqrt(), y.atan2(x))))
            .collect();
        self.tween_start = Instant::now();
        if style == Style::Force {
            // Restart physics from the radial positions.
            self.sim = Sim::new(&self.layout, self.sim.params);
        }
        self.style = style;
        self.auto_fit = true;
    }

    pub fn adjust_radial(&mut self, delta: f32) {
        let r = self.sim.params.radial + delta;
        self.sim.set_radial(r);
    }

    pub fn pick(&self, w: f32, h: f32, px: f32, py: f32) -> Option<usize> {
        self.frame_desc(None).pick(w, h, px, py)
    }

    pub fn node_name(&self, i: usize) -> String {
        self.layout.nodes[i].name.clone()
    }

    /// Start dragging node `i` (force mode only).
    pub fn begin_drag(&mut self, i: usize) {
        if self.style != Style::Force || i == bootmap_core::layout::ROOT {
            return;
        }
        self.dragging = Some(self.node_name(i));
        self.sim.pin(i, Some(self.pos[i]));
        self.sim.alpha_target = 0.3;
        self.sim.reheat(0.3);
    }

    pub fn drag_to(&mut self, world: (f32, f32)) {
        if let Some(i) = self.index(&self.dragging) {
            self.sim.pin(i, Some(world));
        }
    }

    pub fn end_drag(&mut self) {
        if let Some(i) = self.index(&self.dragging) {
            self.sim.pin(i, None);
        }
        self.dragging = None;
        self.sim.alpha_target = 0.0;
    }

    fn frame_desc(&self, hover: Option<usize>) -> Frame<'_> {
        Frame {
            lay: &self.layout,
            pos: &self.pos,
            style: self.style,
            cam: self.cam,
            hover,
            rings: match self.style {
                Style::Radial => 1.0,
                Style::Force => self.sim.params.radial * 0.8,
            },
        }
    }

    fn update_camera(&mut self, w: f32, h: f32) {
        if !self.auto_fit {
            return;
        }
        let margin = 0.05 * w.min(h) + if self.renderer.opts.labels { 70.0 } else { 20.0 };
        let target = match self.style {
            Style::Radial => {
                // Fit the outer ring, not the momentary node spread.
                let ext = bootmap_core::layout::radius(self.layout.max_depth);
                Camera::fit_centered(&[(ext, ext)], w, h, margin)
            }
            Style::Force => Camera::fit(&self.pos, w, h, margin),
        };
        self.cam = if self.snap { target } else { self.cam.lerp(target, 0.08) };
        self.snap = false;
    }

    /// Advance (data, physics, camera) and draw one frame at the source's time.
    pub fn frame(&mut self, pm: &mut tiny_skia::Pixmap) {
        if self.source.poll() {
            self.relayout();
        }
        if self.style == Style::Force {
            for _ in 0..TICKS_PER_FRAME {
                self.sim.tick();
            }
        }
        self.update_positions();
        let t = self.source.now();
        self.draw_at(pm, t);
    }

    /// Draw at a given boot time without advancing anything but the camera.
    pub fn draw_at(&mut self, pm: &mut tiny_skia::Pixmap, t: Usec) {
        self.update_camera(pm.width() as f32, pm.height() as f32);
        let wall = self.epoch.elapsed().as_secs_f32();
        let hover = self.index(&self.hover);
        let frame = Frame {
            lay: &self.layout,
            pos: &self.pos,
            style: self.style,
            cam: self.cam,
            hover,
            rings: match self.style {
                Style::Radial => 1.0,
                Style::Force => self.sim.params.radial * 0.8,
            },
        };
        self.renderer.render(pm, &frame, &self.source.tl, t, wall);
    }
}
