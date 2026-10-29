//! Renderer-neutral frame description and the backends that draw it.
//!
//! The UI composes a [`Scene`]: an ordered list of drawing operations in
//! physical pixels. [`GridShaper`] owns fonts, metrics and OpenType shaping and
//! anchors every shaped cluster to its terminal cell, so all backends draw the
//! same glyphs at the same integer positions. The [`software`] backend is the
//! reference (and the mandatory fallback); a GPU backend must match its output
//! within the documented golden tolerance.
use crate::config::Config;
use crate::model::{Cell, ColorSpec};
use cosmic_text::Color;
use std::borrow::Cow;
use std::sync::Arc;

#[cfg(test)]
pub(crate) mod golden;
pub mod shaper;
pub mod software;

pub use shaper::{GridShaper, PlacedGlyph, ShapedRow};

/// Physical-pixel rectangle; may extend outside the frame (callers clip).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}
impl Rect {
    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < self.x as f64 + self.w as f64
            && y < self.y as f64 + self.h as f64
    }
    pub fn intersection(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x as i64 + self.w as i64).min(other.x as i64 + other.w as i64);
        let bottom = (self.y as i64 + self.h as i64).min(other.y as i64 + other.h as i64);
        Self {
            x,
            y,
            w: (right - x as i64).max(0) as u32,
            h: (bottom - y as i64).max(0) as u32,
        }
    }
    pub fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// One drawing operation. Operations are applied in order; later ones cover
/// earlier ones exactly as the software framebuffer does.
#[derive(Debug, Clone)]
pub enum Op<'a> {
    /// Opaque fill, clipped to the target.
    Fill { rect: Rect, color: u32 },
    /// Shaped text (positions already resolved), alpha-blended inside `clip`.
    Glyphs {
        clip: Rect,
        glyphs: Vec<PlacedGlyph>,
    },
    /// One terminal row of `cells` at (`x`, `y`), clipped to `clip`. Backends
    /// cache the shaped/rasterized row by (`pane`, `row`) and the cell contents.
    Row {
        pane: u64,
        row: usize,
        cells: Cow<'a, [Cell]>,
        x: i32,
        y: i32,
        clip: Rect,
    },
    /// Invert RGB inside `rect` (block cursor). `rect` is already clipped.
    Invert { rect: Rect },
    /// A cached sub-frame (crow panel) drawn at `rect`, clipped to `clip`.
    Layer {
        rect: Rect,
        clip: Rect,
        layer: Arc<Layer>,
    },
}

/// Independently cached content in its own coordinate space: a background
/// and fills/glyphs relative to the layer origin. `key` changes whenever the
/// content, size, scale or colors change.
#[derive(Debug)]
pub struct Layer {
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub background: u32,
    pub ops: Vec<Op<'static>>,
}

/// A complete frame.
#[derive(Debug, Default)]
pub struct Scene<'a> {
    pub width: u32,
    pub height: u32,
    pub background: u32,
    /// Panes drawn in this frame; row caches of other panes are dropped.
    pub live_panes: Vec<u64>,
    pub ops: Vec<Op<'a>>,
}

pub(crate) fn parse_color(value: &str, fallback: u32) -> u32 {
    value
        .strip_prefix('#')
        .filter(|s| s.len() == 6)
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .unwrap_or(fallback)
}
pub(crate) fn cosmic_color(value: u32) -> Color {
    Color::rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}
pub(crate) fn resolve_color(config: &Config, color: ColorSpec) -> u32 {
    match color {
        ColorSpec::Foreground => parse_color(&config.foreground, 0xeaeaea),
        ColorSpec::Background => parse_color(&config.background, 0x111111),
        ColorSpec::Rgb(r, g, b) => (r as u32) << 16 | (g as u32) << 8 | b as u32,
        ColorSpec::Indexed(i) if i < 16 => config
            .palette
            .get(i as usize)
            .map(|s| parse_color(s, 0xeaeaea))
            .unwrap_or(0xeaeaea),
        ColorSpec::Indexed(i) if i < 232 => {
            let i = i as u32 - 16;
            let v = |n| if n == 0 { 0 } else { 55 + 40 * n };
            v(i / 36) << 16 | v(i / 6 % 6) << 8 | v(i % 6)
        }
        ColorSpec::Indexed(i) => {
            let c = 8 + 10 * (i as u32 - 232);
            c << 16 | c << 8 | c
        }
    }
}
