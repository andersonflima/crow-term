//! What a paste sends: copied files and dropped files become shell-quoted
//! paths, an image alone on the clipboard becomes the path of a PNG written
//! for it, anything else is text. Priority when the clipboard has several
//! kinds: files, then image, then text (copying a file in a file manager also
//! puts its name as text and its icon as an image; the file is what was meant).
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Pasted images older than this are removed.
pub const IMAGE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Files(Vec<PathBuf>),
    Image {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
    Text(String),
}

/// Read the clipboard in priority order (files, image, text).
pub fn read(clipboard: &mut arboard::Clipboard) -> Option<Content> {
    #[cfg(feature = "test-hooks")]
    if let Some(files) = test_files() {
        return Some(Content::Files(files));
    }
    #[cfg(feature = "test-hooks")]
    if let Some(image) = test_image() {
        return Some(image);
    }
    if let Ok(files) = clipboard.get().file_list()
        && !files.is_empty()
    {
        return Some(Content::Files(files));
    }
    if let Ok(image) = clipboard.get_image()
        && image.width > 0
        && image.height > 0
    {
        return Some(Content::Image {
            width: image.width,
            height: image.height,
            rgba: image.bytes.into_owned(),
        });
    }
    clipboard.get_text().ok().map(Content::Text)
}

/// Test hook: `<runtime>/test-clipboard-image` ("W H" line, then W*H*4 RGBA
/// bytes) stands in for an image on the OS clipboard.
#[cfg(feature = "test-hooks")]
fn test_image() -> Option<Content> {
    let data = std::fs::read(crate::ipc::runtime_dir().ok()?.join("test-clipboard-image")).ok()?;
    let newline = data.iter().position(|&b| b == b'\n')?;
    let header = std::str::from_utf8(&data[..newline]).ok()?;
    let (width, height) = header.split_once(' ')?;
    Some(Content::Image {
        width: width.trim().parse().ok()?,
        height: height.trim().parse().ok()?,
        rgba: data[newline + 1..].to_vec(),
    })
}

/// Test hook: `<runtime>/test-clipboard-files` (one path per line) stands in
/// for a file list on the OS clipboard, which test drivers cannot set.
#[cfg(feature = "test-hooks")]
fn test_files() -> Option<Vec<PathBuf>> {
    let path = crate::ipc::runtime_dir().ok()?.join("test-clipboard-files");
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
    )
}

/// Why a path was not pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsafe(pub String);

impl std::fmt::Display for Unsafe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Characters no pasted path may contain. Single quotes make every other
/// character literal in sh, bash, zsh, dash, ksh, fish, tcsh/csh and
/// PowerShell alike, except these: `'` ends the quote, `\` is an escape in
/// fish, `!` is history expansion in tcsh/csh, `$` and backticks are
/// expanded by some shells in some modes, and control characters (newline,
/// ESC…) act on the terminal or the line editor. On Windows the cmd
/// metacharacters are refused as well, since cmd does not honour single
/// quotes; `\` is the Windows path separator and is literal inside
/// PowerShell single quotes (fish is not a Windows shell).
fn unsafe_char(c: char, windows: bool) -> bool {
    matches!(c, '\'' | '!' | '$' | '`')
        || (c == '\\' && !windows)
        || c.is_control()
        || matches!(c, '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}')
        || (windows && matches!(c, '&' | '|' | '^' | '%' | '(' | ')' | '<' | '>' | '"'))
}

/// A path as one word that every supported shell reads the same way:
/// `'<path>'`, refused (never altered) when it contains a character that
/// could act differently in some shell. The shell reading it is not guessed:
/// under tmux, ssh, sudo or a container the guess would be wrong.
pub fn quote(path: &Path) -> std::result::Result<String, Unsafe> {
    quote_for(path, cfg!(windows))
}

/// `quote` for a given platform (testable on every host).
pub fn quote_for(path: &Path, windows: bool) -> std::result::Result<String, Unsafe> {
    let text = path.to_string_lossy();
    if text.is_empty() {
        return Err(Unsafe("empty path".into()));
    }
    // A trailing backslash would sit right before the closing quote, where
    // a shell reading `\'` as an escape (fish, also under WSL/MSYS) would not
    // end the word there.
    if text.chars().any(|c| unsafe_char(c, windows)) || text.ends_with('\\') {
        let shown: String = text
            .chars()
            .map(|c| {
                if c.is_control() {
                    format!("\\u{{{:x}}}", u32::from(c))
                } else {
                    c.to_string()
                }
            })
            .collect();
        return Err(Unsafe(format!(
            "file name has characters that can't be pasted safely: {shown}"
        )));
    }
    Ok(format!("'{text}'"))
}

