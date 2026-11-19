#[cfg(feature = "tools")]
use anyhow::Context;
use anyhow::Result;
use bootmap_core::render::RenderOptions;
use bootmap_core::systemd::{LiveMsg, Systemd};
use bootmap_core::{BootTimeline, LayoutOptions, UnitKind};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::mpsc;

#[cfg(feature = "drm")]
mod ask;
#[cfg(feature = "drm")]
mod display;
#[cfg(feature = "drm")]
mod kms;
#[cfg(feature = "drm")]
mod vt;
mod live;
#[cfg(feature = "window")]
mod window;

#[derive(Parser)]
#[command(version, about = "Graphical radial systemd boot visualizer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
pub struct GraphArgs {
    /// Also show *.device units (hidden by default; there are many).
    #[arg(long)]
    devices: bool,
    /// Also show *.slice and *.scope units.
    #[arg(long)]
    slices: bool,
    /// Include units that never started this boot.
    #[arg(long)]
    all: bool,
    /// Hide process satellites.
    #[arg(long)]
    no_processes: bool,
    /// Hide faint non-tree dependency edges.
    #[arg(long)]
    no_extra_edges: bool,
    /// Pull toward each unit's dependency depth: 0 lets the graph float
    /// freely, 1 keeps early units near the center and late ones outside.
    #[arg(long, default_value_t = 0.5)]
    pub radial: f32,
}

impl GraphArgs {
    pub fn layout_options(&self) -> LayoutOptions {
        let mut o = LayoutOptions::default();
        if self.devices {
            o.hidden_kinds.remove(&UnitKind::Device);
        }
        if self.slices {
            o.hidden_kinds.remove(&UnitKind::Slice);
            o.hidden_kinds.remove(&UnitKind::Scope);
        }
        o.include_never_started = self.all;
        o.processes = !self.no_processes;
        o
    }
    pub fn render_options(&self) -> RenderOptions {
        RenderOptions { extra_edges: !self.no_extra_edges, ..Default::default() }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Snapshot the current boot to JSON.
    #[cfg(feature = "tools")]
    Capture {
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Keep units first started more than this many seconds after boot finished.
        #[arg(long, default_value_t = 30.0)]
        slack: f64,
    },
    /// Render one frame of a recorded boot to PNG.
    #[cfg(feature = "tools")]
    Render {
        input: PathBuf,
        /// Seconds after userspace start (negative: into the initrd), or "end"
        /// (the final picture, once it has settled).
        #[arg(long, default_value = "end", allow_hyphen_values = true)]
        at: String,
        #[arg(short, long, default_value = "bootmap.png")]
        output: PathBuf,
        /// Highlight a unit and its neighbors, as when hovering it in the viewer.
        #[arg(long)]
        highlight: Option<String>,
        /// Size of text and dots relative to a ~96 dpi screen (default: the
        /// image height relative to 1080).
        #[arg(long)]
        ui_scale: Option<f32>,
        #[arg(long, default_value = "1920x1080")]
        size: String,
        #[command(flatten)]
        graph: GraphArgs,
    },
    /// Replay a recording (or watch live, without a file) in a window.
    #[cfg(feature = "window")]
    View {
        input: Option<PathBuf>,
        /// Replay speed multiplier (0.5 = half speed).
        #[arg(long, default_value_t = 1.0)]
        speed: f32,
        #[command(flatten)]
        graph: GraphArgs,
    },
    /// Boot-time mode: draw the live boot directly on the display via KMS.
    #[cfg(feature = "drm")]
    Boot {
        /// Where to save the recorded timeline once boot finishes.
        #[arg(long, default_value = "/var/log/bootmap/last.json")]
        record: PathBuf,
        /// Keep showing the finished graph this many seconds (0 = until stopped).
        #[arg(long, default_value_t = 0.0)]
        hold: f32,
        /// Replay a recording instead of watching systemd (for testing on a VT).
        #[arg(long)]
        replay: Option<PathBuf>,
        /// Follow a shutdown instead of a boot: take the display back and show
        /// units stopping until systemd hands over to the final shutdown stage.
        #[arg(long)]
        shutdown: bool,
        /// Virtual terminal to draw on. Not the console's, so that a login
        /// prompt starting there does not take the screen back.
        #[arg(long, default_value_t = 12)]
        vt: u32,
        /// File created once the display is released (on SIGUSR1).
        #[arg(long, default_value = "/run/bootmap/released")]
        released_marker: PathBuf,
        #[command(flatten)]
        graph: GraphArgs,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        #[cfg(feature = "tools")]
        Cmd::Capture { output, slack } => {
            let mut sd = Systemd::connect()?;
            let mut tl = sd.snapshot()?;
            if tl.phases.finish > 0 {
                let cutoff = tl.phases.finish + (slack * 1e6) as u64;
                tl.units.retain(|_, u| u.t_activating.is_none_or(|t| t <= cutoff));
                tl.synthesize_events();
            }
            eprintln!(
                "captured {} units, {} events (via {})",
                tl.units.len(),
                tl.events.len(),
                if sd.is_private() { "private socket" } else { "system bus" }
            );
            match output {
                Some(p) => std::fs::write(&p, tl.to_json()).with_context(|| format!("writing {}", p.display()))?,
                None => println!("{}", tl.to_json()),
            }
        }
        #[cfg(feature = "tools")]
        Cmd::Render { input, at, output, size, highlight, ui_scale, graph } => {
            let tl = BootTimeline::load(&input)?;
            let (w, h) = parse_size(&size)?;
            let t = parse_at(&tl, &at)?;
            let mut scene = live::Scene::new(live::Source::replay(tl, 1.0), &graph);
            scene.settle();
            scene.hover = highlight;
            scene.renderer.ui_scale = ui_scale.unwrap_or((h as f32 / 1080.0).max(1.0));
            let mut pm = tiny_skia::Pixmap::new(w, h).context("pixmap")?;
            scene.draw_at(&mut pm, t);
            if std::env::var_os("BOOTMAP_STATS").is_some() {
                // Benchmark: animate the replay for a few seconds, printing frame times.
                let start = std::time::Instant::now();
                while start.elapsed().as_secs_f32() < 4.5 {
                    scene.frame(&mut pm);
                }
            }
            pm.save_png(&output)?;
            let lay = &scene.layout;
            eprintln!("wrote {} ({} nodes, depth {})", output.display(), lay.nodes.len() - 1, lay.max_depth);
        }
        #[cfg(feature = "window")]
        Cmd::View { input, speed, graph } => {
            let source = match input {
                Some(p) => live::Source::replay(BootTimeline::load(&p)?, speed),
                None => live::Source::live()?,
            };
            window::run(source, graph)?;
        }
        #[cfg(feature = "drm")]
        Cmd::Boot { record, hold, replay, shutdown, vt, released_marker, graph } => {
            // Before anything slow: an early SIGUSR1 would otherwise kill us.
            let sig = kms::Signals::install()?;
            let source = match replay {
                Some(p) => live::Source::replay(BootTimeline::load(&p)?, 1.0),
                None => live::Source::live()?,
            };
            kms::run(sig, source, graph, kms::BootOptions { record, hold, released_marker, shutdown, vt })?;
        }
    }
    Ok(())
}

#[cfg(feature = "tools")]
fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once('x').context("size must look like 1920x1080")?;
    Ok((w.parse()?, h.parse()?))
}

