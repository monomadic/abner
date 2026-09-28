//! Startup configuration: `abner.default.toml` (compiled in, the complete
//! set of keys) overlaid by at most one user file.
//!
//! The overlay is a deep merge of TOML tables — a user file names only the
//! keys it changes — and the merged table is then deserialized strictly
//! (`deny_unknown_fields`), so a typo'd key is an error rather than a
//! silently ignored setting. Every error names the file it came from;
//! `main` refuses to start on any of them.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::app::Mode;

const DEFAULT_TOML: &str = include_str!("../abner.default.toml");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub window: Window,
    pub playback: Playback,
    pub compare: Compare,
    pub mask: MaskCfg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Playback {
    #[serde(deserialize_with = "de_mode")]
    pub view: Mode,
    pub start_paused: bool,
    pub seek_step: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compare {
    pub delta_gain: f32,
    pub blend: f32,
    pub checker_size: f32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaskCfg {
    pub brush_size: f32,
}

fn de_mode<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Mode, D::Error> {
    let s = String::deserialize(d)?;
    Mode::parse(&s).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "unknown view `{s}`, expected one of: overlay sbs delta split checker blend"
        ))
    })
}

/// Minimum window, shared with main's `with_min_inner_size`.
pub const MIN_WINDOW: (f64, f64) = (720.0, 480.0);

impl Default for Config {
    /// The compiled-in defaults alone. Can't fail at runtime: the test
    /// `default_is_complete_and_valid` parses the same bytes.
    fn default() -> Self {
        Self::from_tables(default_table(), None).expect("abner.default.toml is valid")
    }
}

impl Config {
    /// Resolve and load the configuration for this run. `explicit` is
    /// `--config`'s path. Returns the config and the overlay file used.
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<(Config, Option<PathBuf>)> {
        let path = match explicit {
            Some(p) => {
                if !p.is_file() {
                    bail!("config {}: no such file", p.display());
                }
                Some(p.to_path_buf())
            }
            None => search_paths().into_iter().find(|p| p.is_file()),
        };
        let Some(path) = path else { return Ok((Config::default(), None)) };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("config {}", path.display()))?;
        let cfg = Self::from_overlay_str(&text)
            .with_context(|| format!("config {}", path.display()))?;
        Ok((cfg, Some(path)))
    }

    /// The defaults overlaid with one user file's contents.
    pub fn from_overlay_str(text: &str) -> anyhow::Result<Config> {
        let overlay: toml::Table = toml::from_str(text)?;
        Self::from_tables(default_table(), Some(overlay))
    }

    fn from_tables(mut base: toml::Table, overlay: Option<toml::Table>) -> anyhow::Result<Config> {
        if let Some(o) = overlay {
            merge(&mut base, o, "")?;
        }
        let cfg: Config = toml::Value::Table(base).try_into()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Ranges mirror the live clamps in `App` (`adjust_param`,
    /// `resize_brush`), so a config can't start somewhere the keys
    /// couldn't reach.
    fn validate(&self) -> anyhow::Result<()> {
        fn range(key: &str, v: f64, lo: f64, hi: f64) -> anyhow::Result<()> {
            if !(lo..=hi).contains(&v) {
                bail!("`{key}` = {v} is out of range ({lo}–{hi})");
            }
            Ok(())
        }
        range("window.width", self.window.width, MIN_WINDOW.0, 16384.0)?;
        range("window.height", self.window.height, MIN_WINDOW.1, 16384.0)?;
        range("playback.seek_step", self.playback.seek_step, 0.001, 3600.0)?;
        range("compare.delta_gain", self.compare.delta_gain as f64, 1.0, 64.0)?;
        range("compare.blend", self.compare.blend as f64, 0.0, 1.0)?;
        range("compare.checker_size", self.compare.checker_size as f64, 4.0, 512.0)?;
        range("mask.brush_size", self.mask.brush_size as f64, 1.0, 4096.0)?;
        Ok(())
    }
}

fn default_table() -> toml::Table {
    toml::from_str(DEFAULT_TOML).expect("abner.default.toml parses")
}

/// Search order after `--config`: the working directory, then the two
/// `~/.config` spellings.
fn search_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("abner.toml"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let cfg = PathBuf::from(home).join(".config");
        out.push(cfg.join("abner").join("abner.toml"));
        out.push(cfg.join("abner.toml"));
    }
    out
}

/// Deep-merge `over` into `base`: tables merge key by key, anything else
/// replaces. A table where the default has a scalar (or vice versa) is
/// rejected here, since the type check after the merge would only see
/// the overlay's shape.
fn merge(base: &mut toml::Table, over: toml::Table, prefix: &str) -> anyhow::Result<()> {
    for (k, v) in over {
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o, &key)?,
            (Some(toml::Value::Table(_)), _) => bail!("`{key}` must be a table"),
            (Some(_), toml::Value::Table(_)) => bail!("`{key}` is not a table"),
            (Some(slot), v) => *slot = v,
            (None, _) => bail!("unknown key `{key}`"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str) -> String {
        format!("{:#}", Config::from_overlay_str(text).unwrap_err())
    }

    #[test]
    fn default_is_complete_and_valid() {
        let c = Config::default();
        assert_eq!(c.playback.view, Mode::Overlay);
        assert_eq!((c.window.width, c.window.height), (1280.0, 800.0));
        assert!(!c.playback.start_paused);
    }

    #[test]
    fn empty_overlay_is_the_default() {
        let c = Config::from_overlay_str("").unwrap();
        assert_eq!(c.compare.delta_gain, Config::default().compare.delta_gain);
    }

    #[test]
    fn overlay_changes_only_what_it_names() {
        let c = Config::from_overlay_str("[playback]\nview = \"delta\"\n[compare]\nblend = 0.25\n").unwrap();
        assert_eq!(c.playback.view, Mode::Delta);
        assert_eq!(c.compare.blend, 0.25);
        // Siblings in the same tables keep their defaults.
        assert_eq!(c.playback.seek_step, 1.0);
        assert_eq!(c.compare.checker_size, 48.0);
    }

    #[test]
    fn errors_name_the_key() {
        assert!(err("[playback]\nspeeed = 2\n").contains("playback.speeed"));
        assert!(err("[nope]\n").contains("`nope`"));
        assert!(err("playback = 3\n").contains("`playback` must be a table"));
        assert!(err("[compare]\nblend = 2.0\n").contains("compare.blend"));
        assert!(err("[playback]\nview = \"wide\"\n").contains("unknown view `wide`"));
        assert!(err("[playback]\nseek_step = \"one\"\n").contains("seek_step"));
        // Syntax errors carry toml's line/column report.
        assert!(err("[playback\n").contains("line 1"));
    }

    #[test]
    fn explicit_missing_path_is_an_error() {
        let e = Config::load(Some(Path::new("/nonexistent/abner.toml"))).unwrap_err();
        assert!(format!("{e:#}").contains("no such file"));
    }
}
