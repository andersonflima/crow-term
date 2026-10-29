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
use std::collections::HashMap;
use std::sync::Arc;

pub mod bench;
#[cfg(test)]
pub(crate) mod golden;
pub mod gpu;
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
    pub ops: Vec<Op<'a>>,
}

/// Per-row backend data keyed by row *content*, so a row that moved (scroll,
/// split, window switch back) reuses its shaping/raster instead of being
/// redone. A position index keeps the common case (row unchanged in place) to
/// one comparison. Entries not drawn in the latest frame are dropped, which
/// bounds the cache by the visible rows.
pub(crate) struct RowCache<T> {
    positions: HashMap<(u64, usize), u64>,
    entries: HashMap<u64, Vec<RowEntry<T>>>,
    frame: u64,
}
struct RowEntry<T> {
    cells: Vec<Cell>,
    value: T,
    frame: u64,
}
impl<T> Default for RowCache<T> {
    fn default() -> Self {
        Self {
            positions: HashMap::new(),
            entries: HashMap::new(),
            frame: 0,
        }
    }
}
impl<T> RowCache<T> {
    pub fn clear(&mut self) {
        self.positions.clear();
        self.entries.clear();
    }
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }
    /// Drop every row not used since [`Self::begin_frame`].
    pub fn end_frame(&mut self) {
        let frame = self.frame;
        self.entries.retain(|_, bucket| {
            bucket.retain(|entry| entry.frame == frame);
            !bucket.is_empty()
        });
        let entries = &self.entries;
        self.positions.retain(|_, hash| entries.contains_key(hash));
    }
    /// The value for `cells` drawn at (`pane`, `row`), created by `make` on a miss.
    pub fn get_or_insert_with(
        &mut self,
        pane: u64,
        row: usize,
        cells: &[Cell],
        make: impl FnOnce() -> T,
    ) -> &mut T {
        let frame = self.frame;
        // Fast path: the row is unchanged at its position.
        let cached = self.positions.get(&(pane, row)).and_then(|&hash| {
            let index = self
                .entries
                .get(&hash)?
                .iter()
                .position(|entry| entry.cells == cells)?;
            Some((hash, index))
        });
        let (hash, index) = match cached {
            Some(found) => found,
            None => {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                cells.hash(&mut hasher);
                let hash = hasher.finish();
                self.positions.insert((pane, row), hash);
                let bucket = self.entries.entry(hash).or_default();
                let index = match bucket.iter().position(|entry| entry.cells == cells) {
                    Some(index) => index,
                    None => {
                        bucket.push(RowEntry {
                            cells: cells.to_vec(),
                            value: make(),
                            frame,
                        });
                        bucket.len() - 1
                    }
                };
                (hash, index)
            }
        };
        let bucket = self.entries.get_mut(&hash).expect("row entry present");
        let entry = &mut bucket[index];
        entry.frame = frame;
        &mut entry.value
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UnderlineStyle;
    fn row(text: &str) -> Vec<Cell> {
        text.chars()
            .map(|c| Cell {
                text: c.to_string(),
                fg: ColorSpec::Foreground,
                bg: ColorSpec::Background,
                bold: false,
                italic: false,
                underline: false,
                strike: false,
                wide_spacer: false,
                underline_style: UnderlineStyle::None,
                underline_color: None,
                link: None,
            })
            .collect()
    }
    #[test]
    fn row_cache_reuses_moved_rows_and_drops_undrawn_ones() {
        let mut cache = RowCache::<u32>::default();
        let mut made = 0;
        let mut draw = |cache: &mut RowCache<u32>, rows: &[&str]| {
            cache.begin_frame();
            for (index, text) in rows.iter().enumerate() {
                cache.get_or_insert_with(1, index, &row(text), || {
                    made += 1;
                    made
                });
            }
            cache.end_frame();
        };
        draw(&mut cache, &["a", "b", "c"]);
        // Scrolling by one line only creates the new bottom row.
        draw(&mut cache, &["b", "c", "d"]);
        // Idle frame: nothing new.
        draw(&mut cache, &["b", "c", "d"]);
        drop(draw);
        assert_eq!(made, 4);
        // "a" was not drawn in the last frames: evicted, so it is made again.
        cache.begin_frame();
        let value = *cache.get_or_insert_with(1, 0, &row("a"), || 99);
        assert_eq!(value, 99);
        // Equal content at two positions shares one entry.
        let shared = *cache.get_or_insert_with(2, 7, &row("b"), || 100);
        assert_eq!(shared, 2);
        cache.end_frame();
        assert_eq!(cache.entries.values().map(Vec::len).sum::<usize>(), 2);
        cache.clear();
        assert!(cache.entries.is_empty() && cache.positions.is_empty());
    }
}
