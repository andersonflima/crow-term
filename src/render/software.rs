//! Reference CPU backend: draws a [`Scene`] into a `0x00RRGGBB` framebuffer
//! (softbuffer's layout) with 8-bit, gamma-space alpha blending.
use super::{GridShaper, Layer, Op, Rect, Scene, ShapedRow};
use crate::config::Config;
use crate::model::Cell;
use cosmic_text::Color;
use std::collections::HashMap;

struct CachedRow {
    cells: Vec<Cell>,
    pixels: Vec<u32>,
    width: usize,
}

/// Row rasters keyed by (pane, row) and the last rasterized cached layer.
#[derive(Default)]
pub struct SoftwareRenderer {
    rows: HashMap<(u64, usize), CachedRow>,
    layer: Option<(u64, Vec<u32>)>,
}
impl SoftwareRenderer {
    /// Forget row rasters (metrics, scale or layout changed). Layers are
    /// re-rasterized when their key changes.
    pub fn invalidate(&mut self) {
        self.rows.clear();
    }
    /// Draw `scene` into `frame` (`scene.width` pixels per row).
    pub fn render(
        &mut self,
        scene: &Scene<'_>,
        shaper: &mut GridShaper,
        config: &Config,
        frame: &mut [u32],
    ) {
        let fw = scene.width as usize;
        frame.fill(scene.background);
        self.rows.retain(|(id, _), _| scene.live_panes.contains(id));
        for op in &scene.ops {
            match op {
                Op::Fill { rect, color } => fill(frame, fw, *rect, *color),
                Op::Glyphs { clip, glyphs } => draw_glyphs(frame, fw, *clip, glyphs, shaper),
                Op::Row {
                    pane,
                    row,
                    cells,
                    x,
                    y,
                    clip,
                } => {
                    let cached = self.row(shaper, config, *pane, *row, cells);
                    blit(frame, fw, &cached.pixels, cached.width, *x, *y, *clip);
                }
                Op::Invert { rect } => invert(frame, fw, *rect),
                Op::Layer { rect, clip, layer } => {
                    if self.layer.as_ref().is_none_or(|(key, _)| *key != layer.key) {
                        self.layer = Some((layer.key, rasterize_layer(layer, shaper)));
                    }
                    if let Some((_, pixels)) = &self.layer {
                        blit(
                            frame,
                            fw,
                            pixels,
                            layer.width as usize,
                            rect.x,
                            rect.y,
                            *clip,
                        );
                    }
                }
            }
        }
        shaper.trim();
    }
    fn row(
        &mut self,
        shaper: &mut GridShaper,
        config: &Config,
        pane: u64,
        row: usize,
        cells: &[Cell],
    ) -> &CachedRow {
        let key = (pane, row);
        let needs_update = self
            .rows
            .get(&key)
            .is_none_or(|cached| cached.cells != cells);
        if needs_update {
            let shaped = shaper.shape_row(config, cells);
            let pixels = rasterize_row(&shaped, shaper);
            self.rows.insert(
                key,
                CachedRow {
                    cells: cells.to_vec(),
                    pixels,
                    width: shaped.width,
                },
            );
        }
        &self.rows[&key]
    }
}

/// Rasterize a shaped row into its own `width` x `height` buffer.
pub fn rasterize_row(shaped: &ShapedRow, shaper: &mut GridShaper) -> Vec<u32> {
    let (width, height, cw) = (shaped.width, shaped.height, shaped.cell_width);
    let mut pixels = vec![shaped.background; width * height];
    for (col, &color) in shaped.cell_bg.iter().enumerate() {
        for y in 0..height {
            let start = y * width + col * cw;
            pixels[start..start + cw].fill(color);
        }
    }
    for glyph in &shaped.glyphs {
        shaper
            .swash
            .with_pixels(&mut shaper.fonts, glyph.key, glyph.color, |x, y, color| {
                let x = x + glyph.x;
                let y = y + glyph.y;
                if x >= 0 && y >= 0 && (x as usize) < width && (y as usize) < height {
                    blend(&mut pixels[y as usize * width + x as usize], color);
                }
            });
    }
    for &(rect, color) in &shaped.decorations {
        fill(&mut pixels, width, rect, color);
    }
    pixels
}

fn rasterize_layer(layer: &Layer, shaper: &mut GridShaper) -> Vec<u32> {
    let w = layer.width as usize;
    let mut pixels = vec![layer.background; w * layer.height as usize];
    for op in &layer.ops {
        match op {
            Op::Fill { rect, color } => fill(&mut pixels, w, *rect, *color),
            Op::Glyphs { clip, glyphs } => draw_glyphs(&mut pixels, w, *clip, glyphs, shaper),
            Op::Invert { rect } => invert(&mut pixels, w, *rect),
            // Layers hold chrome only; rows and nested layers are not cached here.
            Op::Row { .. } | Op::Layer { .. } => {}
        }
    }
    pixels
}