/// Paths as typed words, each followed by a space (like a file dropped on a
/// terminal: ready for the next argument). All or nothing.
pub fn paths_text(paths: &[PathBuf]) -> std::result::Result<String, Unsafe> {
    let mut out = String::new();
    for path in paths {
        out.push_str(&quote(path)?);
        out.push(' ');
    }
    Ok(out)
}

/// Where pasted images are written: a private directory (0700).
pub fn image_dir() -> Result<PathBuf> {
    // GUI journeys keep everything in their runtime directory.
    #[cfg(feature = "test-hooks")]
    if let Ok(dir) = crate::ipc::runtime_dir()
        && dir.join("test-clipboard-image").exists()
    {
        return Ok(dir.join("pasted-images"));
    }
    let dirs = directories::ProjectDirs::from("dev", "crow", "crow-term")
        .context("no cache directory for pasted images")?;
    Ok(dirs.cache_dir().join("pasted-images"))
}

pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
        // Not a symlink planted in its place; changing the mode fails
        // unless this user owns it.
        anyhow::ensure!(
            std::fs::symlink_metadata(dir)?.is_dir(),
            "{} is not a directory",
            dir.display()
        );
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("make {} private", dir.display()))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(())
}

/// Remove pasted images older than `IMAGE_TTL` (best effort).
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= IMAGE_TTL);
        if old && name.to_string_lossy().starts_with("paste-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write the image as a PNG readable only by this user; returns its path.
pub fn save_png(dir: &Path, width: usize, height: usize, rgba: &[u8]) -> Result<PathBuf> {
    anyhow::ensure!(
        width > 0 && height > 0 && rgba.len() == width * height * 4,
        "clipboard image has an unexpected size"
    );
    private_dir(dir)?;
    sweep(dir);
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let path = dir.join(format!(
        "paste-{}-{:09}-{}.png",
        stamp.as_secs(),
        stamp.subsec_nanos(),
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names every shell must read back verbatim inside single quotes.
    const ACCEPTED: [&str; 7] = [
        "/tmp/plain-file_1.txt",
        "/tmp/x;echo PWNED;#y",
        "/tmp/a b \"double\" #hash ;semi",
        "/tmp/glob*?[x]~{a,b}",
        "/tmp/日本語 🦀 é",
        "/tmp/percent%PATH% caret^ amp&",
        "/tmp/paren(1) <lt> |pipe",
    ];
    /// The reviewer's mismatched-shell payloads: all refused.
    const REFUSED: [&str; 10] = [
        "/tmp/a\\'; echo PWNED-FISH #",
        "/tmp/it's",
        "/tmp/a\\b",
        "/tmp/$(echo PWNED)",
        "/tmp/`echo PWNED`",
        "/tmp/bang!!",
        "/tmp/a\nrm -rf ~",
        "/tmp/\x1b[201~x",
        "/tmp/‘typographic’",
        "/tmp/del\x7f",
    ];

    #[test]
    fn windows_paths_keep_their_backslashes_and_refuse_cmd_metacharacters() {
        let q = |s: &str| quote_for(Path::new(s), true);
        assert_eq!(
            q("C:\\Users\\runneradmin\\a b.txt").unwrap(),
            "'C:\\Users\\runneradmin\\a b.txt'"
        );
        assert_eq!(
            q("C:\\Users\\x\\日本 ü.txt").unwrap(),
            "'C:\\Users\\x\\日本 ü.txt'"
        );
        for bad in [
            "C:\\a&b",
            "C:\\a|b",
            "C:\\a^b",
            "C:\\%PATH%",
            "C:\\a(1)",
            "C:\\a<b>",
            "C:\\a\"b",
            "C:\\it's",
            "C:\\$x",
            "C:\\a!",
        ] {
            assert!(q(bad).is_err(), "{bad}");
        }
        // A trailing backslash (a drive root) is refused: `'C:\'` would not
        // end the word in fish.
        assert!(q("C:\\").is_err());
        assert!(q("D:\\data\\").is_err());
        // The same backslash is refused on Unix (fish reads it as an escape).
        assert!(quote_for(Path::new("/tmp/a\\b"), false).is_err());
    }

    #[test]
    fn one_portable_form_and_everything_else_refused() {
        for name in ACCEPTED {
            if cfg!(windows) && name.contains(['&', '|', '^', '%', '(', ')', '<', '>', '"']) {
                assert!(quote(Path::new(name)).is_err(), "{name:?} on Windows");
                continue;
            }
            assert_eq!(quote(Path::new(name)).unwrap(), format!("'{name}'"));
        }
        for name in REFUSED {
            let error = quote_for(Path::new(name), false).unwrap_err();
            assert!(error.0.contains("can't be pasted safely"), "{error}");
            assert!(
                !error.0.contains('\n') && !error.0.contains('\x1b'),
                "status shows controls escaped"
            );
        }
        for c in [
            '\'', '\\', '!', '$', '`', '\n', '\r', '\t', '\x15', '\u{9b}',
        ] {
            assert!(
                quote_for(Path::new(&format!("/tmp/x{c}y")), false).is_err(),
                "{c:?}"
            );
        }
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(paths_text(&[p("/a"), p("/b c")]).unwrap(), "'/a' '/b c' ");
        assert!(paths_text(&[p("/a"), p("/b'c")]).is_err(), "all or nothing");
    }

    /// Run `program args... <script>`; None when the shell is not installed.
    fn run(program: &str, args: &[&str], script: &str) -> Option<String> {
        let out = std::process::Command::new(program)
            .args(args)
            .arg(script)
            .output()
            .ok()?;
        assert!(
            out.status.success(),
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(String::from_utf8(out.stdout).unwrap())
    }

    #[test]
    fn the_same_quoted_word_reads_back_verbatim_in_every_installed_shell() {
        let shells: [(&str, &[&str]); 8] = [
            ("sh", &["-c"]),
            ("bash", &["--norc", "-c"]),
            ("zsh", &["-f", "-c"]),
            ("dash", &["-c"]),
            ("ksh", &["-c"]),
            ("fish", &["--no-config", "-c"]),
            ("tcsh", &["-f", "-c"]),
            ("pwsh", &["-NoProfile", "-NonInteractive", "-Command"]),
        ];
        let mut checked = Vec::new();
        for (program, args) in shells {
            // Windows: the POSIX shells there (Git Bash, MSYS) rewrite
            // path-like arguments; PowerShell is the shell that matters.
            if cfg!(windows) && program != "pwsh" {
                continue;
            }
            for name in ACCEPTED {
                let Ok(word) = quote(Path::new(name)) else {
                    continue;
                };
                let script = if program == "pwsh" {
                    format!(
                        "[Console]::OutputEncoding = [Text.Encoding]::UTF8; [Console]::Out.Write('<' + {word} + '>')"
                    )
                } else {
                    format!("printf '<%s>' {word}")
                };
                let Some(out) = run(program, args, &script) else {
                    break;
                };
                assert_eq!(
                    out,
                    format!("<{name}>"),
                    "{program} read {word} differently"
                );
                if !checked.contains(&program) {
                    checked.push(program);
                }
            }
        }
        eprintln!("portable quoting checked with: {checked:?}");
        if !cfg!(windows) {
            assert!(checked.contains(&"sh"), "sh is always installed on Unix");
        }
        if std::env::var_os("CI").is_some() && cfg!(windows) {
            assert!(
                checked.contains(&"pwsh"),
                "pwsh is installed on the Windows runners"
            );
        }
    }

    #[test]
    fn pasted_images_are_private_png_files_that_expire() {
        let base = std::env::temp_dir().join(format!("crow-clip-test-{}", std::process::id()));
        let dir = base.join("pasted-images");
        let _ = std::fs::remove_dir_all(&base);
        let path = save_png(&dir, 2, 1, &[255, 0, 0, 255, 0, 0, 255, 128]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Expired files go on the next paste; fresh ones stay.
        let old = dir.join("paste-old.png");
        std::fs::write(&old, b"x").unwrap();
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(SystemTime::now() - IMAGE_TTL - Duration::from_secs(60))
            .unwrap();
        drop(file);
        sweep(&dir);
        assert!(!old.exists());
        assert!(path.exists());
        assert!(save_png(&dir, 2, 2, &[0; 3]).is_err(), "size mismatch");
        let _ = std::fs::remove_dir_all(&base);
    }
}
