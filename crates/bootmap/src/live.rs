//! Data sources (replay / live) and the scene driver shared by all backends.
//!
//! Each frontend (viewer, boot mode, tools) is a cargo feature and uses part
//! of this; a build with only some of them leaves the rest unused.
#![cfg_attr(
    not(all(feature = "window", feature = "drm", feature = "tools")),
    allow(dead_code)
)]

use crate::GraphArgs;
use anyhow::Result;
use bootmap_core::config::Look;
use bootmap_core::model::Usec;
use bootmap_core::render::{Camera, Frame};
use bootmap_core::systemd::{monotonic_now, LiveMsg, Systemd};
use bootmap_core::{BootTimeline, ForceParams, Layout, LayoutOptions, Renderer, Sim};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Instant;

pub enum Mode {
    Replay {
        speed: f32,
        base: Usec,
        started: Instant,
        paused_at: Option<Usec>,
    },
    Live {
        rx: Receiver<LiveMsg>,
        finished_at: Option<Instant>,
        disconnected: bool,
    },
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
        Self {
            tl,
            mode: Mode::Replay {
                speed,
                base,
                started: Instant::now(),
                paused_at: None,
            },
        }
    }

    pub fn live() -> Result<Self> {
        let mut sd = Systemd::connect()?;
        let tl = sd.snapshot()?;
        let rx = crate::spawn_watcher(sd);
        Ok(Self {
            tl,
            mode: Mode::Live {
                rx,
                finished_at: None,
                disconnected: false,
            },
        })
    }

    /// Shutdown view: when the shutdown began, after marking units that were
    /// already being stopped when we took the snapshot.
    pub fn begin_shutdown(&mut self) -> Usec {
        let now = monotonic_now();
        let from = match self.tl.phases.shutdown_start {
            0 => now,
            t => t.min(now),
        };
        let stopping: Vec<String> = self
            .tl
            .units
            .values()
            .filter(|u| u.state == bootmap_core::UnitState::Deactivating)
            .map(|u| u.name.clone())
            .collect();
        for unit in stopping {
            self.tl.apply(bootmap_core::Event {
                t: now,
                unit,
                state: bootmap_core::UnitState::Deactivating,
            });
        }
        from
    }

    /// Drain pending changes. Returns true if the set of units changed
    /// (a re-layout is needed).
    pub fn poll(&mut self) -> bool {
        let Mode::Live {
            rx,
            finished_at,
            disconnected,
        } = &mut self.mode
        else {
            return false;
        };
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
                    let u = self
                        .tl
                        .units
                        .entry(name.clone())
                        .or_insert_with(|| bootmap_core::Unit::new(name));
                    if !u.job && u.t_activating.is_none() {
                        structural = true;
                    }
                    u.job = true;
                }
                Ok(LiveMsg::JobDone(name)) => {
                    if let Some(u) = self.tl.units.get_mut(&name) {
                        // Never started: it leaves the graph (layouts only
                        // keep units that started or still have a job).
                        structural |= u.job && u.t_activating.is_none();
                        u.job = false;
                    }
                }
                Ok(LiveMsg::Event(ev)) => {
                    let first_start = self
                        .tl
                        .units
                        .get(&ev.unit)
                        .is_none_or(|u| u.t_activating.is_none());
                    // Timestamps feed the layout ordering; re-layout on first start.
                    // A unit that fails leaves its template group: re-lay out then too.
                    let failed = ev.state == bootmap_core::UnitState::Failed;
                    structural |= self.tl.apply(ev) && (first_start || failed);
                }
                Ok(LiveMsg::Finished(_)) if self.tl.phases.in_initrd => {}
                Ok(LiveMsg::Finished(ph)) => {
                    self.tl.phases = ph;
                    finished_at.get_or_insert_with(Instant::now);
                }
                Ok(LiveMsg::Resync(new)) => {
                    self.tl.merge_snapshot(*new, monotonic_now());
                    *disconnected = false;
                    if self.tl.phases.finish > 0 {
                        finished_at.get_or_insert_with(Instant::now);
                    }
                    structural = true;
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

    /// After chrooting into the real root: look for the re-executed PID 1 on
    /// a new channel. The gap must not look like "PID 1 is gone", so
    /// `disconnected` is reset now; if reconnecting fails for 60 s the channel
    /// closes and the usual disconnect path takes over again.
    #[cfg(feature = "drm")]
    pub fn reconnect(&mut self) {
        if let Mode::Live {
            rx, disconnected, ..
        } = &mut self.mode
        {
            *rx = crate::spawn_reconnecting_watcher();
            *disconnected = false;
        }
    }

    /// Current time on the boot clock (monotonic µs).
    pub fn now(&self) -> Usec {
        match &self.mode {
            Mode::Replay {
                speed,
                base,
                started,
                paused_at,
            } => paused_at.unwrap_or_else(|| {
                let (_, end) = self.tl.span();
                // Run on past the last event until the picture has settled.
                let end = end + bootmap_core::render::SETTLE_USEC;
                (base + (started.elapsed().as_secs_f64() * *speed as f64 * 1e6) as Usec).min(end)
            }),
            Mode::Live { .. } => monotonic_now(),
        }
    }

    /// Seconds since the boot finished (None if not finished yet).
    pub fn finished_for(&self) -> Option<f32> {
        if self.tl.phases.in_initrd {
            return None;
        }
        match &self.mode {
            Mode::Live { finished_at, .. } => finished_at.map(|t| t.elapsed().as_secs_f32()),
            Mode::Replay { .. } => {
                let (_, end) = self.tl.span();
                let now = self.now();
                (now >= end).then_some(0.0)
            }
        }
    }

    pub fn toggle_pause(&mut self) {
        let now = self.now();
        if let Mode::Replay {
            speed,
            base,
            started,
            paused_at,
        } = &mut self.mode
        {
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
    pub fn seek(&mut self, secs: f32) {
        let now = self.now();
        let (start, end) = self.tl.span();
        let end = end + bootmap_core::render::SETTLE_USEC;
        let lo = start.saturating_sub(200_000);
        if let Mode::Replay {
            base,
            started,
            paused_at,
            ..
        } = &mut self.mode
        {
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

    pub fn restart(&mut self) {
        if let Mode::Replay {
            base,
            started,
            paused_at,
            ..
        } = &mut self.mode
        {
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
    pub layout: Layout,
    pub sim: Sim,
    /// World position of each layout node, from the simulation.
    pos: Vec<(f32, f32)>,
    pub renderer: Renderer,
    pub cam: Camera,
    /// Keep the whole graph in view (off once the user pans or zooms).
    pub auto_fit: bool,
    /// Prune mode, experimental: fit only what has not been pruned, so the
    /// view zooms in as the graph burns down (`z` in the viewer).
    pub fit_remaining: bool,
    /// Prune mode: each node's opacity at the last physics update.
    fade: Option<Vec<f32>>,
    snap: bool,
    /// Hovered / dragged unit, by name so they survive re-layouts.
    pub hover: Option<String>,
    pub dragging: Option<String>,
    epoch: Instant,
    /// Frame timing, printed when BOOTMAP_STATS is set.
    stats: Option<Stats>,
}

/// Frame rate the viewer runs at, and the physics and camera are tuned for.
pub const VIEWER_FPS: f32 = 60.0;
/// Fitting what is left after pruning (`fit_remaining`) zooms in at most
/// this much further than fitting the whole graph.
const MAX_ZOOM_IN: f32 = 3.0;
/// Physics steps per drawn frame (settles in ~2.5 s at 60 fps).
const TICKS_PER_FRAME: usize = 2;

impl Scene {
    pub fn new(source: Source, graph: &GraphArgs, look: Look) -> Self {
        let mut opts = graph.layout_options();
        opts.prune = look.mode == bootmap_core::config::Mode::Prune;
        let layout = Layout::compute(&source.tl, &opts);
        let sim = Sim::new(
            &layout,
            ForceParams {
                radial: graph.radial,
                ..Default::default()
            },
        );
        let pos = sim.positions();
        Self {
            sim,
            pos,
            layout,
            renderer: Renderer::new(graph.render_options(), look),
            cam: Camera {
                cx: 0.0,
                cy: 0.0,
                scale: 50.0,
            },
            auto_fit: true,
            fit_remaining: false,
            fade: None,
            snap: true,
            hover: None,
            dragging: None,
            source,
            opts,
            epoch: Instant::now(),
            stats: std::env::var_os("BOOTMAP_STATS").map(|_| Stats::default()),
        }
    }

    /// The set of units changed: recompute the tree. The simulation keeps
    /// known units where they are and lets new ones drift in.
    fn relayout(&mut self) {
        self.layout = Layout::compute(&self.source.tl, &self.opts);
        self.sim.rebuild(&self.layout);
    }

    fn index(&self, name: &Option<String>) -> Option<usize> {
        name.as_ref()
            .and_then(|n| self.layout.index.get(n).copied())
    }

    /// Prune mode: let the physics know which units are fading out or gone
    /// at boot time `t`, so the rest move into the room they leave.
    pub fn update_presence(&mut self, t: Usec) {
        self.fade = self.renderer.fade(&self.layout, &self.source.tl, t);
        if let Some(fade) = &self.fade {
            self.sim.set_presence(fade);
        }
    }

    /// Run physics to rest (for still renders).
    pub fn settle(&mut self) {
        self.sim.settle(2000);
        self.pos = self.sim.positions();
        self.snap = true;
    }

    pub fn adjust_radial(&mut self, delta: f32) {
        let r = self.sim.params.radial + delta;
        self.sim.set_radial(r);
    }

    pub fn pick(&self, w: f32, h: f32, px: f32, py: f32) -> Option<usize> {
        // Pointer reach scales with the screen, like the dots. Pruned units
        // are gone: the pointer finds what is still there.
        let fade = self
            .renderer
            .fade(&self.layout, &self.source.tl, self.source.now());
        self.frame_desc(None)
            .pick_where(w, h, px, py, 14.0 * self.renderer.ui_scale, |i| {
                fade.as_ref().is_none_or(|f| f[i] > 0.0)
            })
    }

    pub fn node_name(&self, i: usize) -> String {
        self.layout.nodes[i].name.clone()
    }

    /// Start dragging node `i`. The center node stays put.
    pub fn begin_drag(&mut self, i: usize) {
        if i == bootmap_core::layout::ROOT {
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
            cam: self.cam,
            hover,
        }
    }

    fn update_camera(&mut self, w: f32, h: f32) {
        if !self.auto_fit {
            return;
        }
        let label_room = if self.renderer.opts.labels {
            70.0
        } else {
            20.0
        };
        let margin = 0.05 * w.min(h) + label_room * self.renderer.ui_scale;
        let whole = Camera::fit(&self.pos, w, h, margin);
        let target = match self.fade.as_ref().filter(|_| self.fit_remaining) {
            Some(fade) => {
                let left: Vec<(f32, f32)> = self
                    .pos
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| fade.get(*i).is_none_or(|&a| a > 0.0))
                    .map(|(_, &p)| p)
                    .collect();
                // Both fits center on the same point: only the zoom differs.
                let mut cam = Camera::fit(&left, w, h, margin);
                cam.scale = cam.scale.min(whole.scale * MAX_ZOOM_IN);
                cam
            }
            None => whole,
        };
        self.cam = if self.snap {
            target
        } else {
            self.cam.lerp(target, 0.08)
        };
        self.snap = false;
    }

    /// Advance (data, physics, camera) and draw one frame at the source's time.
    pub fn frame(&mut self, pm: &mut tiny_skia::Pixmap) {
        let t0 = Instant::now();
        if self.source.poll() {
            self.relayout();
        }
        let t = self.source.now();
        self.update_presence(t);
        for _ in 0..TICKS_PER_FRAME {
            self.sim.tick();
        }
        self.pos = self.sim.positions();
        let t1 = Instant::now();
        self.draw_at(pm, t);
        if let Some(st) = &mut self.stats {
            st.record(t1 - t0, t1.elapsed(), pm.width(), pm.height());
        }
    }

    /// Draw at a given boot time without advancing anything but the camera.
    pub fn draw_at(&mut self, pm: &mut tiny_skia::Pixmap, t: Usec) {
        self.update_camera(pm.width() as f32, pm.height() as f32);
        let wall = self.epoch.elapsed().as_secs_f32();
        self.draw(pm, t, wall);
    }

    /// Advance physics and camera by `n` viewer frames (at `VIEWER_FPS`),
    /// on a fixed clock instead of the wall clock: for rendering an animation
    /// that moves the way the viewer does. `t` is the boot time being drawn.
    pub fn advance(&mut self, n: u32, w: f32, h: f32, t: Usec) {
        for _ in 0..n {
            if self.source.poll() {
                self.relayout();
            }
            self.update_presence(t);
            for _ in 0..TICKS_PER_FRAME {
                self.sim.tick();
            }
            self.pos = self.sim.positions();
            self.update_camera(w, h);
        }
    }

    /// Draw as things stand, at boot time `t`; `wall` (seconds) drives the
    /// pulsing of units that are starting.
    pub fn draw(&mut self, pm: &mut tiny_skia::Pixmap, t: Usec, wall: f32) {
        let frame = Frame {
            lay: &self.layout,
            pos: &self.pos,
            cam: self.cam,
            hover: self.index(&self.hover),
        };
        self.renderer.render(pm, &frame, &self.source.tl, t, wall);
    }
}

/// Rolling frame-time statistics (BOOTMAP_STATS=1).
#[derive(Default)]
struct Stats {
    frames: u32,
    update: std::time::Duration,
    draw: std::time::Duration,
    since: Option<Instant>,
}

impl Stats {
    fn record(&mut self, update: std::time::Duration, draw: std::time::Duration, w: u32, h: u32) {
        let since = *self.since.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.update += update;
        self.draw += draw;
        let el = since.elapsed();
        if el.as_secs_f32() >= 2.0 {
            let n = self.frames as f32;
            eprintln!(
                "bootmap: {w}x{h}: {:.1} fps · per frame: update {:.2} ms, draw {:.2} ms",
                n / el.as_secs_f32(),
                self.update.as_secs_f32() * 1e3 / n,
                self.draw.as_secs_f32() * 1e3 / n,
            );
            *self = Stats::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bootmap_core::{Event, UnitState};
    use std::sync::mpsc;

    fn live_source() -> (Source, mpsc::Sender<LiveMsg>) {
        let (tx, rx) = mpsc::channel();
        let src = Source {
            tl: BootTimeline::default(),
            mode: Mode::Live {
                rx,
                finished_at: None,
                disconnected: false,
            },
        };
        (src, tx)
    }

    fn in_graph(src: &Source, unit: &str) -> bool {
        Layout::compute(&src.tl, &LayoutOptions::default())
            .index
            .contains_key(unit)
    }

    #[test]
    fn a_skipped_job_is_no_longer_pending() {
        // systemd queues the job, then skips it (unmet condition): JobNew, JobRemoved.
        let (mut src, tx) = live_source();
        let unit = "modprobe@configfs.service";
        tx.send(LiveMsg::Job(unit.into())).unwrap();
        assert!(src.poll(), "a newly queued unit joins the graph");
        assert!(in_graph(&src, unit));
        tx.send(LiveMsg::JobDone(unit.into())).unwrap();
        assert!(src.poll(), "and leaves it when its job is skipped");
        assert!(!src.tl.units[unit].job);
        assert!(!in_graph(&src, unit));
    }

    #[test]
    fn a_job_that_ran_keeps_its_unit() {
        let (mut src, tx) = live_source();
        let unit = "systemd-sysctl.service";
        tx.send(LiveMsg::Job(unit.into())).unwrap();
        tx.send(LiveMsg::Event(Event {
            t: 100,
            unit: unit.into(),
            state: UnitState::Activating,
        }))
        .unwrap();
        tx.send(LiveMsg::Event(Event {
            t: 200,
            unit: unit.into(),
            state: UnitState::Active,
        }))
        .unwrap();
        src.poll();
        tx.send(LiveMsg::JobDone(unit.into())).unwrap();
        assert!(!src.poll(), "a finished job changes nothing structural");
        assert!(in_graph(&src, unit));
    }
}
