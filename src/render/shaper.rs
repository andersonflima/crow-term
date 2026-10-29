//! Fonts, cell metrics and grid-anchored OpenType shaping shared by every
//! backend. Shaping decides *which* glyph goes *where* (integer pixel origin);
//! backends only decide how to put the glyph image there.
use super::{Rect, cosmic_color, parse_color, resolve_color};
use crate::config::Config;
use crate::model::{Cell, UnderlineStyle};
use cosmic_text::{
    Attrs, Buffer, CacheKey, Color, Family, FeatureTag, FontFeatures, FontSystem, Metrics, Shaping,
    Style, SwashCache, Weight, Wrap,
};
use std::collections::HashMap;
use std::sync::Arc;

const REGULAR: &[u8] = include_bytes!("../../assets/fonts/CrowquillMono-Regular.ttf");
const BOLD: &[u8] = include_bytes!("../../assets/fonts/CrowquillMono-Bold.ttf");
const ITALIC: &[u8] = include_bytes!("../../assets/fonts/CrowquillMono-Italic.ttf");
const BOLD_ITALIC: &[u8] = include_bytes!("../../assets/fonts/CrowquillMono-BoldItalic.ttf");

/// Families that hold the Nerd Font icons (Private Use Area), in preference order.
pub(crate) const SYMBOL_FAMILIES: [&str; 2] = ["Symbols Nerd Font Mono", "Symbols Nerd Font"];
/// Private Use Area code points: icon fonts live here, and a system CJK font's
/// vendor glyphs at the same code points are never the intended icon.
pub(crate) fn private_use(text: &str) -> bool {
    text.chars().next().is_some_and(|c| {
        matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
    })
}

/// A glyph image to draw with its top-left placement origin at (`x`, `y`) plus
/// the image's own placement offsets, in the coordinate space of its op.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedGlyph {
    pub key: CacheKey,
    pub x: i32,
    pub y: i32,
    /// Base color: mask glyphs take its RGB; color glyphs ignore it.
    pub color: Color,
}

/// A terminal row resolved into backend-neutral pieces, in row-local pixels
/// (`width` x `height`). Drawing order: `background`, `cell_bg` per column,
/// `glyphs` (alpha-blended, clipped to the row), then `decorations` (opaque).
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedRow {
    pub width: usize,
    pub height: usize,
    pub cell_width: usize,
    pub background: u32,
    pub cell_bg: Vec<u32>,
    pub glyphs: Vec<PlacedGlyph>,
    pub decorations: Vec<(Rect, u32)>,
}

