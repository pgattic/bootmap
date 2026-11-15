//! Data sources (replay / live) and the scene driver shared by all backends.

use crate::GraphArgs;
use anyhow::Result;
use bootmap_core::model::Usec;
use bootmap_core::systemd::{monotonic_now, LiveMsg, Systemd};
use bootmap_core::{BootTimeline, Layout, LayoutOptions, Renderer};
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

/// Owns the layout and renderer and smoothly animates between layouts.
pub struct Scene {
    pub source: Source,
    opts: LayoutOptions,
    target: Layout,
    shown: Layout,
    from: HashMap<String, (f32, f32)>,
    tween_start: Instant,
    pub renderer: Renderer,
    epoch: Instant,
}

const TWEEN_SECS: f32 = 0.6;

impl Scene {
    pub fn new(source: Source, graph: &GraphArgs) -> Self {
        let opts = graph.layout_options();
        let target = Layout::compute(&source.tl, &opts);
        Self {
            shown: target.clone(),
            target,
            from: HashMap::new(),
            tween_start: Instant::now(),
            renderer: Renderer::new(graph.render_options()),
            source,
            opts,
            epoch: Instant::now(),
        }
    }

    fn relayout(&mut self) {
        self.from = self.shown.nodes.iter().map(|n| (n.name.clone(), (n.r, n.theta))).collect();
        self.target = Layout::compute(&self.source.tl, &self.opts);
        self.tween_start = Instant::now();
    }

    fn update_tween(&mut self) {
        let f = (self.tween_start.elapsed().as_secs_f32() / TWEEN_SECS).min(1.0);
        if f >= 1.0 && self.from.is_empty() {
            return;
        }
        let e = 1.0 - (1.0 - f).powi(3); // ease-out cubic
        self.shown = self.target.clone();
        let mut delta: HashMap<usize, (f32, f32)> = HashMap::new();
        for (i, n) in self.shown.nodes.iter_mut().enumerate() {
            if let Some(&(r0, t0)) = self.from.get(&n.name) {
                let mut dt = (n.theta - t0).rem_euclid(TAU);
                if dt > PI {
                    dt -= TAU;
                }
                let (r, th) = (r0 + (n.r - r0) * e, t0 + dt * e);
                delta.insert(i, (r - n.r, th - n.theta));
                n.r = r;
                n.theta = th;
            }
        }
        for p in &mut self.shown.procs {
            if let Some(&(dr, dth)) = delta.get(&p.unit) {
                p.r += dr;
                p.theta += dth;
            }
        }
        if f >= 1.0 {
            self.from.clear();
        }
    }

    /// Advance and draw one frame.
    pub fn frame(&mut self, pm: &mut tiny_skia::Pixmap) {
        if self.source.poll() {
            self.relayout();
        }
        self.update_tween();
        let t = self.source.now();
        let wall = self.epoch.elapsed().as_secs_f32();
        self.renderer.render(pm, &self.shown, &self.source.tl, t, wall);
    }
}
