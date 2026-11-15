use anyhow::{bail, Context, Result};
use bootmap_core::render::RenderOptions;
use bootmap_core::systemd::{LiveMsg, Systemd};
use bootmap_core::{BootTimeline, Layout, LayoutOptions, Renderer, UnitKind};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::mpsc;

#[cfg(feature = "drm")]
mod kms;
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
    Capture {
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Keep units first started more than this many seconds after boot finished.
        #[arg(long, default_value_t = 30.0)]
        slack: f64,
    },
    /// Render one frame of a recorded boot to PNG.
    Render {
        input: PathBuf,
        /// Seconds after userspace start, or "end".
        #[arg(long, default_value = "end")]
        at: String,
        #[arg(short, long, default_value = "bootmap.png")]
        output: PathBuf,
        #[arg(long, default_value = "1920x1080")]
        size: String,
        #[command(flatten)]
        graph: GraphArgs,
    },
    /// Replay a recording (or watch live, without a file) in a window.
    #[cfg(feature = "window")]
    View {
        input: Option<PathBuf>,
        /// Replay speed multiplier.
        #[arg(long, default_value_t = 0.5)]
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
        #[command(flatten)]
        graph: GraphArgs,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
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
        Cmd::Render { input, at, output, size, graph } => {
            let tl = BootTimeline::load(&input)?;
            let (w, h) = parse_size(&size)?;
            let t = parse_at(&tl, &at)?;
            let lay = Layout::compute(&tl, &graph.layout_options());
            let mut r = Renderer::new(graph.render_options());
            let mut pm = tiny_skia::Pixmap::new(w, h).context("pixmap")?;
            r.render(&mut pm, &lay, &tl, t, 0.25);
            pm.save_png(&output)?;
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
        Cmd::Boot { record, hold, replay, graph } => {
            let source = match replay {
                Some(p) => live::Source::replay(BootTimeline::load(&p)?, 1.0),
                None => live::Source::live()?,
            };
            kms::run(source, graph, record, hold)?;
        }
    }
    Ok(())
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once('x').context("size must look like 1920x1080")?;
    Ok((w.parse()?, h.parse()?))
}

fn parse_at(tl: &BootTimeline, at: &str) -> Result<u64> {
    let (start, end) = tl.span();
    if at == "end" {
        return Ok(end);
    }
    let s: f64 = at.trim_end_matches('s').parse().context("--at must be seconds or \"end\"")?;
    if s < 0.0 {
        bail!("--at must be >= 0");
    }
    Ok(start + (s * 1e6) as u64)
}

/// Helper used by `live`: spawn a thread that feeds systemd changes.
pub fn spawn_watcher(mut sd: Systemd) -> mpsc::Receiver<LiveMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Err(e) = sd.watch(|m| tx.send(m).is_ok()) {
            eprintln!("bootmap: watcher stopped: {e:#}");
        }
    });
    rx
}
