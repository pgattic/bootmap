//! User configuration: how the graph looks.
//!
//! Read from a TOML file (`/etc/bootmap.toml` by default; the NixOS module
//! writes it from `services.bootmap.settings`). Every key is optional:
//!
//! ```toml
//! theme = "black"            # "dark" (default) or "black"
//! text-scale = 1.2           # 0.5 to 3, on top of the automatic screen scaling
//! font = "/path/font.ttf"    # TrueType or OpenType; default: bundled DejaVu Sans
//! center-image = "/path/logo.png"
//! title = "Welcome"          # "" hides it; default: "bootmap · <hostname>"
//!
//! [colors]                   # override any color of the theme: "#rrggbb" or "#rgb"
//! active = "#7ebae4"
//!
//! [show]                     # all default to true
//! status = true              # elapsed time and the per-state counts
//! activity = true            # what is starting now / the slowest units
//! legend = true
//! phases = true              # bottom line: time per boot stage, progress bar
//! labels = true
//! extra-edges = true         # faint lines for dependencies outside the tree
//! ```
//!
//! [`Config::parse`] rejects unknown keys and bad values, so a typo fails
//! `bootmap check-config` (and with it the NixOS rebuild), not the boot.
//! [`Config::resolve`] then loads the font and image files into a [`Look`],
//! which is what the renderer uses.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer};
use std::path::{Path, PathBuf};

/// Where bootmap looks for its configuration when not told otherwise.
pub const DEFAULT_PATH: &str = "/etc/bootmap.toml";

/// An opaque color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb8(pub u8, pub u8, pub u8);

impl Rgb8 {
    /// `#rrggbb` or `#rgb`.
    pub fn parse(s: &str) -> Result<Self> {
        let hex = s.strip_prefix('#').with_context(|| format!("color {s:?} must start with '#'"))?;
        let digit = |c: u8| -> Result<u8> {
            (c as char).to_digit(16).map(|d| d as u8).with_context(|| format!("color {s:?} is not hexadecimal"))
        };
        let b = hex.as_bytes();
        match b.len() {
            6 => Ok(Self(
                digit(b[0])? << 4 | digit(b[1])?,
                digit(b[2])? << 4 | digit(b[3])?,
                digit(b[4])? << 4 | digit(b[5])?,
            )),
            3 => Ok(Self(digit(b[0])? * 17, digit(b[1])? * 17, digit(b[2])? * 17)),
            _ => bail!("color {s:?} must be #rrggbb or #rgb"),
        }
    }
}

impl<'de> Deserialize<'de> for Rgb8 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Rgb8::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    /// Dark blue-grey background (the default).
    #[default]
    Dark,
    /// True black background, for OLED panels and a seamless boot.
    Black,
}

/// Every color the renderer uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Behind everything.
    pub background: Rgb8,
    /// Behind the info panel and the passphrase prompt.
    pub panel: Rgb8,
    /// Main text: titles, labels, counts.
    pub text: Rgb8,
    /// Secondary text: headings, the legend, timings.
    pub muted: Rgb8,
    /// Empty part of the progress bar.
    pub track: Rgb8,
    /// Units not started yet.
    pub pending: Rgb8,
    /// Units starting or stopping (also: the passphrase prompt's accent).
    pub starting: Rgb8,
    /// Units running.
    pub active: Rgb8,
    /// Targets.
    pub target: Rgb8,
    /// Units that ran and exited.
    pub exited: Rgb8,
    /// Failed units, errors.
    pub failed: Rgb8,
    /// Processes of running units.
    pub process: Rgb8,
}

