//! Links in the terminal: which link is under the pointer (an OSC 8 hyperlink
//! or a URL detected in the text, also across soft-wrapped rows), whether it
//! may be opened, and how it is opened. Pure except `open`; unit-tested.
//!
//! Policy (an allow-list): `http` and `https` open on modifier+click;
//! `mailto` too, with only `to`, `cc`, `bcc`, `subject` and `body` kept (no
//! `attach=`). An OSC 8 link whose visible text names another host asks
//! first. `file:` asks first (a second modifier+click, at least 0.5 s later,
//! after releasing the button) and never runs a program: executables,
//! scripts, bundles and launchers are revealed in the file manager instead.
//! Every other scheme is refused, as are URLs with spaces or control
//! characters. Links open through the OS opener with the URL (or path) as one
//! argument, never a shell. While the modifier is held, the status line shows
//! the real target, with its host in punycode and invisible characters shown.
use crate::model::Screen;

/// Longest URL that is ever opened.
pub const MAX_URL: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub url: String,
    /// Every (row, col) cell the link covers, for the hover underline.
    pub cells: Vec<(usize, usize)>,
    /// From an OSC 8 hyperlink (the program chose the target).
    pub osc8: bool,
    /// What the cells show (for an OSC 8 link, may differ from the target).
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Open,
    Confirm,
    Refuse,
}

/// One cell of a logical line (soft-wrapped rows joined).
struct Unit<'a> {
    text: &'a str,
    row: usize,
    col: usize,
    link: Option<u16>,
}

/// The logical line through `row`: the rows joined by soft wraps.
fn logical_line(screen: &Screen, row: usize) -> Vec<Unit<'_>> {
    let wraps = |r: usize| screen.wrapped.get(r).copied().unwrap_or(false);
    let mut first = row;
    while first > 0 && wraps(first - 1) {
        first -= 1;
    }
    let mut last = row;
    while wraps(last) && last + 1 < screen.cells.len() {
        last += 1;
    }
    let mut units = Vec::new();
    for r in first..=last {
        let Some(cells) = screen.cells.get(r) else {
            break;
        };
        for (col, cell) in cells.iter().enumerate() {
            if cell.wide_spacer {
                continue;
            }
            units.push(Unit {
                text: if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                },
                row: r,
                col,
                link: cell.link,
            });
        }
    }
    units
}

/// The link under (row, col): an OSC 8 hyperlink first, else a URL in the text.
pub fn link_at(screen: &Screen, row: usize, col: usize) -> Option<Hit> {
    let units = logical_line(screen, row);
    // A wide character's spacer belongs to the character before it.
    let at = units
        .iter()
        .rposition(|unit| unit.row == row && unit.col <= col)?;
    if let Some(index) = units[at].link
        && let Some(url) = screen.links.get(usize::from(index))
    {
        let mut start = at;
        while start > 0 && units[start - 1].link == Some(index) {
            start -= 1;
        }
        let mut end = at;
        while end + 1 < units.len() && units[end + 1].link == Some(index) {
            end += 1;
        }
        return Some(Hit {
            url: url.clone(),
            cells: units[start..=end].iter().map(|u| (u.row, u.col)).collect(),
            osc8: true,
            text: units[start..=end].iter().map(|u| u.text).collect(),
        });
    }
    // Detected URLs: byte offsets of the joined text map back to units.
    let mut text = String::new();
    let mut owner = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        text.push_str(unit.text);
        owner.extend(std::iter::repeat_n(index, unit.text.len()));
    }
    find_urls(&text).into_iter().find_map(|range| {
        let (first, last) = (owner[range.start], owner[range.end - 1]);
        (first..=last).contains(&at).then(|| Hit {
            url: text[range.clone()].to_owned(),
            cells: units[first..=last].iter().map(|u| (u.row, u.col)).collect(),
            osc8: false,
            text: text[range].to_owned(),
        })
    })
}

fn scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

fn url_char(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_control()
        && !matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^')
}

