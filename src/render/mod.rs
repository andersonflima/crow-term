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

/// WCAG 2.x relative luminance of a `0xRRGGBB` color.
pub fn relative_luminance(color: u32) -> f64 {
    let channel = |shift: u32| {
        let c = ((color >> shift) & 255) as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}
/// WCAG 2.x contrast ratio of two colors, from 1.0 (equal) to 21.0.
pub fn contrast_ratio(a: u32, b: u32) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}
fn mix_towards(color: u32, target: u32, step: u32) -> u32 {
    let channel = |shift: u32| {
        let c = (color >> shift) & 255;
        let t = (target >> shift) & 255;
        (c * (255 - step) + t * step + 127) / 255
    };
    channel(16) << 16 | channel(8) << 8 | channel(0)
}
/// `fg` itself when it already has `minimum` contrast against `bg`; otherwise
/// the least change that reaches it: `fg` mixed towards black or white
/// (whichever contrasts more with `bg`), which keeps its hue. When even pure
/// black/white cannot reach `minimum`, that extreme is returned.
pub fn ensure_contrast(fg: u32, bg: u32, minimum: f32) -> u32 {
    let minimum = f64::from(minimum);
    if minimum <= 1.0 || contrast_ratio(fg, bg) >= minimum {
        return fg;
    }
    let target = if contrast_ratio(0xffffff, bg) >= contrast_ratio(0x000000, bg) {
        0xffffff
    } else {
        0x000000
    };
    if contrast_ratio(target, bg) < minimum {
        return target;
    }
    // Moving away from the background's luminance only raises the ratio, so
    // the smallest sufficient step is found by bisection.
    let (mut low, mut high) = (0u32, 255u32);
    while low < high {
        let step = (low + high) / 2;
        if contrast_ratio(mix_towards(fg, target, step), bg) >= minimum {
            high = step;
        } else {
            low = step + 1;
        }
    }
    mix_towards(fg, target, low)
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
    #[test]
    fn wcag_contrast_matches_known_pairs() {
        let close = |a: f64, b: f64| (a - b).abs() < 0.01;
        assert!(close(contrast_ratio(0x000000, 0xffffff), 21.0));
        assert!(close(contrast_ratio(0xffffff, 0xffffff), 1.0));
        // Published WCAG reference values.
        assert!(close(contrast_ratio(0x777777, 0xffffff), 4.48));
        assert!(close(contrast_ratio(0x767676, 0xffffff), 4.54));
        assert!(close(contrast_ratio(0x444444, 0xffffff), 9.74));
        assert!(close(contrast_ratio(0x0000ff, 0xffffff), 8.59));
        assert!(close(contrast_ratio(0xff0000, 0xffffff), 4.0));
        // Symmetric.
        assert_eq!(
            contrast_ratio(0x123456, 0xabcdef),
            contrast_ratio(0xabcdef, 0x123456)
        );
    }
    #[test]
    fn ensure_contrast_reaches_the_minimum_with_the_least_change() {
        // Catppuccin Macchiato text on a white background (the reported bug).
        for fg in [0xcad3f5, 0x8aadf4, 0xf0c6c6, 0x6e738d] {
            let fixed = ensure_contrast(fg, 0xffffff, 3.0);
            assert!(
                contrast_ratio(fixed, 0xffffff) >= 3.0,
                "{fg:06x} -> {fixed:06x}"
            );
            // Darkened (towards black on a light background), not replaced.
            assert!(relative_luminance(fixed) <= relative_luminance(fg));
            // Least change: one step less would not be enough.
            if fixed != fg {
                let target = 0x000000;
                let mut found = None;
                for step in 0..=255 {
                    if contrast_ratio(mix_towards(fg, target, step), 0xffffff) >= 3.0 {
                        found = Some(mix_towards(fg, target, step));
                        break;
                    }
                }
                assert_eq!(Some(fixed), found);
            }
        }
        // Hue kept: a pale blue stays blue-dominant.
        let blue = ensure_contrast(0xcad3f5, 0xffffff, 3.0);
        assert!(blue & 0xff > (blue >> 16) & 0xff);
        // Already sufficient: untouched, byte for byte.
        assert_eq!(ensure_contrast(0x444444, 0xffffff, 3.0), 0x444444);
        assert_eq!(ensure_contrast(0xeaeaea, 0x111111, 3.0), 0xeaeaea);
        // Dark backgrounds lighten.
        let light = ensure_contrast(0x303446, 0x24273a, 3.0);
        assert!(contrast_ratio(light, 0x24273a) >= 3.0);
        assert!(relative_luminance(light) > relative_luminance(0x303446));
        // Off (1.0) never changes anything; unreachable minimums give the extreme.
        assert_eq!(ensure_contrast(0xfefefe, 0xffffff, 1.0), 0xfefefe);
        assert_eq!(ensure_contrast(0x777777, 0x777777, 21.0), 0x000000);
    }
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