pub struct GridShaper {
    pub fonts: FontSystem,
    /// Installed Nerd symbols family for Private Use Area cells that the
    /// configured family cannot draw itself.
    pub symbols: Option<&'static str>,
    /// Whether the configured family maps a Private Use Area code point.
    primary_covers: HashMap<char, bool>,
    pub swash: SwashCache,
    pub font_size: f32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub baseline: f32,
}
impl GridShaper {
    pub fn new(config: &Config, scale: f64) -> Self {
        let mut fonts = FontSystem::new();
        for data in [REGULAR, BOLD, ITALIC, BOLD_ITALIC] {
            // Reuse the embedded read-only bytes instead of copying all four
            // font faces (~38 MiB) into a second heap allocation.
            fonts
                .db_mut()
                .load_font_source(cosmic_text::fontdb::Source::Binary(Arc::new(data)));
        }
        fonts.db_mut().set_monospace_family(&config.font_family);
        let symbols = SYMBOL_FAMILIES.into_iter().find(|family| {
            fonts
                .db()
                .faces()
                .any(|face| face.families.iter().any(|(name, _)| name == family))
        });
        let mut shaper = Self {
            fonts,
            symbols,
            primary_covers: HashMap::new(),
            swash: SwashCache::new(),
            font_size: 18.0,
            cell_width: 11,
            cell_height: 23,
            baseline: 18.0,
        };
        shaper.rescale(config, scale);
        shaper
    }
    /// Recompute metrics for a new scale or font. Row caches of every backend
    /// must be invalidated by the caller.
    pub fn rescale(&mut self, config: &Config, scale: f64) {
        self.font_size = (config.font_size * scale as f32).clamp(6.0, 160.0);
        self.cell_height = (self.font_size * config.line_height).ceil().max(1.0) as u32;
        let mut probe = Buffer::new(
            &mut self.fonts,
            Metrics::new(self.font_size, self.cell_height as f32),
        );
        probe.set_text(
            "0",
            &Attrs::new().family(Family::Name(&config.font_family)),
            Shaping::Advanced,
            None,
        );
        probe.shape_until_scroll(&mut self.fonts, false);
        let metrics = probe
            .layout_runs()
            .next()
            .map(|run| (run.line_w, run.line_y));
        self.cell_width = metrics
            .map(|(width, _)| width.ceil() as u32)
            .unwrap_or((self.font_size * 0.6).ceil() as u32)
            .max(1);
        // Fallback fonts must not move the baseline of the entire terminal row.
        self.baseline = metrics
            .map(|(_, baseline)| baseline)
            .unwrap_or(self.font_size);
        self.swash = SwashCache::new();
    }
    /// Bound glyph image growth during long sessions.
    pub fn trim(&mut self) {
        if self.swash.image_cache.len() > 16384 {
            self.swash.image_cache.clear();
        }
    }
    /// An icon cell drawn from the symbols family: a Private Use Area code point
    /// the configured family does not map (a patched Nerd font keeps its own icons).
    pub fn symbol_cell(&mut self, config: &Config, cell: &Cell) -> bool {
        let Some(c) = cell.text.chars().next() else {
            return false;
        };
        if self.symbols.is_none() || !private_use(&cell.text) {
            return false;
        }
        if self.primary_covers.len() > 4096 {
            self.primary_covers.clear();
        }
        let covered = match self.primary_covers.get(&c) {
            Some(&covered) => covered,
            None => {
                let families = [cosmic_text::fontdb::Family::Name(&config.font_family)];
                let id = self.fonts.db().query(&cosmic_text::fontdb::Query {
                    families: &families,
                    ..Default::default()
                });
                let covered = id
                    .and_then(|id| self.fonts.get_font(id, Weight::NORMAL))
                    .is_some_and(|font| font.as_swash().charmap().map(c) != 0);
                self.primary_covers.insert(c, covered);
                covered
            }
        };
        !covered
    }
    fn attrs<'a>(&self, config: &'a Config, cell: &Cell, symbol: bool) -> Attrs<'a> {
        let mut features = FontFeatures::new();
        if !config.ligatures {
            features
                .disable(FeatureTag::STANDARD_LIGATURES)
                .disable(FeatureTag::CONTEXTUAL_LIGATURES)
                .disable(FeatureTag::CONTEXTUAL_ALTERNATES);
        }
        let family = match self.symbols {
            Some(symbols) if symbol => symbols,
            _ => config.font_family.as_str(),
        };
        Attrs::new()
            .family(Family::Name(family))
            // Symbols Nerd Font ships one weight; asking for bold would skip it.
            .weight(if cell.bold && !symbol {
                Weight::BOLD
            } else {
                Weight::NORMAL
            })
            .style(if cell.italic {
                Style::Italic
            } else {
                Style::Normal
            })
            .color(cosmic_color(resolve_color(config, cell.fg)))
            .font_features(features)
    }
    /// Shape one terminal row. Every shaped cluster is anchored to its cell:
    /// combining offsets and multi-cell ligatures survive, while fallback
    /// advances cannot shift the grid.
    pub fn shape_row(&mut self, config: &Config, cells: &[Cell]) -> ShapedRow {
        let cell_width = self.cell_width as usize;
        let width = cells.len() * cell_width;
        let height = self.cell_height as usize;
        let cell_bg = cells
            .iter()
            .map(|cell| resolve_color(config, cell.bg))
            .collect();
        let (text, byte_cols) = row_text(cells);
        let mut buffer = Buffer::new(
            &mut self.fonts,
            Metrics::new(self.font_size, self.cell_height as f32),
        );
        buffer.set_wrap(Wrap::None);
        buffer.set_size(
            Some(width as f32 + self.font_size * 4.0),
            Some(self.cell_height as f32),
        );
        buffer.set_monospace_width(Some(self.cell_width as f32));
        // Coalesce equal styles, rather than assigning per-cell metadata that breaks ligatures.
        let mut spans: Vec<(&str, Attrs<'_>)> = Vec::new();
        let mut begin = 0;
        let symbol: Vec<bool> = cells
            .iter()
            .map(|cell| self.symbol_cell(config, cell))
            .collect();
        let mut previous_col: Option<usize> = None;
        let mut current_attrs = None;
        for &(byte, col) in &byte_cols {
            let cell = &cells[col];
            if previous_col.is_some_and(|old| {
                !same_text_style(&cells[old], cell) || symbol[old] != symbol[col]
            }) {
                if let Some(attrs) = current_attrs.take() {
                    spans.push((&text[begin..byte], attrs));
                }
                begin = byte;
            }
            if current_attrs.is_none() {
                current_attrs = Some(self.attrs(config, cell, symbol[col]));
            }
            previous_col = Some(col);
        }
        if let Some(attrs) = current_attrs {
            spans.push((&text[begin..], attrs));
        }
        buffer.set_rich_text(
            spans,
            &Attrs::new().family(Family::Name(&config.font_family)),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut self.fonts, false);
        let default_fg = cosmic_color(parse_color(&config.foreground, 0xeaeaea));
        let mut glyphs = Vec::new();
        for run in buffer.layout_runs() {
            let mut cluster_origins: HashMap<(usize, usize), f32> = HashMap::new();
            for glyph in run.glyphs {
                cluster_origins
                    .entry((glyph.start, glyph.end))
                    .and_modify(|x| *x = x.min(glyph.x))
                    .or_insert(glyph.x);
            }
            for glyph in run.glyphs {
                let mut glyph = glyph.clone();
                let col = column_for_byte(&byte_cols, glyph.start);
                let original_x = cluster_origins[&(glyph.start, glyph.end)];
                glyph.x = col as f32 * self.cell_width as f32 + glyph.x - original_x;
                let physical = glyph.physical((0.0, self.baseline), 1.0);
                glyphs.push(PlacedGlyph {
                    key: physical.cache_key,
                    x: physical.x,
                    y: physical.y,
                    color: glyph.color_opt.unwrap_or(default_fg),
                });
            }
        }
        let mut decorations = Vec::new();
        for (col, cell) in cells.iter().enumerate() {
            let color = resolve_color(config, cell.fg);
            if cell.strike && width > 0 {
                decorations.push((
                    Rect {
                        x: (col * cell_width) as i32,
                        y: (height / 2) as i32,
                        w: cell_width as u32,
                        h: 1,
                    },
                    color,
                ));
            }
            let (style, color) = underline_paint(config, cell);
            decorations.extend(
                underline_rects(
                    width,
                    height,
                    col * cell_width..(col + 1) * cell_width,
                    style,
                )
                .into_iter()
                .map(|rect| (rect, color)),
            );
        }
        ShapedRow {
            width,
            height,
            cell_width,
            background: parse_color(&config.background, 0x111111),
            cell_bg,
            glyphs,
            decorations,
        }
    }
    /// Shape UI text (header, tabs, footer, panel) with its top-left at
    /// (`x`, `y`). Returns the glyphs and the laid-out width.
    // Text origin, styling and size are deliberately explicit at this boundary.
    #[allow(clippy::too_many_arguments)]
    pub fn shape_text(
        &mut self,
        config: &Config,
        x: i32,
        y: i32,
        text: &str,
        color: u32,
        size: f32,
    ) -> (Vec<PlacedGlyph>, f32) {
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(size, size * 1.3));
        buffer.set_wrap(Wrap::None);
        buffer.set_text(
            text,
            &Attrs::new()
                .family(Family::Name(&config.font_family))
                .color(cosmic_color(color)),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut self.fonts, false);
        let mut width = 0f32;
        let mut glyphs = Vec::new();
        for run in buffer.layout_runs() {
            width = width.max(run.line_w);
            for glyph in run.glyphs {
                let physical = glyph.physical((x as f32, y as f32 + run.line_y), 1.0);
                glyphs.push(PlacedGlyph {
                    key: physical.cache_key,
                    x: physical.x,
                    y: physical.y,
                    color: cosmic_color(color),
                });
            }
        }
        (glyphs, width)
    }
}