impl Theme {
    pub fn palette(self) -> Palette {
        let dark = Palette {
            background: Rgb8(0x0e, 0x10, 0x16),
            panel: Rgb8(0x16, 0x19, 0x22),
            text: Rgb8(0xd8, 0xdc, 0xe6),
            muted: Rgb8(0x8a, 0x91, 0xa3),
            track: Rgb8(0x23, 0x27, 0x33),
            pending: Rgb8(0x3a, 0x40, 0x4e),
            starting: Rgb8(0xf5, 0xb9, 0x42),
            active: Rgb8(0x4c, 0xc3, 0x8a),
            target: Rgb8(0x6a, 0xa9, 0xff),
            exited: Rgb8(0x5f, 0x8f, 0xa3),
            failed: Rgb8(0xff, 0x5c, 0x5c),
            process: Rgb8(0xc7, 0x9b, 0xff),
        };
        match self {
            Theme::Dark => dark,
            Theme::Black => Palette {
                background: Rgb8(0, 0, 0),
                panel: Rgb8(0x10, 0x10, 0x12),
                track: Rgb8(0x1c, 0x1c, 0x20),
                pending: Rgb8(0x33, 0x36, 0x3e),
                ..dark
            },
        }
    }
}

/// Per-color overrides of the theme. Same names as [`Palette`]'s fields.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Colors {
    pub background: Option<Rgb8>,
    pub panel: Option<Rgb8>,
    pub text: Option<Rgb8>,
    pub muted: Option<Rgb8>,
    pub track: Option<Rgb8>,
    pub pending: Option<Rgb8>,
    pub starting: Option<Rgb8>,
    pub active: Option<Rgb8>,
    pub target: Option<Rgb8>,
    pub exited: Option<Rgb8>,
    pub failed: Option<Rgb8>,
    pub process: Option<Rgb8>,
}

/// Which parts of the picture are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct Show {
    /// Elapsed time and the per-state counts (top left, under the title).
    pub status: bool,
    /// What is starting now, or the slowest units once the boot is done.
    pub activity: bool,
    /// The color legend (top right).
    pub legend: bool,
    /// Bottom line: time spent in each boot stage, and the progress bar.
    pub phases: bool,
    /// Unit labels on the graph.
    pub labels: bool,
    /// Faint lines for dependencies outside the tree.
    pub extra_edges: bool,
}