#[cfg(feature = "tools")]
fn parse_at(tl: &BootTimeline, at: &str) -> Result<u64> {
    let (start, end) = tl.span();
    if at == "end" {
        // The settled final picture, after the last labels have faded.
        return Ok(end + bootmap_core::render::SETTLE_USEC);
    }
    let s: f64 = at.trim_end_matches('s').parse().context("--at must be seconds or \"end\"")?;
    // Relative to the userspace start; negative values reach back into the initrd.
    let origin = if tl.phases.userspace > 0 { tl.phases.userspace } else { start };
    let t = origin as i64 + (s * 1e6) as i64;
    Ok(t.max(start as i64) as u64)
}

/// Helper used by `live`: spawn a thread that feeds systemd changes.
pub fn spawn_watcher(sd: Systemd) -> mpsc::Receiver<LiveMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || watch_into(sd, tx));
    rx
}

fn watch_into(mut sd: Systemd, tx: mpsc::Sender<LiveMsg>) {
    if let Err(e) = sd.watch(|m| tx.send(m).is_ok()) {
        eprintln!("bootmap: watcher stopped: {e:#}");
    }
}

/// After the switch to the real root: retry connecting to the re-executed
/// PID 1 every 100 ms for up to 60 s. On success the thread sends a fresh
/// snapshot (`LiveMsg::Resync`) and then watches like `spawn_watcher`. If the
/// time runs out the thread ends, which closes the channel.
#[cfg(feature = "drm")]
pub fn spawn_reconnecting_watcher() -> mpsc::Receiver<LiveMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(60);
        // Failing is the normal state until PID 1 is back: keep only the last reason.
        let mut last_err = None;
        while Instant::now() < deadline {
            let attempt = Systemd::connect().and_then(|mut sd| Ok((sd.snapshot()?, sd)));
            match attempt {
                Ok((tl, sd)) => {
                    eprintln!("bootmap: reconnected to systemd");
                    if tx.send(LiveMsg::Resync(Box::new(tl))).is_ok() {
                        watch_into(sd, tx);
                    }
                    return;
                }
                Err(e) => last_err = Some(e),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        match last_err {
            Some(e) => eprintln!("bootmap: could not reconnect to systemd: {e:#}"),
            None => eprintln!("bootmap: could not reconnect to systemd"),
        }
    });
    rx
}