/// Underline style and color of a cell: SGR 58 color if set, else the
/// foreground; snapshots that only set the boolean draw a single line.
pub(crate) fn underline_paint(config: &Config, cell: &Cell) -> (UnderlineStyle, u32) {
    let style = match cell.underline_style {
        UnderlineStyle::None if cell.underline => UnderlineStyle::Single,
        style => style,
    };
    let color = resolve_color(config, cell.underline_color.unwrap_or(cell.fg));
    (style, color)
}

/// Pixel geometry of one cell's underline inside a `width` x `height` row, as
/// rectangles (merged runs). Positions use row-absolute x, which keeps curly
/// and dashed patterns continuous across cells. Rectangles may extend past the
/// row bottom; drawing clips them.
pub(crate) fn underline_rects(
    width: usize,
    height: usize,
    columns: std::ops::Range<usize>,
    style: UnderlineStyle,
) -> Vec<Rect> {
    let mut rects: Vec<Rect> = Vec::new();
    if style == UnderlineStyle::None || width == 0 || height < 2 {
        return rects;
    }
    let thickness = (height / 20).max(1);
    let bottom = height - 1;
    let single_top = height.saturating_sub(1 + thickness);
    let mut segment = |x: usize, top: usize| {
        // Merge with a run on the same line that ends right before x.
        if let Some(rect) = rects
            .iter_mut()
            .rev()
            .take(2)
            .find(|rect| rect.y == top as i32 && rect.x as i64 + rect.w as i64 == x as i64)
        {
            rect.w += 1;
        } else {
            rects.push(Rect {
                x: x as i32,
                y: top as i32,
                w: 1,
                h: thickness as u32,
            });
        }
    };
    for x in columns.start..columns.end.min(width) {
        match style {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => segment(x, single_top),
            UnderlineStyle::Double => {
                segment(x, bottom + 1 - thickness);
                segment(x, bottom.saturating_sub(3 * thickness) + 1);
            }
            UnderlineStyle::Dotted => {
                if (x / (thickness * 2).max(2)).is_multiple_of(2) {
                    segment(x, single_top);
                }
            }
            UnderlineStyle::Dashed => {
                let dash = (columns.len() / 2).max(3);
                if (x / dash).is_multiple_of(2) {
                    segment(x, single_top);
                }
            }
            UnderlineStyle::Curly => {
                let amplitude = (height / 8).clamp(1, 12) as f32;
                let period = (columns.len().max(4)) as f32;
                let phase = (x as f32 / period) * std::f32::consts::TAU;
                let offset = (amplitude * (1.0 - phase.cos()) / 2.0).round() as usize;
                segment(x, (bottom + 1).saturating_sub(thickness + offset));
            }
        }
    }
    rects
}