impl Default for Show {
    fn default() -> Self {
        Self { status: true, activity: true, legend: true, phases: true, labels: true, extra_edges: true }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", default)]
pub struct Config {
    pub theme: Theme,
    pub colors: Colors,
    /// Multiplies the size of all text (not dots or lines). Default 1.
    pub text_scale: Option<f32>,
    /// A TrueType/OpenType font file to use instead of the bundled one.
    pub font: Option<PathBuf>,
    /// A PNG drawn in the center node.
    pub center_image: Option<PathBuf>,
    /// The heading, top left. `""` hides it; unset: "bootmap · <hostname>".
    pub title: Option<String>,
    pub show: Show,
}

/// Everything the renderer needs from the configuration, with files loaded.
#[derive(Debug, Clone)]
pub struct Look {
    pub palette: Palette,
    pub text_scale: f32,
    /// Font file contents; `None` means the bundled font.
    pub font: Option<Vec<u8>>,
    pub center_image: Option<tiny_skia::Pixmap>,
    /// `None`: the default heading. `Some("")`: no heading.
    pub title: Option<String>,
    pub show: Show,
}

impl Default for Look {
    fn default() -> Self {
        Config::default().resolve().expect("the default configuration resolves")
    }
}

impl Config {
    pub fn parse(toml_text: &str) -> Result<Self> {
        let c: Config = toml::from_str(toml_text)?;
        if let Some(s) = c.text_scale {
            if !(0.5..=3.0).contains(&s) {
                bail!("text-scale must be between 0.5 and 3, not {s}");
            }
        }
        Ok(c)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    /// The configuration at `path`, or the defaults if there is no file
    /// there. Problems are reported and fall back to the defaults: at boot a
    /// broken configuration must not stop the picture.
    pub fn load_or_default(path: &Path) -> Look {
        if !path.exists() {
            return Look::default();
        }
        match Self::load(path).and_then(|c| c.resolve()) {
            Ok(look) => look,
            Err(e) => {
                eprintln!("bootmap: ignoring configuration: {e:#}");
                Look::default()
            }
        }
    }

    pub fn palette(&self) -> Palette {
        let mut p = self.theme.palette();
        let c = &self.colors;
        let set = |slot: &mut Rgb8, v: Option<Rgb8>| {
            if let Some(v) = v {
                *slot = v;
            }
        };
        set(&mut p.background, c.background);
        set(&mut p.panel, c.panel);
        set(&mut p.text, c.text);
        set(&mut p.muted, c.muted);
        set(&mut p.track, c.track);
        set(&mut p.pending, c.pending);
        set(&mut p.starting, c.starting);
        set(&mut p.active, c.active);
        set(&mut p.target, c.target);
        set(&mut p.exited, c.exited);
        set(&mut p.failed, c.failed);
        set(&mut p.process, c.process);
        p
    }

    /// Load the files the configuration names, checking they are usable.
    pub fn resolve(&self) -> Result<Look> {
        let font = match &self.font {
            Some(path) => {
                let bytes = std::fs::read(path).with_context(|| format!("reading font {}", path.display()))?;
                fontdue::Font::from_bytes(bytes.as_slice(), fontdue::FontSettings::default())
                    .map_err(|e| anyhow::anyhow!("font {}: {e}", path.display()))?;
                Some(bytes)
            }
            None => None,
        };
        let center_image = match &self.center_image {
            Some(path) => Some(
                tiny_skia::Pixmap::load_png(path)
                    .with_context(|| format!("center image {} (must be a PNG)", path.display()))?,
            ),
            None => None,
        };
        Ok(Look {
            palette: self.palette(),
            text_scale: self.text_scale.unwrap_or(1.0),
            font,
            center_image,
            title: self.title.clone(),
            show: self.show,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_the_default() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.palette(), Theme::Dark.palette());
        assert_eq!(c.show, Show::default());
        let look = c.resolve().unwrap();
        assert_eq!(look.text_scale, 1.0);
        assert!(look.font.is_none() && look.center_image.is_none() && look.title.is_none());
    }

    #[test]
    fn theme_and_overrides() {
        let c = Config::parse(
            r##"
            theme = "black"
            text-scale = 1.5
            title = ""
            [colors]
            active = "#7ebae4"
            failed = "#f00"
            [show]
            legend = false
            extra-edges = false
            "##,
        )
        .unwrap();
        let p = c.palette();
        assert_eq!(p.background, Rgb8(0, 0, 0));
        assert_eq!(p.active, Rgb8(0x7e, 0xba, 0xe4));
        assert_eq!(p.failed, Rgb8(0xff, 0, 0));
        assert_eq!(p.target, Theme::Dark.palette().target, "untouched colors come from the theme");
        assert!(!c.show.legend && !c.show.extra_edges && c.show.labels);
        let look = c.resolve().unwrap();
        assert_eq!(look.text_scale, 1.5);
        assert_eq!(look.title.as_deref(), Some(""));
    }

    #[test]
    fn mistakes_are_rejected() {
        for (bad, why) in [
            ("tehme = \"dark\"", "unknown key"),
            ("theme = \"light\"", "unknown theme"),
            ("[colors]\nactive = \"7ebae4\"", "missing #"),
            ("[colors]\nactive = \"#7ebae\"", "wrong length"),
            ("[colors]\nactive = \"#zzzzzz\"", "not hex"),
            ("[colors]\nactiv = \"#ffffff\"", "unknown color"),
            ("[show]\nlegnd = false", "unknown panel"),
            ("text-scale = 10", "out of range"),
        ] {
            assert!(Config::parse(bad).is_err(), "accepted {bad:?} ({why})");
        }
    }

    #[test]
    fn missing_files_are_errors_when_resolving() {
        let c = Config::parse("center-image = \"/nonexistent/logo.png\"").unwrap();
        assert!(c.resolve().is_err());
        let c = Config::parse("font = \"/nonexistent/font.ttf\"").unwrap();
        assert!(c.resolve().is_err());
    }

    #[test]
    fn missing_config_file_means_defaults() {
        let look = Config::load_or_default(Path::new("/nonexistent/bootmap.toml"));
        assert_eq!(look.palette, Theme::Dark.palette());
    }
}