fn draw_glyphs(
    frame: &mut [u32],
    frame_width: usize,
    clip: Rect,
    glyphs: &[super::PlacedGlyph],
    shaper: &mut GridShaper,
) {
    for glyph in glyphs {
        shaper.swash.with_pixels(
            &mut shaper.fonts,
            glyph.key,
            glyph.color,
            |gx, gy, color| {
                let px = gx + glyph.x;
                let py = gy + glyph.y;
                if clip.contains(px as f64, py as f64)
                    && let Some(pixel) = frame.get_mut(py as usize * frame_width + px as usize)
                {
                    blend(pixel, color);
                }
            },
        );
    }
}

pub(crate) fn blend(target: &mut u32, color: Color) {
    let alpha = color.a() as u32;
    if alpha == 0 {
        return;
    }
    let mix = |old: u32, new: u32| (new * alpha + old * (255 - alpha) + 127) / 255;
    *target = mix((*target >> 16) & 255, color.r() as u32) << 16
        | mix((*target >> 8) & 255, color.g() as u32) << 8
        | mix(*target & 255, color.b() as u32);
}
pub(crate) fn fill(frame: &mut [u32], width: usize, rect: Rect, color: u32) {
    if width == 0 {
        return;
    }
    let bounds = Rect {
        x: 0,
        y: 0,
        w: width as u32,
        h: (frame.len() / width) as u32,
    };
    let rect = rect.intersection(bounds);
    for y in rect.y..rect.y + rect.h as i32 {
        let start = y as usize * width + rect.x as usize;
        frame[start..start + rect.w as usize].fill(color);
    }
}
fn invert(frame: &mut [u32], width: usize, rect: Rect) {
    for y in rect.y..rect.y + rect.h as i32 {
        for x in rect.x..rect.x + rect.w as i32 {
            let pixel = &mut frame[y as usize * width + x as usize];
            *pixel ^= 0xffffff;
        }
    }
}
pub(crate) fn blit(
    frame: &mut [u32],
    width: usize,
    source: &[u32],
    source_width: usize,
    x: i32,
    y: i32,
    clip: Rect,
) {
    if width == 0 || source_width == 0 {
        return;
    }
    let target = Rect {
        x,
        y,
        w: source_width as u32,
        h: (source.len() / source_width) as u32,
    }
    .intersection(clip)
    .intersection(Rect {
        x: 0,
        y: 0,
        w: width as u32,
        h: (frame.len() / width) as u32,
    });
    for row in target.y..target.y + target.h as i32 {
        let src = (row - y) as usize * source_width + (target.x - x) as usize;
        let dst = row as usize * width + target.x as usize;
        frame[dst..dst + target.w as usize].copy_from_slice(&source[src..src + target.w as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColorSpec, UnderlineStyle};
    use crate::render::parse_color;
    use crate::render::shaper::{private_use, underline_paint, underline_rects};
    use cosmic_text::{Attrs, Buffer, Family, FeatureTag, FontFeatures, Metrics, Shaping};
    pub(crate) fn cell(text: &str) -> Cell {
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
    fn raster(shaper: &mut GridShaper, config: &Config, cells: &[Cell]) -> (Vec<u32>, usize) {
        let shaped = shaper.shape_row(config, cells);
        (rasterize_row(&shaped, shaper), shaped.width)
    }
    fn draw_underline(
        pixels: &mut [u32],
        width: usize,
        height: usize,
        columns: std::ops::Range<usize>,
        style: UnderlineStyle,
        color: u32,
    ) {
        for rect in underline_rects(width, height, columns, style) {
            fill(pixels, width, rect, color);
        }
    }
    #[test]
    fn underline_styles_rasterize_distinct_shapes_inside_the_cell() {
        let (width, height) = (24usize, 20usize);
        let render = |style| {
            let mut pixels = vec![0u32; width * height];
            draw_underline(&mut pixels, width, height, 8..16, style, 0xff0000);
            pixels
        };
        let rows = |pixels: &[u32]| {
            (0..height)
                .filter(|y| pixels[y * width..(y + 1) * width].contains(&0xff0000))
                .collect::<Vec<_>>()
        };
        let lit = |pixels: &[u32]| pixels.iter().filter(|&&p| p == 0xff0000).count();
        assert_eq!(lit(&render(UnderlineStyle::None)), 0);
        let single = render(UnderlineStyle::Single);
        assert_eq!(rows(&single), [height - 2]);
        assert_eq!(lit(&single), 8);
        // Nothing is drawn outside the cell's columns.
        for pixels in [
            UnderlineStyle::Double,
            UnderlineStyle::Curly,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ]
        .map(render)
        {
            for y in 0..height {
                for x in (0..8).chain(16..width) {
                    assert_eq!(pixels[y * width + x], 0);
                }
            }
        }
        assert_eq!(rows(&render(UnderlineStyle::Double)).len(), 2);
        assert!(
            rows(&render(UnderlineStyle::Curly)).len() >= 2,
            "undercurl is not flat"
        );
        assert!(lit(&render(UnderlineStyle::Dotted)) < 8);
        assert!(lit(&render(UnderlineStyle::Dashed)) < 8);
        // Tiny rows and out-of-range columns never index outside the buffer.
        let mut tiny = vec![0u32; 4];
        draw_underline(&mut tiny, 4, 1, 0..100, UnderlineStyle::Curly, 1);
        draw_underline(&mut tiny, 2, 2, 0..100, UnderlineStyle::Double, 1);
    }

    #[test]
    fn row_raster_uses_the_cell_underline_style_and_color() {
        let config = Config::default();
        let mut curly = cell("x");
        curly.underline = true;
        curly.underline_style = UnderlineStyle::Curly;
        curly.underline_color = Some(ColorSpec::Rgb(255, 0, 0));
        assert_eq!(
            underline_paint(&config, &curly),
            (UnderlineStyle::Curly, 0xff0000)
        );
        let mut legacy = cell("x");
        legacy.underline = true;
        legacy.fg = ColorSpec::Rgb(0, 0, 255);
        assert_eq!(
            underline_paint(&config, &legacy),
            (UnderlineStyle::Single, 0x0000ff)
        );
        // Through the real row rasterizer: red pixels only below the glyph area,
        // on more than one scanline (a curl, not a straight line).
        let mut renderer = GridShaper::new(&config, 1.0);
        let (pixels, width) = raster(&mut renderer, &config, &[curly]);
        let height = pixels.len() / width;
        let red: Vec<usize> = (0..height)
            .filter(|y| pixels[y * width..(y + 1) * width].contains(&0xff0000))
            .collect();
        assert!(red.len() >= 2, "{red:?}");
        assert!(red.iter().all(|&y| y >= height * 2 / 3), "{red:?}");
    }

    #[test]
    fn shaped_clusters_do_not_shift_following_terminal_cells() {
        let config = Config::default();
        let mut renderer = GridShaper::new(&config, 1.0);
        let cw = renderer.cell_width as usize;
        let mut spacer = cell("");
        spacer.wide_spacer = true;
        for mut prefix in [
            vec![cell("界"), spacer],
            vec![cell("e\u{301}"), cell(" ")],
            vec![cell("!"), cell("=")],
        ] {
            prefix.push(cell("X"));
            let actual = raster(&mut renderer, &config, &prefix).0;
            let expected = raster(&mut renderer, &config, &[cell(" "), cell(" "), cell("X")]).0;
            for row in 0..renderer.cell_height as usize {
                assert_eq!(
                    &actual[row * 3 * cw + 2 * cw..(row + 1) * 3 * cw],
                    &expected[row * 3 * cw + 2 * cw..(row + 1) * 3 * cw]
                );
            }
        }
    }

    #[test]
    fn private_use_cells_are_icon_cells() {
        for icon in ["\u{E702}", "\u{F113}", "\u{F0001}", "\u{10FFFD}"] {
            assert!(private_use(icon), "{icon:?}");
        }
        for text in ["a", "界", "🚀", "", "\u{F900}"] {
            assert!(!private_use(text), "{text:?}");
        }
    }

    #[test]
    fn bundled_font_shapes_ligatures_and_raster_cache_reuses_rows() {
        let config = Config {
            ligatures: true,
            ..Config::default()
        };
        let mut renderer = GridShaper::new(&config, 1.0);
        let cells = "!=->==="
            .chars()
            .map(|c| cell(&c.to_string()))
            .collect::<Vec<_>>();
        let mut software = SoftwareRenderer::default();
        let first = software
            .row(&mut renderer, &config, 1, 0, &cells)
            .pixels
            .as_ptr();
        assert_eq!(
            first,
            software
                .row(&mut renderer, &config, 1, 0, &cells)
                .pixels
                .as_ptr()
        );
        assert!(
            software
                .row(&mut renderer, &config, 1, 0, &cells)
                .pixels
                .iter()
                .any(|pixel| *pixel != parse_color(&config.background, 0xffffff))
        );
        let attrs = Attrs::new().family(Family::Name(&config.font_family));
        let mut buffer = Buffer::new(&mut renderer.fonts, Metrics::new(18.0, 24.0));
        buffer.set_text("!=", &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut renderer.fonts, false);
        // Crowquill's contextual alternates may use one glyph per cell; verify actual
        // glyph substitutions rather than assuming ligatures reduce the glyph count.
        let enabled = buffer
            .layout_runs()
            .flat_map(|run| run.glyphs.iter().map(|g| g.glyph_id))
            .collect::<Vec<_>>();
        let mut features = FontFeatures::new();
        features
            .disable(FeatureTag::CONTEXTUAL_ALTERNATES)
            .disable(FeatureTag::STANDARD_LIGATURES);
        buffer.set_text(
            "!=",
            &attrs.font_features(features),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut renderer.fonts, false);
        let disabled = buffer
            .layout_runs()
            .flat_map(|run| run.glyphs.iter().map(|g| g.glyph_id))
            .collect::<Vec<_>>();
        assert_ne!(
            enabled, disabled,
            "bundled font must provide the requested ligature feature"
        );
    }
}