fn same_text_style(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg && a.bold == b.bold && a.italic == b.italic
}
pub(crate) fn row_text(cells: &[Cell]) -> (String, Vec<(usize, usize)>) {
    let mut text = String::new();
    let mut byte_cols = Vec::new();
    for (col, cell) in cells.iter().enumerate() {
        if cell.wide_spacer {
            continue;
        }
        byte_cols.push((text.len(), col));
        if cell.text.is_empty() {
            text.push(' ');
        } else {
            text.push_str(&cell.text);
        }
    }
    (text, byte_cols)
}
pub(crate) fn column_for_byte(map: &[(usize, usize)], byte: usize) -> usize {
    map.iter()
        .rev()
        .find(|(start, _)| *start <= byte)
        .map_or(0, |(_, col)| *col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColorSpec, UnderlineStyle};
    fn cell(text: &str) -> Cell {
        Cell {
            text: text.into(),
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
        }
    }
    #[test]
    fn symbols_family_only_for_icons_the_configured_font_lacks() {
        let config = Config::default();
        let mut renderer = GridShaper::new(&config, 1.0);
        renderer.symbols = None;
        assert!(
            !renderer.symbol_cell(&config, &cell("\u{E702}")),
            "no symbols font installed"
        );
        renderer.symbols = Some(SYMBOL_FAMILIES[0]);
        assert!(!renderer.symbol_cell(&config, &cell("a")));
        assert!(!renderer.symbol_cell(&config, &cell("")));
        // The configured face's own charmap decides: the bundled Crowquill Mono
        // maps the powerline separators (U+E0A0-E0B3) but not nf-dev-git (U+E702).
        assert!(renderer.symbol_cell(&config, &cell("\u{E702}")));
        assert!(!renderer.symbol_cell(&config, &cell("\u{E0B0}")));
        assert_eq!(renderer.primary_covers.get(&'\u{E0B0}'), Some(&true));
        // A configured family that does not exist covers nothing: always the symbols family.
        let missing = Config {
            font_family: "No Such Family For Crow Tests".into(),
            ..Config::default()
        };
        renderer.primary_covers.clear();
        assert!(renderer.symbol_cell(&missing, &cell("\u{F113}")));
        let attrs = renderer.attrs(
            &missing,
            &Cell {
                bold: true,
                ..cell("\u{F113}")
            },
            true,
        );
        assert_eq!(attrs.family, Family::Name(SYMBOL_FAMILIES[0]));
        assert_eq!(
            attrs.weight,
            Weight::NORMAL,
            "single-weight icon font must not be skipped for bold"
        );
    }
}
