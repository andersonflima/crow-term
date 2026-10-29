//! Test-only golden image helpers: PNG I/O for `0x00RRGGBB` frames and the
//! tolerant comparison shared by the software goldens and GPU-vs-software tests.
use std::path::{Path, PathBuf};

/// Tolerance used by every golden comparison: at most `max_channel` difference
/// per channel, on at most `max_fraction` of the pixels.
#[derive(Debug, Clone, Copy)]
pub struct Tolerance {
    pub max_channel: u8,
    pub max_fraction: f64,
}
/// Documented renderer tolerance: <= 2 per channel on <= 0.1% of the pixels.
pub const RENDERER: Tolerance = Tolerance {
    max_channel: 2,
    max_fraction: 0.001,
};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Diff {
    /// Pixels with any channel difference.
    pub differing: usize,
    /// Pixels whose difference exceeds the per-channel tolerance.
    pub over: usize,
    pub max_channel: u8,
    pub total: usize,
}
impl Diff {
    pub fn within(&self, tolerance: Tolerance) -> bool {
        self.over == 0 && self.differing as f64 <= tolerance.max_fraction * self.total as f64
    }
}

pub fn diff(actual: &[u32], expected: &[u32], tolerance: Tolerance) -> Diff {
    assert_eq!(actual.len(), expected.len(), "frame sizes differ");
    let mut out = Diff {
        total: actual.len(),
        ..Diff::default()
    };
    for (&a, &e) in actual.iter().zip(expected) {
        if a & 0xffffff == e & 0xffffff {
            continue;
        }
        out.differing += 1;
        let delta = [16, 8, 0]
            .into_iter()
            .map(|shift| ((a >> shift) & 255).abs_diff((e >> shift) & 255) as u8)
            .max()
            .unwrap_or(0);
        out.max_channel = out.max_channel.max(delta);
        if delta > tolerance.max_channel {
            out.over += 1;
        }
    }
    out
}

pub fn write_png(path: &Path, pixels: &[u32], width: u32, height: u32) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().unwrap();
    let mut data = Vec::with_capacity(pixels.len() * 3);
    for &p in pixels {
        data.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
    }
    writer.write_image_data(&data).unwrap();
}

pub fn read_png(path: &Path) -> Option<(Vec<u32>, u32, u32)> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    let mut reader = decoder.read_info().ok()?;
    let mut data = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut data).ok()?;
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => return None,
    };
    let pixels = data[..info.buffer_size()]
        .chunks_exact(channels)
        .map(|c| (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32)
        .collect();
    Some((pixels, info.width, info.height))
}

/// Committed goldens live in `tests/goldens/render/`.
pub fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/goldens/render")
        .join(format!("{name}.png"))
}
/// Candidates for review are written outside the repository.
pub fn candidate_path(name: &str) -> PathBuf {
    std::env::temp_dir()
        .join("crow-term-golden-candidates")
        .join(format!("{name}.png"))
}

/// Compare a frame with its committed golden. A missing or different golden
/// writes a candidate (outside the repository) and fails with its path.
pub fn assert_golden(name: &str, pixels: &[u32], width: u32, height: u32) {
    let path = golden_path(name);
    let Some((expected, ew, eh)) = read_png(&path) else {
        let candidate = candidate_path(name);
        write_png(&candidate, pixels, width, height);
        panic!(
            "golden {} missing; candidate written to {} (review, then copy it into tests/goldens/render/)",
            path.display(),
            candidate.display()
        );
    };
    let ok = (ew, eh) == (width, height) && {
        let d = diff(pixels, &expected, RENDERER);
        if !d.within(RENDERER) {
            eprintln!("golden {name}: {d:?}");
        }
        d.within(RENDERER)
    };
    if !ok {
        let candidate = candidate_path(name);
        write_png(&candidate, pixels, width, height);
        panic!(
            "golden {name} differs ({width}x{height} vs {ew}x{eh}); candidate written to {}",
            candidate.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn png_round_trip_and_tolerance() {
        let pixels = vec![0x102030, 0xffffff, 0, 0x7f7f7f];
        let path = std::env::temp_dir().join(format!("crow-golden-{}.png", std::process::id()));
        write_png(&path, &pixels, 2, 2);
        let (back, w, h) = read_png(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!((back.as_slice(), w, h), (pixels.as_slice(), 2, 2));
        let mut near = pixels.clone();
        near[1] = 0xfefdff;
        let d = diff(&near, &pixels, RENDERER);
        assert_eq!((d.differing, d.over, d.max_channel), (1, 0, 2));
        // One differing pixel of four is 25% > 0.1%.
        assert!(!d.within(RENDERER));
        near[1] = 0xfafafa;
        assert_eq!(diff(&near, &pixels, RENDERER).over, 1);
        let big = vec![0u32; 2000];
        let mut one = big.clone();
        one[0] = 0x000001;
        assert!(diff(&one, &big, RENDERER).within(RENDERER));
    }
}
