//! Test-only capture of the composed GUI frame, so graphical tests compare what
//! the renderer produced instead of OS screenshot quirks (compositor, scaling,
//! window decorations). The encoder is always compiled and unit-tested; the GUI
//! only calls it when built with the `frame-dump` cargo feature and started with
//! `CROW_TERM_FRAME_DUMP=<existing directory>`. Release builds never contain the hook.
//!
//! Pixels are `0x00RRGGBB` rows (softbuffer's layout). A GPU renderer reuses this
//! by reading its framebuffer back into the same layout and calling [`FrameDump::write`].
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Environment variable naming the directory that receives `frame.ppm`.
pub const ENV: &str = "CROW_TERM_FRAME_DUMP";
/// File replaced atomically after every presented frame.
pub const FILE: &str = "frame.ppm";
/// Prefix of the PPM comment line carrying the JSON metadata.
pub const META_PREFIX: &str = "# crow-frame ";

/// A pane as drawn: physical-pixel rectangle and the grid inside it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PaneMeta {
    pub id: u64,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CursorMeta {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    pub shape: String,
}

/// What the frame shows, so tests can wait for a state and locate cells without
/// guessing font metrics. Field names are a test contract, not a public API.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FrameMeta {
    pub schema: &'static str,
    pub seq: u64,
    pub renderer: &'static str,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub cell_width: u32,
    pub cell_height: u32,
    /// Mux snapshot revision drawn (0 before the first snapshot).
    pub revision: u64,
    pub session: String,
    pub window: u64,
    pub pane: u64,
    pub panes: Vec<PaneMeta>,
    pub cursor: Option<CursorMeta>,
    pub focused: bool,
    pub prefix: bool,
    pub status: String,
    pub ime: String,
    /// Pane whose program is not reading its input, if any (footer note).
    pub stuck_pane: Option<u64>,
    /// The link under the pointer while the link modifier is held.
    #[serde(default)]
    pub hover_link: Option<String>,
    /// Footer context segments as drawn (global segment last).
    pub footer: Vec<String>,
    /// Tab labels as drawn.
    pub tabs: Vec<String>,
    /// No tab label is waiting to change (its debounce has run out): what
    /// the tabs show is what they will keep showing.
    pub labels_settled: bool,
}

pub struct FrameDump {
    dir: PathBuf,
    seq: u64,
}

impl FrameDump {
    /// `None` unless the variable names an existing directory (never created here).
    pub fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os(ENV)?);
        if dir.is_dir() {
            Some(Self { dir, seq: 0 })
        } else {
            eprintln!(
                "crow-term: {ENV} ignored: {} is not a directory",
                dir.display()
            );
            None
        }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    /// Next sequence number; the caller stores it in [`FrameMeta::seq`].
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
    /// Replace `frame.ppm` atomically (write a sibling, then rename), so a reader
    /// never sees a torn frame. A reader holding the file open on Windows can make
    /// the rename fail briefly; retry a few times, then report the error.
    pub fn write(
        &self,
        pixels: &[u32],
        width: u32,
        height: u32,
        meta: &FrameMeta,
    ) -> io::Result<()> {
        let bytes = encode_ppm(pixels, width, height, meta)?;
        let temp = self.dir.join(format!("{FILE}.{}.tmp", std::process::id()));
        {
            // Never follow a planted link or reuse a stale file: remove the name
            // (a link itself, not its target), then create it exclusively, owner-only.
            let _ = std::fs::remove_file(&temp);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&bytes)?;
        }
        let target = self.dir.join(FILE);
        let mut last = None;
        for _ in 0..20 {
            match std::fs::rename(&temp, &target) {
                Ok(()) => return Ok(()),
                Err(error) => last = Some(error),
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(&temp);
        Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
    }
}

/// Binary PPM (P6, 8-bit RGB) with the metadata as a single JSON comment line.
pub fn encode_ppm(
    pixels: &[u32],
    width: u32,
    height: u32,
    meta: &FrameMeta,
) -> io::Result<Vec<u8>> {
    let count = width as usize * height as usize;
    if pixels.len() < count {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "frame has {} pixels, {width}x{height} needs {count}",
                pixels.len()
            ),
        ));
    }
    let json = serde_json::to_string(meta).map_err(io::Error::other)?;
    let mut out = Vec::with_capacity(64 + json.len() + count * 3);
    write!(out, "P6\n{META_PREFIX}{json}\n{width} {height}\n255\n")?;
    for &pixel in &pixels[..count] {
        out.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(seq: u64) -> FrameMeta {
        FrameMeta {
            schema: "crow-frame/1",
            seq,
            renderer: "software",
            width: 2,
            height: 1,
            scale: 1.0,
            cell_width: 1,
            cell_height: 1,
            revision: 7,
            session: "s\n\"x".into(),
            window: 1,
            pane: 2,
            panes: vec![PaneMeta {
                id: 2,
                x: 0,
                y: 0,
                w: 2,
                h: 1,
                rows: 1,
                cols: 2,
            }],
            cursor: None,
            focused: true,
            prefix: false,
            status: String::new(),
            ime: String::new(),
            stuck_pane: None,
            hover_link: None,
            footer: Vec::new(),
            tabs: Vec::new(),
            labels_settled: true,
        }
    }

    #[test]
    fn ppm_is_rgb_with_single_line_json_metadata() {
        let bytes = encode_ppm(&[0x00ff_8001, 0xff12_3456], 2, 1, &meta(3)).unwrap();
        let header_end = bytes.len() - 6;
        let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let lines: Vec<&str> = header.lines().collect();
        assert_eq!(lines[0], "P6");
        let json = lines[1].strip_prefix(META_PREFIX).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["seq"], 3);
        assert_eq!(
            value["session"], "s\n\"x",
            "newlines stay escaped on one line"
        );
        assert_eq!(&lines[2..], ["2 1", "255"]);
        // The unused high byte of 0RGB pixels is ignored.
        assert_eq!(&bytes[header_end..], [0xff, 0x80, 0x01, 0x12, 0x34, 0x56]);
    }

    #[test]
    fn short_frames_are_rejected_and_writes_replace_atomically() {
        assert!(encode_ppm(&[0], 2, 1, &meta(1)).is_err());
        let dir = tempfile::tempdir().unwrap();
        let mut dump = FrameDump {
            dir: dir.path().into(),
            seq: 0,
        };
        for _ in 0..2 {
            let seq = dump.next_seq();
            dump.write(&[0, 0xffffff], 2, 1, &meta(seq)).unwrap();
        }
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [FILE], "no temporary files left behind");
        let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("\"seq\":2"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let mode = std::fs::metadata(dir.path().join(FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "frame readable by others: {mode:o}");
            // A planted link at the temporary name is replaced, never written through.
            let victim = dir.path().join("victim");
            std::fs::write(&victim, b"keep").unwrap();
            symlink(
                &victim,
                dir.path()
                    .join(format!("{FILE}.{}.tmp", std::process::id())),
            )
            .unwrap();
            dump.write(&[0, 0], 2, 1, &meta(9)).unwrap();
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        }
    }
}