/// Byte ranges of the URLs in `text`: `scheme://…` or `mailto:…`, without
/// trailing punctuation or an unbalanced closing bracket.
pub fn find_urls(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut found = Vec::new();
    let mut search = 0;
    while let Some(offset) = text[search..].find(':') {
        let colon = search + offset;
        search = colon + 1;
        let start = text[..colon]
            .char_indices()
            .rev()
            .take_while(|&(_, c)| scheme_char(c))
            .last()
            .map(|(i, _)| i)
            .unwrap_or(colon);
        // The scheme starts with a letter, at a word boundary.
        let Some(start) = text[start..colon]
            .char_indices()
            .find(|&(_, c)| c.is_ascii_alphabetic())
            .map(|(i, _)| start + i)
        else {
            continue;
        };
        let scheme = text[start..colon].to_ascii_lowercase();
        let rest = &text[colon + 1..];
        let slashes = rest.starts_with("//");
        if scheme.is_empty() || scheme.len() > 32 || !(slashes || scheme == "mailto") {
            continue;
        }
        let body = colon + 1 + if slashes { 2 } else { 0 };
        let mut end = body
            + text[body..]
                .char_indices()
                .find(|&(_, c)| !url_char(c))
                .map(|(i, _)| i)
                .unwrap_or(text.len() - body);
        // Trailing punctuation belongs to the sentence, not the URL.
        while let Some(last) = text[..end].chars().next_back() {
            let unbalanced = |open: char, close: char| {
                last == close
                    && text[start..end].matches(close).count()
                        > text[start..end].matches(open).count()
            };
            if matches!(last, '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '*')
                || unbalanced('(', ')')
                || unbalanced('[', ']')
            {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        if end > body {
            found.push(start..end);
            search = end;
        }
    }
    found
}

pub fn scheme(url: &str) -> String {
    url.split_once(':')
        .map(|(s, _)| s.to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn policy(url: &str) -> Policy {
    // Browsers read `\` as `/` (`https://evil.example\@bank.example` goes to
    // evil.example): never guess which host a backslash URL means.
    if url.is_empty()
        || url.len() > MAX_URL
        || url
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Policy::Refuse;
    }
    match scheme(url).as_str() {
        "http" | "https" => match host(url) {
            Some(host) if !host.is_empty() && !host.contains('%') => {
                // `https://bank.example@evil.example`: ask.
                if userinfo(url) {
                    Policy::Confirm
                } else {
                    Policy::Open
                }
            }
            _ => Policy::Refuse,
        },
        "mailto" => Policy::Open,
        "file" if file_path(url).is_some() => Policy::Confirm,
        _ => Policy::Refuse,
    }
}

/// What a modifier+click on `hit` does.
pub fn decide(hit: &Hit) -> Policy {
    match policy(&hit.url) {
        Policy::Open if hit.osc8 && misleading(hit) => Policy::Confirm,
        policy => policy,
    }
}

fn authority(url: &str) -> Option<&str> {
    url.split_once("://")?.1.split(['/', '?', '#']).next()
}

/// The URL carries a user name or password before its host.
pub fn userinfo(url: &str) -> bool {
    authority(url).is_some_and(|authority| authority.contains('@'))
}

/// The host of a `scheme://host…` URL, lowercased, without user info/port.
pub fn host(url: &str) -> Option<String> {
    let host = authority(url)?.rsplit('@').next()?;
    let host = if host.starts_with('[') {
        host.split(']').next().map(|h| format!("{h}]"))?
    } else {
        host.split(':').next()?.to_owned()
    };
    Some(host.trim_end_matches('.').to_lowercase())
}

/// Hosts named in `text`: in URLs, or as bare domains (`bank.example`,
/// `www.bank.example/login`), anywhere in the text.
fn hosts_in(text: &str) -> Vec<String> {
    let mut hosts: Vec<String> = find_urls(text)
        .into_iter()
        .filter_map(|range| host(&text[range]))
        .collect();
    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '.' || c == '-')) {
        let word = word.trim_matches('.').to_lowercase();
        let labels: Vec<&str> = word.split('.').collect();
        if labels.len() >= 2
            && labels
                .iter()
                .all(|l| !l.is_empty() && !l.starts_with('-') && !l.ends_with('-'))
            && labels
                .last()
                .is_some_and(|tld| tld.chars().count() >= 2 && tld.chars().all(char::is_alphabetic))
        {
            hosts.push(word);
        }
    }
    hosts
}

/// An OSC 8 link whose visible text could make the reader expect another
/// destination: any non-ASCII character in a text that is not the URL itself
/// (dot lookalikes such as U+2024, U+FF0E, U+3002, invisible and combining
/// characters, other scripts), or an ASCII domain anywhere in it that is not
/// the target's host.
fn misleading(hit: &Hit) -> bool {
    let text = hit.text.trim();
    if text == hit.url {
        return false;
    }
    if !text.is_ascii() {
        return true;
    }
    let Some(target) = host(&hit.url) else {
        return false;
    };
    let same = |a: &str, b: &str| {
        let strip = |h: &str| ascii_host(h.trim_start_matches("www.")).to_lowercase();
        strip(a) == strip(b)
    };
    hosts_in(text).iter().any(|shown| !same(shown, &target))
}

/// `mailto:` with only the address and the to/cc/bcc/subject/body fields.
pub fn sanitize_mailto(url: &str) -> String {
    let Some((head, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|field| {
            let name = field.split('=').next().unwrap_or("").to_ascii_lowercase();
            matches!(name.as_str(), "to" | "cc" | "bcc" | "subject" | "body")
        })
        .collect();
    if kept.is_empty() {
        head.to_owned()
    } else {
        format!("{head}?{}", kept.join("&"))
    }
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The local path of a `file:` URL (`file:///p`, `file://localhost/p`).
/// Other hosts and UNC paths (`file:////host/share`) are not local: `None`.
pub fn file_path(url: &str) -> Option<std::path::PathBuf> {
    let rest = url
        .get(5..)
        .filter(|_| scheme(url) == "file")?
        .strip_prefix("//")?;
    let path = if rest.starts_with('/') {
        rest
    } else {
        let (host, path) = rest.split_at(rest.find('/')?);
        if !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        path
    };
    let path = percent_decode(path.split(['?', '#']).next()?)?;
    if path.starts_with("//") || path.contains('\\') || path.chars().any(char::is_control) {
        return None;
    }
    // file:///C:/dir on Windows.
    #[cfg(windows)]
    let path = path
        .strip_prefix('/')
        .filter(|p| p.get(1..2) == Some(":"))
        .unwrap_or(&path)
        .to_owned();
    Some(std::path::PathBuf::from(path))
}

/// Documents a `file:` link may open with their default application. Only
/// formats whose default handler shows content and runs nothing on macOS,
/// Linux and Windows. HTML, SVG, source code and scripts are not here (a
/// browser or an interpreter may be their handler): they are revealed.
pub const OPENABLE: [&str; 13] = [
    "txt", "md", "markdown", "pdf", "png", "jpg", "jpeg", "gif", "webp", "csv", "tsv", "log",
    "json",
];

/// What a `file:` link to `path` may do: open the real file (an allowed
/// document), else reveal it in the file manager. Symlinks and `..` are
/// resolved first, so a link cannot name a document and reach an app.
pub fn file_action(path: &std::path::Path) -> anyhow::Result<Opened> {
    check_file(path).map(|(action, _)| action)
}

/// The identity of the file that was checked (device, inode on Unix).
pub type FileId = Option<(u64, u64)>;

fn file_id(meta: &std::fs::Metadata) -> FileId {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// The real path (symlinks and `..` resolved; Windows verbatim prefix removed).
fn real_path(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let real = std::fs::canonicalize(path)
        .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    // Windows canonical paths are verbatim (`\\?\C:\…`); openers want `C:\…`.
    #[cfg(windows)]
    let real = match real.to_str().and_then(|p| p.strip_prefix(r"\\?\")) {
        Some(plain) if !plain.starts_with("UNC") => std::path::PathBuf::from(plain),
        _ => real,
    };
    Ok(real)
}

/// `file_action`, plus the identity of the file it checked.
fn check_file(path: &std::path::Path) -> anyhow::Result<(Opened, FileId)> {
    let real = real_path(path)?;
    let id = std::fs::symlink_metadata(&real)
        .ok()
        .and_then(|meta| file_id(&meta));
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = real
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let document = real.is_file()
        && !name.ends_with('.')
        && OPENABLE.contains(&ext.as_str())
        && !executable(&real)
        && !real.components().any(|c| {
            let c = c.as_os_str().to_string_lossy().to_ascii_lowercase();
            c.ends_with(".app") || c.ends_with(".bundle") || c.ends_with(".framework")
        });
    Ok((
        if document {
            Opened::File(real)
        } else {
            Opened::Revealed(real)
        },
        id,
    ))
}

#[cfg(unix)]
fn executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn executable(_: &std::path::Path) -> bool {
    false
}

/// RFC 3492 punycode of one label.
fn punycode(label: &str) -> Option<String> {
    const BASE: u32 = 36;
    let digit = |d: u32| {
        char::from(if d < 26 {
            b'a' + d as u8
        } else {
            b'0' + (d - 26) as u8
        })
    };
    let adapt = |mut delta: u32, points: u32, first: bool| {
        delta /= if first { 700 } else { 2 };
        delta += delta / points;
        let mut k = 0;
        while delta > ((BASE - 1) * 26) / 2 {
            delta /= BASE - 1;
            k += BASE;
        }
        k + (BASE * delta) / (delta + 38)
    };
    let input: Vec<u32> = label.chars().map(u32::from).collect();
    let mut out: String = label.chars().filter(char::is_ascii).collect();
    let basic = out.len() as u32;
    let mut handled = basic;
    if basic > 0 {
        out.push('-');
    }
    let (mut n, mut delta, mut bias) = (128u32, 0u32, 72u32);
    while (handled as usize) < input.len() {
        let m = *input.iter().filter(|&&c| c >= n).min()?;
        delta = delta.checked_add((m - n).checked_mul(handled + 1)?)?;
        n = m;
        for &c in &input {
            if c < n {
                delta = delta.checked_add(1)?;
            }
            if c == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = if k <= bias {
                        1
                    } else if k >= bias + 26 {
                        26
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    out.push(digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                out.push(digit(q));
                bias = adapt(delta, handled + 1, handled == basic);
                delta = 0;
                handled += 1;
            }
        }
        delta += 1;
        n += 1;
    }
    Some(out)
}

/// A host as ASCII: non-ASCII labels in punycode (`xn--…`).
pub fn ascii_host(host: &str) -> String {
    host.split('.')
        .map(|label| {
            if label.is_ascii() {
                label.to_owned()
            } else {
                punycode(&label.to_lowercase())
                    .map_or_else(|| "xn--?".into(), |p| format!("xn--{p}"))
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Characters that change how text looks without being seen: bidi controls,
/// zero-width characters, the BOM.
pub fn invisible(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}' | '\u{061c}' | '\u{feff}' | '\u{180e}' | '\u{00ad}')
}

fn escape_invisible(text: &str) -> String {
    text.chars()
        .map(|c| {
            if invisible(c) || c.is_control() {
                format!("\\u{{{:x}}}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn elide(text: &str, keep: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= keep * 2 + 1 {
        return text.to_owned();
    }
    let head: String = chars[..keep].iter().collect();
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("{head}…{tail}")
}

/// The target as the status line shows it: `scheme://[user@]host[:port]`
/// always in full (host in punycode), only the rest shortened; invisible and
/// control characters written as `\u{…}`.
pub fn display(url: &str) -> String {
    let (Some((scheme, rest)), Some(host)) = (url.split_once("://"), host(url)) else {
        return elide(&escape_invisible(url), 48);
    };
    let at = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..at];
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((user, hostport)) => (format!("{user}@"), hostport),
        None => (String::new(), authority),
    };
    let port = if hostport.starts_with('[') {
        hostport.split_once(']').map_or("", |(_, port)| port)
    } else {
        hostport.find(':').map_or("", |i| &hostport[i..])
    };
    let origin = format!("{scheme}://{user}{}{port}", ascii_host(&host));
    format!(
        "{}{}",
        escape_invisible(&origin),
        elide(&escape_invisible(&rest[at..]), 30)
    )
}

/// A second modifier+click confirms a link that asks first, but only as a
/// separate, deliberate click: at least `MIN` after the prompt, with the
/// button released in between, within `MAX` (a double-click never confirms).
#[derive(Debug, Default)]
pub struct Confirm {
    pending: Option<(String, std::time::Instant, bool)>,
}

impl Confirm {
    pub const MIN: std::time::Duration = std::time::Duration::from_millis(500);
    pub const MAX: std::time::Duration = std::time::Duration::from_secs(5);

    /// A modifier+click on a link that asks first: true when it confirms.
    pub fn click(&mut self, url: &str, now: std::time::Instant) -> bool {
        if let Some((pending, at, released)) = &self.pending
            && pending == url
        {
            let waited = now.saturating_duration_since(*at);
            if *released && waited >= Self::MIN && waited < Self::MAX {
                self.pending = None;
                return true;
            }
            if waited < Self::MAX {
                // Too soon (a double-click): the prompt stands, unconfirmed.
                return false;
            }
        }
        self.pending = Some((url.to_owned(), now, false));
        false
    }

    /// The mouse button went up.
    pub fn released(&mut self) {
        if let Some((_, _, released)) = &mut self.pending {
            *released = true;
        }
    }
}

/// The OS opener for `target` (a URL or a local path): one argument, no shell.
pub fn opener(target: &str) -> std::process::Command {
    #[cfg(target_os = "macos")]
    let command = {
        let mut command = std::process::Command::new("/usr/bin/open");
        command.arg(target);
        command
    };
    #[cfg(windows)]
    let command = {
        // The URL protocol handler, as Explorer uses it; `cmd /c start` would
        // let a shell parse the URL.
        let mut command = std::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").arg(target);
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(target);
        command
    };
    command
}

/// Show `path` in the file manager without opening it.
pub fn revealer(path: &std::path::Path) -> std::process::Command {
    #[cfg(target_os = "macos")]
    let command = {
        let mut command = std::process::Command::new("/usr/bin/open");
        command.arg("-R").arg(path);
        command
    };
    #[cfg(windows)]
    let command = {
        let mut command = std::process::Command::new("explorer.exe");
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path);
        command.arg(arg);
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path.parent().unwrap_or(std::path::Path::new("/")));
        command
    };
    command
}

/// What `open` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    Url(String),
    File(std::path::PathBuf),
    Revealed(std::path::PathBuf),
}

/// Documents larger than this are revealed instead of copied and opened.
pub const MAX_OPEN_COPY: u64 = 256 * 1024 * 1024;

/// Where verified documents are copied before opening (private, 0700).
pub fn copies_dir() -> anyhow::Result<std::path::PathBuf> {
    #[cfg(feature = "test-hooks")]
    if let Ok(dir) = crate::ipc::runtime_dir()
        && dir.join("test-opener").exists()
    {
        return Ok(dir.join("opened-files"));
    }
    let dirs = directories::ProjectDirs::from("dev", "crow", "crow-term")
        .ok_or_else(|| anyhow::anyhow!("no cache directory for opened files"))?;
    Ok(dirs.cache_dir().join("opened-files"))
}

/// Remove copies older than a day (best effort).
fn sweep_copies(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= crate::clip::IMAGE_TTL);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Copy the verified document `real` into a fresh private directory under
/// `dir` and return the copy, which is what gets opened. `checked` is the
/// identity recorded when the file was verified: the path must still resolve
/// to itself (no component turned into a symlink), and the file opened (no
/// final symlink followed) must still be that same regular file. After the
/// copy, swapping the original path changes nothing, before or after the
/// opener starts. The download marks travel with it (macOS quarantine,
/// Windows Mark of the Web), so a downloaded document keeps its protections.
pub fn private_copy(
    real: &std::path::Path,
    checked: FileId,
    dir: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    use std::io::{Read, Write};
    let changed = || anyhow::anyhow!("{} changed after it was checked", real.display());
    if real_path(real)? != real {
        return Err(changed());
    }
    let meta = std::fs::symlink_metadata(real)?;
    if !meta.is_file() || file_id(&meta) != checked {
        return Err(changed());
    }
    anyhow::ensure!(
        meta.len() <= MAX_OPEN_COPY,
        "{} is too large to open from a link",
        real.display()
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_NONBLOCK,
    );
    let source = options.open(real)?;
    let opened = source.metadata()?;
    if !opened.is_file() || file_id(&opened) != checked {
        return Err(changed());
    }
    crate::clip::private_dir(dir)?;
    sweep_copies(dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let folder = dir.join(format!(
        "{}-{:09}-{}",
        stamp.as_secs(),
        stamp.subsec_nanos(),
        std::process::id()
    ));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    #[cfg(not(unix))]
    let _ = &mut builder;
    builder.create(&folder)?;
    let name = real
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("no file name"))?;
    let copy = folder.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut target = options.open(&copy)?;
    let copied = std::io::copy(&mut (&source).take(MAX_OPEN_COPY + 1), &mut target)?;
    anyhow::ensure!(
        copied <= MAX_OPEN_COPY,
        "{} is too large to open from a link",
        real.display()
    );
    target.flush()?;
    copy_download_mark(&source, &target, real, &copy)?;
    Ok(copy)
}

/// Carry the "downloaded from the internet" mark to the copy: the
/// `com.apple.quarantine` attribute on macOS, the `Zone.Identifier` stream
/// (Mark of the Web, Office Protected View) on Windows.
#[allow(unused_variables)]
fn copy_download_mark(
    source: &std::fs::File,
    target: &std::fs::File,
    real: &std::path::Path,
    copy: &std::path::Path,
) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        const NAME: &[u8] = b"com.apple.quarantine\0";
        let mut value = vec![0u8; 4096];
        // SAFETY: valid fds and a NUL-terminated name; the buffer outlives the call.
        let size = unsafe {
            libc::fgetxattr(
                source.as_raw_fd(),
                NAME.as_ptr().cast(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        if size > 0 {
            // SAFETY: as above; `size` bytes of `value` were written.
            let status = unsafe {
                libc::fsetxattr(
                    target.as_raw_fd(),
                    NAME.as_ptr().cast(),
                    value.as_ptr().cast(),
                    size as usize,
                    0,
                    0,
                )
            };
            anyhow::ensure!(
                status == 0,
                "could not keep the quarantine mark: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    #[cfg(windows)]
    {
        let stream = |path: &std::path::Path| {
            let mut name = path.as_os_str().to_owned();
            name.push(":Zone.Identifier");
            std::path::PathBuf::from(name)
        };
        if let Ok(mark) = std::fs::read(stream(real)) {
            std::fs::write(stream(copy), mark)?;
        }
    }
    Ok(())
}

/// The command for an allowed link (no process started; a `file:` document
/// is copied first, see [`private_copy`]).
pub fn plan(url: &str) -> anyhow::Result<(Opened, std::process::Command)> {
    plan_with(url, copies_dir)
}

fn plan_with(
    url: &str,
    copies: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) -> anyhow::Result<(Opened, std::process::Command)> {
    match (policy(url), scheme(url).as_str()) {
        (Policy::Refuse, _) => anyhow::bail!("this link is not opened: {}", display(url)),
        (_, "file") => {
            let path = file_path(url)
                .ok_or_else(|| anyhow::anyhow!("not a local file: {}", display(url)))?;
            let (action, checked) = check_file(&path)?;
            Ok(match action {
                Opened::File(real) => {
                    let copy = private_copy(&real, checked, &copies()?)?;
                    let command = opener(&copy.to_string_lossy());
                    (Opened::File(copy), command)
                }
                Opened::Revealed(real) => {
                    let command = revealer(&real);
                    (Opened::Revealed(real), command)
                }
                Opened::Url(_) => unreachable!("file links are files"),
            })
        }
        (_, "mailto") => {
            let url = sanitize_mailto(url);
            let command = opener(&url);
            Ok((Opened::Url(url), command))
        }
        _ => Ok((Opened::Url(url.to_owned()), opener(url))),
    }
}

/// Start the opener without waiting for it (reaped in the background).
pub fn open(url: &str) -> anyhow::Result<Opened> {
    use std::process::Stdio;
    let (opened, mut command) = plan(url)?;
    // Test hook (GUI journeys): record the link instead of opening a browser
    // when `<runtime>/test-opener` exists.
    #[cfg(feature = "test-hooks")]
    if let Ok(dir) = crate::ipc::runtime_dir()
        && dir.join("test-opener").exists()
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("test-opened-links"))?;
        let line = match &opened {
            Opened::Url(url) => url.clone(),
            Opened::File(path) => format!("file {}", path.display()),
            Opened::Revealed(path) => format!("reveal {}", path.display()),
        };
        writeln!(file, "{line}")?;
        return Ok(opened);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(opened)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn screen(rows: &[&str], wrapped: &[bool]) -> Screen {
        let cols = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0);
        let cells = rows
            .iter()
            .map(|row| {
                let mut cells: Vec<Cell> = row.chars().map(|c| cell(&c.to_string())).collect();
                cells.resize(cols, cell(" "));
                cells
            })
            .collect();
        Screen {
            size: Size {
                rows: rows.len() as u16,
                cols: cols as u16,
            },
            cells,
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
                shape: "block".into(),
            },
            modes: Modes::default(),
            title: String::new(),
            alive: true,
            display_offset: 0,
            bell: 0,
            links: Vec::new(),
            clipboard_seq: 0,
            wrapped: wrapped.to_vec(),
        }
    }

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

    fn urls(text: &str) -> Vec<&str> {
        find_urls(text).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn urls_are_found_without_the_sentence_around_them() {
        assert_eq!(
            urls("see https://example.com/a?b=1&c=2#x."),
            ["https://example.com/a?b=1&c=2#x"]
        );
        assert_eq!(
            urls("(https://en.wikipedia.org/wiki/Rust_(language))"),
            ["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
        assert_eq!(urls("[docs](https://docs.rs/x)"), ["https://docs.rs/x"]);
        assert_eq!(
            urls("mail mailto:a@b.c, or http://x.y:8080/p;"),
            ["mailto:a@b.c", "http://x.y:8080/p"]
        );
        assert_eq!(urls("'file:///tmp/a b'"), ["file:///tmp/a"]);
        assert_eq!(urls("<https://a.b/c>"), ["https://a.b/c"]);
        assert_eq!(urls("https://日本.jp/パス"), ["https://日本.jp/パス"]);
        assert!(urls("time 12:30, C:\\dir, key: value, http://").is_empty());
    }

    #[test]
    fn a_url_wrapped_across_rows_is_one_link() {
        let s = screen(
            &["go https://exam", "ple.com/long/pa", "th now         "],
            &[true, true, false],
        );
        let hit = link_at(&s, 1, 3).unwrap();
        assert_eq!(hit.url, "https://example.com/long/path");
        assert_eq!(hit.cells.first(), Some(&(0, 3)));
        assert_eq!(hit.cells.last(), Some(&(2, 1)));
        assert_eq!(link_at(&s, 0, 6).unwrap().url, hit.url);
        assert!(link_at(&s, 2, 4).is_none(), "not on the link");
        // The same rows without a soft wrap are two separate texts.
        let s = screen(&["go https://exam", "ple.com/long/pa"], &[false, false]);
        assert_eq!(link_at(&s, 0, 6).unwrap().url, "https://exam");
        assert!(link_at(&s, 1, 3).is_none());
    }

    #[test]
    fn an_osc8_hyperlink_wins_over_the_text_under_it() {
        let mut s = screen(&["click https://shown.example here"], &[false]);
        s.links = vec!["https://target.example/".into()];
        for col in 6..26 {
            s.cells[0][col].link = Some(0);
        }
        let hit = link_at(&s, 0, 10).unwrap();
        assert_eq!(hit.url, "https://target.example/");
        assert!(hit.osc8);
        assert_eq!(hit.cells.len(), 20);
        assert!(link_at(&s, 0, 2).is_none());
    }

    #[test]
    fn wide_characters_map_to_their_cells() {
        let mut s = screen(&["界 https://a.b/界 x"], &[false]);
        // "界" is two cells: insert spacers after each.
        let mut row = Vec::new();
        for c in s.cells[0].drain(..) {
            let wide = c.text == "界";
            row.push(c);
            if wide {
                let mut spacer = cell(" ");
                spacer.wide_spacer = true;
                row.push(spacer);
            }
        }
        s.cells[0] = row;
        let hit = link_at(&s, 0, 16).unwrap();
        assert_eq!(hit.url, "https://a.b/界");
        assert_eq!(hit.cells.first(), Some(&(0, 3)));
        assert_eq!(hit.cells.last(), Some(&(0, 15)));
    }

    #[test]
    fn only_allowed_schemes_open() {
        assert_eq!(policy("https://example.com"), Policy::Open);
        assert_eq!(policy("HTTP://example.com/x"), Policy::Open);
        assert_eq!(policy("mailto:a@b.c"), Policy::Open);
        assert_eq!(policy("file:///etc/hosts"), Policy::Confirm);
        assert_eq!(policy("file://localhost/etc/hosts"), Policy::Confirm);
        // A user name before the host is a classic disguise: ask.
        assert_eq!(
            policy("https://bank.example@evil.example/"),
            Policy::Confirm
        );
        for refused in [
            // Browsers read `\` as `/`: this goes to evil.example.
            "https://evil.example\\@bank.example",
            "https://evil.example\\.bank.example/",
            "https://evil%2eexample/",
            "file://evil.example/share/x",
            "file:////host/share/x",
            "file://///host/share",
            "ssh://host",
            "ftp://host/x",
            "javascript:alert(1)",
            "data:text/html,x",
            "vscode://file/x",
            "ms-settings:",
            "search-ms:query=x",
            "C:\\Windows\\System32\\calc.exe",
            "c:/x",
            "https://a.b/\nrm",
            "https://a.b/ c",
            "http:///nohost",
            "-x://y",
        ] {
            assert_eq!(policy(refused), Policy::Refuse, "{refused}");
        }
        assert_eq!(
            policy(&format!("https://a.b/{}", "x".repeat(MAX_URL))),
            Policy::Refuse
        );
    }

    #[test]
    fn mailto_keeps_only_address_and_message_fields() {
        assert_eq!(
            sanitize_mailto(
                "mailto:a@b.c?subject=Hi&attach=/etc/passwd&Attachment=x&body=yo&cc=d@e.f"
            ),
            "mailto:a@b.c?subject=Hi&body=yo&cc=d@e.f"
        );
        assert_eq!(
            sanitize_mailto("mailto:a@b.c?attach=~/.ssh/id_rsa"),
            "mailto:a@b.c"
        );
        let (opened, command) = plan("mailto:a@b.c?attach=x").unwrap();
        assert_eq!(opened, Opened::Url("mailto:a@b.c".into()));
        assert_eq!(command.get_args().last().unwrap(), "mailto:a@b.c");
    }

    #[test]
    fn an_osc8_link_whose_text_names_another_host_asks_first() {
        let hit = |text: &str, url: &str| Hit {
            url: url.into(),
            cells: vec![],
            osc8: true,
            text: text.into(),
        };
        for (text, url) in [
            ("https://bank.example/login", "https://evil.example/"),
            ("bank.example", "https://evil.example/"),
            ("www.Bank.example/login", "https://evil.example/"),
            // Anywhere in the text, not only at the start.
            ("Log in at bank.example now", "https://evil.example/"),
            ("see https://docs.rs and bank.example", "https://docs.rs/"),
            // Lookalike dots, invisible and combining characters, other scripts.
            ("bank\u{2024}example", "https://evil.example/"),
            ("bank\u{ff0e}example", "https://evil.example/"),
            ("bank\u{3002}example", "https://evil.example/"),
            ("bank\u{ff61}example", "https://evil.example/"),
            ("bank.exa\u{200b}mple", "https://evil.example/"),
            ("\u{feff}bank.example", "https://evil.example/"),
            ("bank.e\u{338}xample", "https://evil.example/"),
            ("evil\u{2024}example", "https://evil.example/"),
            ("https://bücher.example", "https://xn--bcher-kva.example/"),
            ("Zahlung bestätigen", "https://evil.example/"),
        ] {
            assert_eq!(decide(&hit(text, url)), Policy::Confirm, "{text} -> {url}");
        }
        for (text, url) in [
            ("click here", "https://evil.example/"),
            ("docs.rs/x", "https://docs.rs/x/latest"),
            ("www.docs.rs", "https://docs.rs/"),
            ("v1.2.3 released", "https://github.com/o/r/releases"),
        ] {
            assert_eq!(decide(&hit(text, url)), Policy::Open, "{text} -> {url}");
        }
        let shown = Hit {
            url: "https://a.example".into(),
            cells: vec![],
            osc8: false,
            text: "https://a.example".into(),
        };
        assert_eq!(decide(&shown), Policy::Open);
    }

    #[test]
    fn the_status_always_shows_the_whole_origin_and_shortens_only_the_path() {
        assert_eq!(ascii_host("bücher.example"), "xn--bcher-kva.example");
        assert_eq!(ascii_host("münchen.de"), "xn--mnchen-3ya.de");
        assert_eq!(ascii_host("例え.テスト"), "xn--r8jz45g.xn--zckzah");
        assert_eq!(display("https://аpple.com/x"), "https://xn--pple-43d.com/x");
        assert_eq!(
            display("https://user@bücher.example:8443/p?q"),
            "https://user@xn--bcher-kva.example:8443/p?q"
        );
        assert_eq!(
            display("https://a.example/\u{202e}fdp.exe"),
            "https://a.example/\\u{202e}fdp.exe"
        );
        // An invisible character in the host makes it punycode: visibly not ab.example.
        assert_eq!(
            display("https://a\u{200b}b.example/"),
            "https://xn--ab-g1t.example/"
        );
        let long_host = format!(
            "https://{}.bank.example.evil.example:8443",
            "login".repeat(12)
        );
        let shown = display(&format!("{long_host}/{}", "p".repeat(300)));
        assert!(shown.starts_with(&long_host), "{shown}");
        assert!(
            shown.chars().count() < long_host.chars().count() + 70,
            "{shown}"
        );
    }

    #[test]
    fn a_double_click_never_confirms_a_link_that_asks_first() {
        let start = std::time::Instant::now();
        let at = |ms| start + std::time::Duration::from_millis(ms);
        let mut confirm = Confirm::default();
        assert!(!confirm.click("file:///x", at(0)), "first click only asks");
        confirm.released();
        assert!(
            !confirm.click("file:///x", at(150)),
            "double-click: too soon"
        );
        confirm.released();
        assert!(
            confirm.click("file:///x", at(900)),
            "deliberate second click"
        );
        let mut confirm = Confirm::default();
        assert!(!confirm.click("file:///x", at(0)));
        assert!(
            !confirm.click("file:///x", at(1000)),
            "button never released"
        );
        let mut confirm = Confirm::default();
        confirm.click("file:///x", at(0));
        confirm.released();
        assert!(!confirm.click("file:///x", at(6000)), "too late");
        confirm.released();
        assert!(!confirm.click("file:///y", at(7000)), "another link");
    }

    /// `file:` URL of a local path, on any platform.
    fn file_url(path: &std::path::Path) -> String {
        let text = path
            .to_string_lossy()
            .replace('\\', "/")
            .replace(' ', "%20");
        if text.starts_with('/') {
            format!("file://{text}")
        } else {
            format!("file:///{text}")
        }
    }

    #[test]
    fn file_links_open_only_documents_and_reveal_everything_else() {
        // Canonical, without the Windows verbatim prefix (`\\?\`).
        let canonical = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let canonical = canonical
            .to_str()
            .and_then(|p| p.strip_prefix(r"\\?\"))
            .map_or(canonical.clone(), std::path::PathBuf::from);
        let dir = canonical.join(format!("crow-links-{}", std::process::id()));
        let copies = dir.join("copies");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = |p: &std::path::Path| file_action(p).unwrap();
        for name in ["notes.txt", "README.md", "doc.pdf", "shot.png", "data.json"] {
            let path = dir.join(name);
            std::fs::write(&path, "x").unwrap();
            assert!(matches!(real(&path), Opened::File(_)), "{name}");
            let (opened, command) = plan_with(&file_url(&path), || Ok(copies.clone())).unwrap();
            // What opens is a private copy, never the path a link named.
            let Opened::File(copy) = opened else {
                panic!("{name} not opened")
            };
            assert!(
                copy.starts_with(&copies) && copy.file_name() == path.file_name(),
                "{copy:?}"
            );
            assert_eq!(command.get_args().last().unwrap(), copy.as_os_str());
        }
        for name in [
            "run.command",
            "a.desktop",
            "setup.exe",
            "s.ps1",
            "b.bat",
            "l.lnk",
            "x.webloc",
            "x.inetloc",
            "x.fileloc",
            "x.hta",
            "x.wsf",
            "x.cpl",
            "x.scf",
            "page.html",
            "img.svg",
            "main.rs",
            "script.py",
            "trailing.txt.",
            "noext",
        ] {
            // Windows drops a trailing dot: "x.txt." is the document x.txt there.
            if cfg!(windows) && name.ends_with('.') {
                continue;
            }
            let path = dir.join(name);
            std::fs::write(&path, "x").unwrap();
            assert!(matches!(real(&path), Opened::Revealed(_)), "{name}");
        }
        // A bundle, a path into it, a symlink to it: revealed, never opened.
        let app = dir.join("Evil.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/readme.txt"), "x").unwrap();
        assert!(matches!(real(&app), Opened::Revealed(_)));
        assert_eq!(
            real(&app.join("Contents/..")),
            real(&app),
            "`..` resolved to the bundle"
        );
        assert!(matches!(
            real(&app.join("Contents/MacOS/readme.txt")),
            Opened::Revealed(_)
        ));
        assert!(
            matches!(real(&dir), Opened::Revealed(_)),
            "folders are revealed"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let link = dir.join("innocent.txt");
            std::os::unix::fs::symlink(&app, &link).unwrap();
            assert!(
                matches!(real(&link), Opened::Revealed(p) if p == app),
                "symlink to an app"
            );
            let tool = dir.join("tool.txt");
            std::fs::write(&tool, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(matches!(real(&tool), Opened::Revealed(_)), "executable bit");
        }
        assert!(file_action(&dir.join("missing.txt")).is_err());
        let checked = |p: &std::path::Path| match check_file(p).unwrap() {
            (Opened::File(real), id) => (real, id),
            other => panic!("{other:?}"),
        };
        #[cfg(unix)]
        {
            // Swapped for a symlink after the check: refused.
            let doc = dir.join("swap.txt");
            std::fs::write(&doc, "x").unwrap();
            let (real, id) = checked(&doc);
            std::fs::remove_file(&doc).unwrap();
            std::os::unix::fs::symlink(&app, &doc).unwrap();
            assert!(
                private_copy(&real, id, &copies).is_err(),
                "symlink after the check"
            );
            // Replaced by another regular file (the first one kept, so its
            // inode is not reused): refused.
            let doc = dir.join("replaced.txt");
            std::fs::write(&doc, "x").unwrap();
            let (real, id) = checked(&doc);
            std::fs::rename(&doc, dir.join("kept.txt")).unwrap();
            std::fs::write(&doc, "other").unwrap();
            assert!(
                private_copy(&real, id, &copies).is_err(),
                "another file after the check"
            );
            // A folder on the way turned into a symlink: refused.
            let sub = dir.join("sub");
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("doc.txt"), "x").unwrap();
            let (real, id) = checked(&sub.join("doc.txt"));
            std::fs::rename(&sub, dir.join("moved")).unwrap();
            std::os::unix::fs::symlink(dir.join("moved"), &sub).unwrap();
            assert!(
                private_copy(&real, id, &copies).is_err(),
                "intermediate symlink"
            );
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            let (real, id) = checked(&dir.join("notes.txt"));
            let copy = private_copy(&real, id, &copies).unwrap();
            assert_eq!(
                (mode(&copies), mode(copy.parent().unwrap()), mode(&copy)),
                (0o700, 0o700, 0o600)
            );
            assert_eq!(std::fs::read_to_string(&copy).unwrap(), "x");
        }
        #[cfg(target_os = "macos")]
        {
            // A downloaded document keeps its quarantine mark in the copy.
            use std::os::unix::ffi::OsStrExt;
            let doc = dir.join("downloaded.pdf");
            std::fs::write(&doc, "%PDF").unwrap();
            let c = |p: &std::path::Path| std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
            let name = c"com.apple.quarantine";
            let mark = b"0083;66d0f000;Safari;";
            // SAFETY: valid NUL-terminated strings and buffers.
            let set = unsafe {
                libc::setxattr(
                    c(&doc).as_ptr(),
                    name.as_ptr(),
                    mark.as_ptr().cast(),
                    mark.len(),
                    0,
                    0,
                )
            };
            assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
            let (real, id) = checked(&doc);
            let copy = private_copy(&real, id, &copies).unwrap();
            let mut value = [0u8; 64];
            // SAFETY: as above.
            let got = unsafe {
                libc::getxattr(
                    c(&copy).as_ptr(),
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            };
            assert_eq!(
                &value[..got.max(0) as usize],
                mark,
                "quarantine mark copied"
            );
        }
        #[cfg(windows)]
        {
            // A downloaded document keeps its Mark of the Web in the copy.
            let doc = dir.join("downloaded.csv");
            std::fs::write(&doc, "a,b").unwrap();
            let mark = "[ZoneTransfer]\r\nZoneId=3\r\n";
            let mut stream = doc.as_os_str().to_owned();
            stream.push(":Zone.Identifier");
            std::fs::write(&stream, mark).unwrap();
            let (real, id) = checked(&doc);
            let copy = private_copy(&real, id, &copies).unwrap();
            let mut copied = copy.as_os_str().to_owned();
            copied.push(":Zone.Identifier");
            assert_eq!(
                std::fs::read_to_string(&copied).unwrap(),
                mark,
                "Zone.Identifier copied"
            );
        }
        assert_eq!(
            file_path("file:///tmp/a%20b%27c").unwrap(),
            std::path::PathBuf::from("/tmp/a b'c")
        );
        #[cfg(windows)]
        assert_eq!(
            file_path("file:///C:/Users/a%20b/x.txt").unwrap(),
            std::path::PathBuf::from("C:/Users/a b/x.txt")
        );
        assert!(
            file_path("file:///tmp/a%0Ab").is_none(),
            "control characters"
        );
        assert!(file_path("file:////host/share").is_none(), "UNC");
        assert!(
            file_path("file:///%2F%2Fhost/share").is_none(),
            "encoded UNC"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_opener_gets_the_url_as_one_argument_without_a_shell() {
        let url = "https://a.b/?q=$(touch /tmp/x);`id`&r=1";
        let command = opener(url);
        let program = command.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if cfg!(target_os = "macos") {
            assert_eq!(
                (program.as_str(), args),
                ("/usr/bin/open", vec![url.to_owned()])
            );
        } else if cfg!(windows) {
            assert_eq!(
                (program.as_str(), args),
                (
                    "rundll32.exe",
                    vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()]
                )
            );
        } else {
            assert_eq!((program.as_str(), args), ("xdg-open", vec![url.to_owned()]));
        }
        // Refused links get no command at all.
        assert!(plan("javascript:alert(1)").is_err());
    }
}
