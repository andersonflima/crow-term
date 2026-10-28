//! Native, event-driven software frontend. PTYs live in the mux server, not this window.
//! The two RPC workers keep long polls and command I/O off the native event thread.
use crate::config::Config;
use crate::crow::{self, CrowView, Segment, Tone};
use crate::ipc::rpc;
use crate::keyboard::{self, KeyAction, KeyInput};
use crate::model::*;
use anyhow::{Context as _, Result};
use cosmic_text::{
    Attrs, Buffer, Color, Family, FeatureTag, FontFeatures, FontSystem, Metrics, Shaping, Style,
    SwashCache, Weight, Wrap,
};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, KeyLocation, ModifiersState, NamedKey, PhysicalKey};
#[cfg(target_os = "macos")]
use winit::platform::macos::{OptionAsAlt, WindowAttributesExtMacOS};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Theme, Window, WindowId};

const REGULAR: &[u8] = include_bytes!("../assets/fonts/CrowquillMono-Regular.ttf");
const BOLD: &[u8] = include_bytes!("../assets/fonts/CrowquillMono-Bold.ttf");
const ITALIC: &[u8] = include_bytes!("../assets/fonts/CrowquillMono-Italic.ttf");
const BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/CrowquillMono-BoldItalic.ttf");
const HEADER: f32 = 48.0;
const FOOTER: f32 = 25.0;
const PADDING: f32 = 10.0;
/// Crow panel: logical width when docked right, logical line height.
const PANEL_WIDTH: f32 = 380.0;
const PANEL_LINE: f32 = 18.0;
/// Rows of the bottom-docked panel (capped at 2/5 of the window height).
const PANEL_ROWS: f32 = 12.0;

#[derive(Debug)]
enum UiEvent {
    ViewReady,
    Reply {
        response: Response,
        activate_created: bool,
    },
    Error(String),
    /// Link to the daemon: a status for the footer (`None` clears a link note);
    /// `expires` clears it after a moment (the "reconnected" note).
    Link {
        status: Option<String>,
        expires: bool,
    },
    /// The daemon was restarted: the GUI now shows this window and pane.
    Reattached {
        window: u64,
        pane: u64,
    },
    /// Crow agent status, published only when its rendered content changed.
    Crow(Box<CrowView>),
}

/// At most one snapshot notification is queued, even if painting falls behind output.
#[derive(Default)]
struct ViewMailbox {
    latest: Mutex<Option<(u64, View)>>,
    pending: AtomicBool,
}
impl ViewMailbox {
    fn push(&self, window: u64, mut view: View) -> bool {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if view.screens.is_empty()
            && let Some((old_window, old)) = latest.as_mut()
            && *old_window == window
            && old.revision == view.revision
        {
            view.screens = std::mem::take(&mut old.screens);
        }
        *latest = Some((window, view));
        !self.pending.swap(true, Ordering::AcqRel)
    }
    fn take(&self) -> Option<(u64, View)> {
        self.pending.store(false, Ordering::Release);
        self.latest
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

struct Command {
    request: Request,
    activate_created: bool,
}

// A paste is one Command in the queue. Drain all its chunks here before receiving
// another command: no key, resize, or second paste can interleave bracket markers.
const INPUT_CHUNK_BYTES: usize = 16 * 1024;
fn execute_request(
    request: &Request,
    mut send: impl FnMut(&Request) -> Result<Response>,
) -> Result<Response> {
    let Request::Input { pane, bytes } = request else {
        return send(request);
    };
    let mut sent = 0;
    for chunk in bytes.chunks(INPUT_CHUNK_BYTES) {
        let part = Request::Input {
            pane: *pane,
            bytes: chunk.to_vec(),
        };
        match send(&part) {
            Ok(Response::Ok) => sent += chunk.len(),
            Ok(Response::Error(message)) => anyhow::bail!("Input interrupted after {sent}/{} bytes: {message}", bytes.len()),
            Ok(_) => anyhow::bail!("Unexpected input reply after {sent}/{} bytes", bytes.len()),
            Err(error) => return Err(error).with_context(|| format!("Input interrupted after {sent}/{} acknowledged bytes; the last chunk may have reached the PTY", bytes.len())),
        }
    }
    Ok(Response::Ok)
}

/// One request, retried while the link is only slow: a command never fails
/// because the daemon was busy for a moment. Requests with effects (input,
/// split, new window, close…) are retried only when they certainly did not
/// reach the daemon, so nothing is doubled; idempotent ones on any slowness.
fn rpc_patiently(request: &Request, running: &AtomicBool) -> Result<Response> {
    let idempotent = matches!(
        request,
        Request::Resize { .. } | Request::List | Request::PaneCwd { .. }
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut attempt = 0u32;
    loop {
        match rpc(request) {
            Err(error)
                if !crate::link::refused(&error)
                    && (idempotent || crate::link::not_sent(&error))
                    && Instant::now() < deadline
                    && running.load(Ordering::Relaxed) =>
            {
                attempt += 1;
                thread::sleep(Duration::from_millis((50 * u64::from(attempt)).min(500)));
            }
            other => return other,
        }
    }
}

/// Start a new daemon after the previous one stopped and recreate the session
/// in it (the old daemon's programs ended with it).
fn restart_daemon(
    config: Option<&std::path::Path>,
    session: &str,
    shell: &ShellSpec,
) -> Result<(u64, u64)> {
    crate::ipc::restart_daemon(config)?;
    recreate_session(session, shell)
}

/// Create (or find) the session in the running daemon.
fn recreate_session(session: &str, shell: &ShellSpec) -> Result<(u64, u64)> {
    match rpc(&Request::Ensure {
        session: session.to_owned(),
        shell: shell.clone(),
        size: Size::default(),
    })? {
        Response::Created { window, pane } => Ok((window, pane)),
        other => anyhow::bail!("unexpected Ensure reply {other:?}"),
    }
}

/// Attach a native GUI to a running persistent mux server. Closing the GUI only detaches.
pub fn run(
    config: Config,
    config_path: Option<std::path::PathBuf>,
    session: String,
    initial_window: u64,
    initial_pane: u64,
    shell: ShellSpec,
) -> Result<()> {
    let event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let running = Arc::new(AtomicBool::new(true));
    let active_window = Arc::new(AtomicU64::new(initial_window));
    let (tx, rx) = mpsc::channel::<Command>();
    let command_proxy = proxy.clone();
    let crow_proxy = proxy.clone();
    let command_running = running.clone();
    let command_worker = thread::Builder::new()
        .name("crow-ui-commands".into())
        .spawn(move || {
            while command_running.load(Ordering::Relaxed) {
                let command = match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                let event = match execute_request(&command.request, |request| {
                    rpc_patiently(request, &command_running)
                }) {
                    Ok(response) => UiEvent::Reply {
                        response,
                        activate_created: command.activate_created,
                    },
                    Err(error) => UiEvent::Error(format!("Mux command: {error:#}")),
                };
                if command_proxy.send_event(event).is_err() {
                    break;
                }
            }
        })?;
    let mailbox = Arc::new(ViewMailbox::default());
    let watch_mailbox = mailbox.clone();
    let watch_running = running.clone();
    let watch_window = active_window.clone();
    let watch_session = session.clone();
    let watch_shell = shell.clone();
    let watch_worker = thread::Builder::new()
        .name("crow-ui-watch".into())
        .spawn(move || {
            let mut since = 0;
            let mut previous_window = initial_window;
            // A slow reply is never a disconnect; only a daemon that stopped
            // accepting connections is reported, restarted and reattached.
            let mut link = crate::link::Link::default();
            // Set by any failed exchange: a session missing from the daemon's
            // reply afterwards means the daemon was restarted (possibly by
            // another GUI), not that the user closed it.
            let mut recovering = false;
            while watch_running.load(Ordering::Relaxed) {
                let cycle_started = Instant::now();
                let window = watch_window.load(Ordering::Relaxed);
                if window != previous_window {
                    since = 0;
                    previous_window = window;
                }
                match rpc(&Request::Watch {
                    session: watch_session.clone(),
                    window,
                    since,
                }) {
                    Ok(Response::View(view))
                        if recovering && !view.sessions.iter().any(|s| s.name == watch_session) =>
                    {
                        match recreate_session(&watch_session, &watch_shell) {
                            Ok((window, pane)) => {
                                recovering = false;
                                link = crate::link::Link::default();
                                watch_window.store(window, Ordering::Relaxed);
                                previous_window = window;
                                since = 0;
                                if proxy
                                    .send_event(UiEvent::Reattached { window, pane })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(error) => {
                                let _ = proxy.send_event(UiEvent::Link {
                                    status: Some(format!(
                                        "Session could not be recreated ({error:#}); retrying"
                                    )),
                                    expires: false,
                                });
                                thread::sleep(Duration::from_secs(1));
                            }
                        }
                        continue;
                    }
                    Ok(Response::View(view)) => {
                        recovering = false;
                        if let Some(note) = link.ok()
                            && proxy
                                .send_event(UiEvent::Link {
                                    status: Some(note.into()),
                                    expires: true,
                                })
                                .is_err()
                        {
                            break;
                        }
                        since = view.revision;
                        if watch_mailbox.push(window, view)
                            && proxy.send_event(UiEvent::ViewReady).is_err()
                        {
                            break;
                        }
                    }
                    Ok(Response::Error(error)) => {
                        let _ = proxy.send_event(UiEvent::Error(error));
                        break;
                    }
                    Ok(_) => {
                        let _ = proxy
                            .send_event(UiEvent::Error("Unexpected mux Watch response".into()));
                        break;
                    }
                    // The daemon answered with an error (e.g. the session ended).
                    // The daemon answered with an error: show it and keep watching.
                    Err(error) if crate::link::refused(&error) => {
                        let _ = proxy.send_event(UiEvent::Error(format!("{error:#}")));
                        thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    Err(error) => match link.failed(crate::link::classify(&error)) {
                        crate::link::Action::Retry { after, status } => {
                            recovering = true;
                            if let Some(status) = status {
                                eprintln!("crow-term: daemon slow to answer: {error:#}");
                                let _ = proxy.send_event(UiEvent::Link {
                                    status: Some(status),
                                    expires: false,
                                });
                            }
                            thread::sleep(after);
                            continue;
                        }
                        crate::link::Action::Restart { .. } if crate::ipc::daemon_alive() => {
                            // Refusing connections while its process lives (full
                            // backlog, paused): busy, never "stopped".
                            recovering = true;
                            let _ = proxy.send_event(UiEvent::Link {
                                status: Some(crate::link::BUSY.into()),
                                expires: false,
                            });
                            thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                        crate::link::Action::Restart { status } => {
                            recovering = true;
                            let _ = proxy.send_event(UiEvent::Link {
                                status: Some(status),
                                expires: false,
                            });
                            match restart_daemon(
                                config_path.as_deref(),
                                &watch_session,
                                &watch_shell,
                            ) {
                                Ok((window, pane)) => {
                                    // The restart note stands; no "reconnected" over it.
                                    link = crate::link::Link::default();
                                    watch_window.store(window, Ordering::Relaxed);
                                    previous_window = window;
                                    since = 0;
                                    if proxy
                                        .send_event(UiEvent::Reattached { window, pane })
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                Err(error) => match error.downcast_ref::<crate::ipc::NoRestart>() {
                                    // Alive but not accepting right now: it is busy, not gone.
                                    Some(crate::ipc::NoRestart::StillAlive) => {
                                        let _ = proxy.send_event(UiEvent::Link {
                                            status: Some(crate::link::BUSY.into()),
                                            expires: false,
                                        });
                                        thread::sleep(Duration::from_millis(500));
                                    }
                                    Some(crate::ipc::NoRestart::StoppedOnPurpose) => {
                                        let _ = proxy.send_event(UiEvent::Link {
                                            status: Some(
                                                "The daemon was stopped (crow-term stop): its sessions ended. Run crow-term to start again."
                                                    .into(),
                                            ),
                                            expires: false,
                                        });
                                        break;
                                    }
                                    None => {
                                        let _ = proxy.send_event(UiEvent::Link {
                                            status: Some(format!(
                                                "Daemon could not be restarted ({error:#}); retrying"
                                            )),
                                            expires: false,
                                        });
                                        thread::sleep(Duration::from_secs(2));
                                    }
                                },
                            }
                            continue;
                        }
                    },
                }
                // Bound snapshots during sustained PTY output; idle Watch already waits 1s.
                if let Some(remaining) =
                    Duration::from_millis(16).checked_sub(cycle_started.elapsed())
                {
                    thread::sleep(remaining);
                }
            }
        })?;
    // Read-only crow agent status: its own thread, at most one subprocess in flight.
    let crow_shared = if config.crow.enabled {
        match crow::spawn(
            &config.crow,
            running.clone(),
            initial_pane,
            |pane| match rpc(&Request::PaneCwd { pane }) {
                Ok(Response::Cwd(cwd)) => Ok(cwd),
                // rpc() already maps Response::Error (e.g. a closed pane) to Err.
                _ => Err(()),
            },
            move |view| crow_proxy.send_event(UiEvent::Crow(Box::new(view))).is_ok(),
        ) {
            // The worker is never joined: a snapshot in flight finishes or times out
            // on its own and must not delay closing (detaching) the GUI.
            Ok((shared, _detached)) => Some(shared),
            Err(error) => {
                eprintln!("crow-term: crow status disabled: {error}");
                None
            }
        }
    } else {
        None
    };
    let mut app = App::new(
        config,
        session,
        initial_window,
        initial_pane,
        shell,
        tx,
        active_window,
        running.clone(),
        mailbox,
        crow_shared.clone(),
    );
    let result = event_loop.run_app(&mut app);
    let fatal_error = app.fatal_error.take();
    running.store(false, Ordering::Relaxed);
    if let Some(shared) = &crow_shared {
        shared.wake();
    }
    // Closing the event loop never sends ClosePane/KillSession to the mux.
    drop(app);
    let _ = command_worker.join();
    let _ = watch_worker.join();
    result.context("native event loop")?;
    if let Some(error) = fatal_error {
        anyhow::bail!(error);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}
impl Rect {
    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < self.x as f64 + self.w as f64
            && y < self.y as f64 + self.h as f64
    }
    fn intersection(self, other: Self) -> Self {
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
}
/// Vertical separators produce side-by-side panes; horizontal separators stack them.
fn pane_rects(layout: &Layout, rect: Rect, gap: u32, out: &mut Vec<(u64, Rect)>) {
    match layout {
        Layout::Pane(id) => out.push((*id, rect)),
        Layout::Split {
            axis,
            first,
            second,
        } => {
            let mut a = rect;
            let mut b = rect;
            match axis {
                Axis::Vertical => {
                    let gap = gap.min(rect.w);
                    a.w = (rect.w - gap) / 2;
                    b.x += (a.w + gap) as i32;
                    b.w = rect.w - gap - a.w;
                }
                Axis::Horizontal => {
                    let gap = gap.min(rect.h);
                    a.h = (rect.h - gap) / 2;
                    b.y += (a.h + gap) as i32;
                    b.h = rect.h - gap - a.h;
                }
            }
            pane_rects(first, a, gap, out);
            pane_rects(second, b, gap, out);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    row: usize,
    col: usize,
}
#[derive(Debug, Clone)]
struct Selection {
    pane: u64,
    start: Point,
    end: Point,
}
impl Selection {
    fn ordered(&self) -> (Point, Point) {
        if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
    fn contains(&self, row: usize, col: usize) -> bool {
        let (start, end) = self.ordered();
        let point = Point { row, col };
        point >= start && point <= end
    }
}
struct CachedRow {
    cells: Vec<Cell>,
    pixels: Vec<u32>,
    width: usize,
}
/// Families that hold the Nerd Font icons (Private Use Area), in preference order.
const SYMBOL_FAMILIES: [&str; 2] = ["Symbols Nerd Font Mono", "Symbols Nerd Font"];
/// Private Use Area code points: icon fonts live here, and a system CJK font's
/// vendor glyphs at the same code points are never the intended icon.
fn private_use(text: &str) -> bool {
    text.chars().next().is_some_and(|c| {
        matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
    })
}
struct Renderer {
    fonts: FontSystem,
    /// Installed Nerd symbols family for Private Use Area cells that the
    /// configured family cannot draw itself.
    symbols: Option<&'static str>,
    /// Whether the configured family maps a Private Use Area code point.
    primary_covers: HashMap<char, bool>,
    swash: SwashCache,
    rows: HashMap<(u64, usize), CachedRow>,
    font_size: f32,
    cell_width: u32,
    cell_height: u32,
    baseline: f32,
}
impl Renderer {
    fn new(config: &Config, scale: f64) -> Self {
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
        let mut renderer = Self {
            fonts,
            symbols,
            primary_covers: HashMap::new(),
            swash: SwashCache::new(),
            rows: HashMap::new(),
            font_size: 18.0,
            cell_width: 11,
            cell_height: 23,
            baseline: 18.0,
        };
        renderer.rescale(config, scale);
        renderer
    }
    fn rescale(&mut self, config: &Config, scale: f64) {
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
        self.rows.clear();
        self.swash = SwashCache::new();
    }
    /// An icon cell drawn from the symbols family: a Private Use Area code point
    /// the configured family does not map (a patched Nerd font keeps its own icons).
    fn symbol_cell(&mut self, config: &Config, cell: &Cell) -> bool {
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
    fn row(&mut self, config: &Config, pane: u64, row: usize, cells: &[Cell]) -> &CachedRow {
        let key = (pane, row);
        let needs_update = self
            .rows
            .get(&key)
            .is_none_or(|cached| cached.cells != cells);
        if needs_update {
            let width = cells.len() * self.cell_width as usize;
            let height = self.cell_height as usize;
            let mut pixels = vec![parse_color(&config.background, 0x111111); width * height];
            for (col, cell) in cells.iter().enumerate() {
                let color = resolve_color(config, cell.bg);
                for y in 0..height {
                    let start = y * width + col * self.cell_width as usize;
                    pixels[start..start + self.cell_width as usize].fill(color);
                }
            }
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
                    // Anchor every shaped cluster to its terminal cell. Combining glyph offsets
                    // and multi-cell ligatures survive, while fallback advances cannot shift the grid.
                    glyph.x = col as f32 * self.cell_width as f32 + glyph.x - original_x;
                    let physical = glyph.physical((0.0, self.baseline), 1.0);
                    let color = glyph
                        .color_opt
                        .unwrap_or(cosmic_color(parse_color(&config.foreground, 0xeaeaea)));
                    self.swash.with_pixels(
                        &mut self.fonts,
                        physical.cache_key,
                        color,
                        |x, y, color| {
                            let x = x + physical.x;
                            let y = y + physical.y;
                            if x >= 0 && y >= 0 && (x as usize) < width && (y as usize) < height {
                                blend(&mut pixels[y as usize * width + x as usize], color);
                            }
                        },
                    );
                }
            }
            let cell_width = self.cell_width as usize;
            for (col, cell) in cells.iter().enumerate() {
                let color = resolve_color(config, cell.fg);
                if cell.strike && width > 0 {
                    let start = height / 2 * width + col * cell_width;
                    pixels[start..start + cell_width].fill(color);
                }
                let (style, color) = underline_paint(config, cell);
                draw_underline(
                    &mut pixels,
                    width,
                    height,
                    col * cell_width..(col + 1) * cell_width,
                    style,
                    color,
                );
            }
            self.rows.insert(
                key,
                CachedRow {
                    cells: cells.to_vec(),
                    pixels,
                    width,
                },
            );
        }
        &self.rows[&key]
    }
    // Raster destination, clipping and styling are deliberately explicit at this boundary.
    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        frame: &mut [u32],
        frame_width: usize,
        clip: Rect,
        x: i32,
        y: i32,
        text: &str,
        color: u32,
        config: &Config,
        size: f32,
    ) -> f32 {
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
        for run in buffer.layout_runs() {
            width = width.max(run.line_w);
            for glyph in run.glyphs {
                let physical = glyph.physical((x as f32, y as f32 + run.line_y), 1.0);
                self.swash.with_pixels(
                    &mut self.fonts,
                    physical.cache_key,
                    cosmic_color(color),
                    |gx, gy, color| {
                        let px = gx + physical.x;
                        let py = gy + physical.y;
                        if clip.contains(px as f64, py as f64)
                            && let Some(pixel) =
                                frame.get_mut(py as usize * frame_width + px as usize)
                        {
                            blend(pixel, color);
                        }
                    },
                );
            }
        }
        width
    }
}

/// Underline style and color of a cell: SGR 58 color if set, else the
/// foreground; snapshots that only set the boolean draw a single line.
fn underline_paint(config: &Config, cell: &Cell) -> (UnderlineStyle, u32) {
    let style = match cell.underline_style {
        UnderlineStyle::None if cell.underline => UnderlineStyle::Single,
        style => style,
    };
    let color = resolve_color(config, cell.underline_color.unwrap_or(cell.fg));
    (style, color)
}

/// Rasterize one cell's underline into a row buffer. Pure and bounds-checked,
/// so a later GPU renderer can share the geometry. Positions use row-absolute
/// x, which keeps curly and dashed patterns continuous across cells.
fn draw_underline(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    columns: std::ops::Range<usize>,
    style: UnderlineStyle,
    color: u32,
) {
    if style == UnderlineStyle::None || width == 0 || height < 2 {
        return;
    }
    let thickness = (height / 20).max(1);
    let bottom = height - 1;
    let mut plot = |x: usize, y: usize| {
        if x < width && y < height {
            pixels[y * width + x] = color;
        }
    };
    let line = |plot: &mut dyn FnMut(usize, usize), x: usize, top: usize| {
        for dy in 0..thickness {
            plot(x, top + dy);
        }
    };
    let single_top = height.saturating_sub(1 + thickness);
    for x in columns.start..columns.end.min(width) {
        match style {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => line(&mut plot, x, single_top),
            UnderlineStyle::Double => {
                line(&mut plot, x, bottom + 1 - thickness);
                line(&mut plot, x, bottom.saturating_sub(3 * thickness) + 1);
            }
            UnderlineStyle::Dotted => {
                if (x / (thickness * 2).max(2)).is_multiple_of(2) {
                    line(&mut plot, x, single_top);
                }
            }
            UnderlineStyle::Dashed => {
                let dash = (columns.len() / 2).max(3);
                if (x / dash).is_multiple_of(2) {
                    line(&mut plot, x, single_top);
                }
            }
            UnderlineStyle::Curly => {
                let amplitude = (height / 8).clamp(1, 12) as f32;
                let period = (columns.len().max(4)) as f32;
                let phase = (x as f32 / period) * std::f32::consts::TAU;
                let offset = (amplitude * (1.0 - phase.cos()) / 2.0).round() as usize;
                line(
                    &mut plot,
                    x,
                    (bottom + 1).saturating_sub(thickness + offset),
                );
            }
        }
    }
}

fn same_text_style(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg && a.bold == b.bold && a.italic == b.italic
}
fn row_text(cells: &[Cell]) -> (String, Vec<(usize, usize)>) {
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
fn column_for_byte(map: &[(usize, usize)], byte: usize) -> usize {
    map.iter()
        .rev()
        .find(|(start, _)| *start <= byte)
        .map_or(0, |(_, col)| *col)
}
fn parse_color(value: &str, fallback: u32) -> u32 {
    value
        .strip_prefix('#')
        .filter(|s| s.len() == 6)
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .unwrap_or(fallback)
}
fn mix_rgb(a: u32, b: u32, amount: u32) -> u32 {
    let channel = |shift: u32| -> u32 {
        let a = (a >> shift) & 255;
        let b = (b >> shift) & 255;
        (a * (100 - amount) + b * amount) / 100
    };
    channel(16) << 16 | channel(8) << 8 | channel(0)
}
fn light_background(config: &Config) -> bool {
    let bg = parse_color(&config.background, 0xffffff);
    ((bg >> 16) & 255) * 299 + ((bg >> 8) & 255) * 587 + (bg & 255) * 114 > 128_000
}
struct Chrome {
    surface: u32,
    active: u32,
    border: u32,
    text: u32,
    muted: u32,
}
impl Chrome {
    fn new(config: &Config) -> Self {
        let bg = parse_color(&config.background, 0xffffff);
        let fg = parse_color(&config.foreground, 0x444444);
        Self {
            surface: mix_rgb(bg, fg, 3),
            active: mix_rgb(bg, fg, 8),
            border: mix_rgb(bg, fg, 22),
            text: fg,
            muted: mix_rgb(bg, fg, 72),
        }
    }
}
/// Semantic crow colours. Crowquill palettes are greyscale, so state colours are
/// fixed per light/dark background instead of being taken from the ANSI palette.
fn tone_color(tone: Tone, chrome: &Chrome, light: bool) -> u32 {
    match (tone, light) {
        (Tone::Plain, _) => chrome.text,
        (Tone::Muted, _) => chrome.muted,
        (Tone::On, true) => 0x1e7f3c,
        (Tone::On, false) => 0x6fcf8a,
        (Tone::Off, true) => 0xb4532a,
        (Tone::Off, false) => 0xe39264,
        (Tone::Accent, true) => 0x2f5fb3,
        (Tone::Accent, false) => 0x7aa7f0,
        (Tone::Warn, true) => 0xa86400,
        (Tone::Warn, false) => 0xe6b450,
        (Tone::Alert, true) => 0xc0392b,
        (Tone::Alert, false) => 0xf07167,
    }
}
/// Rasterised crow panel, rebuilt only when its lines, size or scale change.
struct PanelLayer {
    key: u64,
    width: usize,
    pixels: Vec<u32>,
}
fn cosmic_color(value: u32) -> Color {
    Color::rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}
fn resolve_color(config: &Config, color: ColorSpec) -> u32 {
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
fn blend(target: &mut u32, color: Color) {
    let alpha = color.a() as u32;
    if alpha == 0 {
        return;
    }
    let mix = |old: u32, new: u32| (new * alpha + old * (255 - alpha) + 127) / 255;
    *target = mix((*target >> 16) & 255, color.r() as u32) << 16
        | mix((*target >> 8) & 255, color.g() as u32) << 8
        | mix(*target & 255, color.b() as u32);
}
fn fill(frame: &mut [u32], width: usize, rect: Rect, color: u32) {
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
fn blit(
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

/// Paint the crow agent list into its own buffer (cached by the caller).
fn render_panel(
    renderer: &mut Renderer,
    config: &Config,
    lines: &[Vec<Segment>],
    panel: Rect,
    scale: f64,
    chrome: &Chrome,
    light: bool,
) -> Vec<u32> {
    let (w, h) = (panel.w as usize, panel.h as usize);
    let mut pixels = vec![chrome.surface; w * h];
    let local = Rect {
        x: 0,
        y: 0,
        w: panel.w,
        h: panel.h,
    };
    // Divider on the edge facing the panes.
    let divider = if config.crow.panel_position == "bottom" {
        Rect { h: 1, ..local }
    } else {
        Rect { w: 1, ..local }
    };
    fill(&mut pixels, w, divider, chrome.border);
    let pad = (PADDING * 1.4 * scale as f32) as i32;
    let line = (PANEL_LINE * scale as f32) as i32;
    let size = (12.0 * scale) as f32;
    let gap = (8.0 * scale) as i32;
    let mut y = pad;
    let capacity = ((h as i32 - pad - pad / 2) / line.max(1)).max(0) as usize;
    for segments in &crow::fit_lines(lines, capacity) {
        let mut x = pad;
        for segment in segments {
            let width = renderer.text(
                &mut pixels,
                w,
                local,
                x,
                y,
                &segment.text,
                tone_color(segment.tone, chrome, light),
                config,
                size,
            );
            x += width.ceil() as i32 + gap;
        }
        y += line;
    }
    pixels
}

struct App {
    config: Config,
    session: String,
    shell: ShellSpec,
    window_id: u64,
    pane_id: u64,
    shared_window: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    commands: mpsc::Sender<Command>,
    mailbox: Arc<ViewMailbox>,
    fatal_error: Option<String>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    renderer: Renderer,
    view: Option<View>,
    scale: f64,
    mods: ModifiersState,
    prefix: bool,
    status: String,
    cursor: PhysicalPosition<f64>,
    selection: Option<Selection>,
    dragging: bool,
    mouse_button: Option<MouseButton>,
    mouse_origin: Option<u64>,
    wheel_remainder: f64,
    ime: String,
    sent_sizes: HashMap<u64, Size>,
    /// Keys whose press reached the PTY, so only their release is reported.
    pressed_keys: HashSet<PhysicalKey>,
    /// Window focus, for DEC 1004 focus reports on pane switches.
    focused: bool,
    /// Pane that becomes active before its screen arrives (window switch).
    pending_focus_in: Option<u64>,
    /// Last seen bell count / OSC 52 copy sequence per pane.
    bells: HashMap<u64, u64>,
    clipboard_seqs: HashMap<u64, u64>,
    /// Visual bell: panes flashing until the deadline.
    bell_flash: Option<(Instant, HashSet<u64>)>,
    window_title: String,
    /// Clear `status` at this instant (transient link notes).
    status_expires: Option<Instant>,
    clipboard: Option<arboard::Clipboard>,
    crow: Option<Arc<crow::Shared>>,
    crow_view: CrowView,
    crow_panel: bool,
    crow_pane: u64,
    panel_layer: Option<PanelLayer>,
    #[cfg(feature = "frame-dump")]
    frame_dump: Option<crate::frame_dump::FrameDump>,
}
impl App {
    // Construction joins independent model/worker handles exactly once, not a hot-path API.
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: Config,
        session: String,
        window_id: u64,
        pane_id: u64,
        shell: ShellSpec,
        commands: mpsc::Sender<Command>,
        shared_window: Arc<AtomicU64>,
        running: Arc<AtomicBool>,
        mailbox: Arc<ViewMailbox>,
        crow: Option<Arc<crow::Shared>>,
    ) -> Self {
        let renderer = Renderer::new(&config, 1.0);
        let crow_panel = crow.is_some() && config.crow.panel;
        Self {
            config,
            session,
            shell,
            window_id,
            pane_id,
            shared_window,
            running,
            commands,
            mailbox,
            fatal_error: None,
            window: None,
            surface: None,
            renderer,
            view: None,
            scale: 1.0,
            mods: ModifiersState::empty(),
            prefix: false,
            status: String::new(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            selection: None,
            dragging: false,
            mouse_button: None,
            mouse_origin: None,
            wheel_remainder: 0.0,
            ime: String::new(),
            sent_sizes: HashMap::new(),
            pressed_keys: HashSet::new(),
            focused: true,
            pending_focus_in: None,
            bells: HashMap::new(),
            clipboard_seqs: HashMap::new(),
            bell_flash: None,
            window_title: String::new(),
            status_expires: None,
            clipboard: None,
            crow,
            crow_view: CrowView::default(),
            crow_panel,
            crow_pane: pane_id,
            panel_layer: None,
            #[cfg(feature = "frame-dump")]
            frame_dump: crate::frame_dump::FrameDump::from_env(),
        }
    }
    /// Crow panel area, or `None` when hidden or the window is too small for it.
    fn panel_rect(&self) -> Option<Rect> {
        if !self.crow_panel {
            return None;
        }
        let size = self.window.as_ref()?.inner_size();
        let scale = self.scale as f32;
        let top = (HEADER * scale).round() as u32;
        let footer = (FOOTER * scale).round() as u32;
        let available = size.height.saturating_sub(top + footer);
        if self.config.crow.panel_position == "bottom" {
            // Fixed height: content changes must never resize (SIGWINCH) the panes.
            let h = ((PANEL_ROWS * PANEL_LINE + 2.0 * PADDING) * scale) as u32;
            let h = h.min(available * 2 / 5);
            if available.saturating_sub(h) < (120.0 * scale) as u32 {
                return None;
            }
            Some(Rect {
                x: 0,
                y: (top + available - h) as i32,
                w: size.width,
                h,
            })
        } else {
            let w = ((PANEL_WIDTH * scale) as u32).min(size.width * 9 / 20);
            if size.width.saturating_sub(w) < (300.0 * scale) as u32 {
                return None;
            }
            Some(Rect {
                x: (size.width - w) as i32,
                y: top as i32,
                w,
                h: available,
            })
        }
    }
    fn toggle_crow_panel(&mut self) {
        let Some(shared) = &self.crow else {
            self.status = "crow status is disabled (crow.enabled = false)".into();
            return;
        };
        self.crow_panel = !self.crow_panel;
        shared.panel.store(self.crow_panel, Ordering::Relaxed);
        shared.wake();
        self.panel_layer = None;
        self.renderer.rows.clear();
        self.resize_panes();
        self.redraw();
    }
    fn send(&self, request: Request, activate_created: bool) {
        let _ = self.commands.send(Command {
            request,
            activate_created,
        });
    }
    fn input(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.selection = None;
            self.send(
                Request::Input {
                    pane: self.pane_id,
                    bytes,
                },
                false,
            );
        }
    }
    fn redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
    fn current_window(&self) -> Option<&WindowInfo> {
        self.view
            .as_ref()?
            .sessions
            .iter()
            .find(|s| s.name == self.session)?
            .windows
            .iter()
            .find(|w| w.id == self.window_id)
    }
    fn windows(&self) -> Vec<WindowInfo> {
        self.view
            .as_ref()
            .and_then(|v| v.sessions.iter().find(|s| s.name == self.session))
            .map(|s| s.windows.clone())
            .unwrap_or_default()
    }
    fn screen(&self, id: u64) -> Option<&Screen> {
        self.view
            .as_ref()?
            .screens
            .iter()
            .find(|(pane, _)| *pane == id)
            .map(|(_, screen)| screen)
    }
    fn modes(&self) -> Modes {
        self.screen(self.pane_id)
            .map(|s| s.modes.clone())
            .unwrap_or_default()
    }
    fn rects(&self) -> Vec<(u64, Rect)> {
        let Some(window) = &self.window else {
            return Vec::new();
        };
        let size = window.inner_size();
        let padding = (PADDING * self.scale as f32).round() as u32;
        let top = (HEADER * self.scale as f32).round() as u32;
        let footer = (FOOTER * self.scale as f32).round() as u32;
        let mut rect = Rect {
            x: padding as i32,
            y: (top + padding) as i32,
            w: size.width.saturating_sub(2 * padding),
            h: size.height.saturating_sub(top + footer + 2 * padding),
        };
        if let Some(panel) = self.panel_rect() {
            if self.config.crow.panel_position == "bottom" {
                rect.h = rect.h.saturating_sub(panel.h);
            } else {
                rect.w = rect.w.saturating_sub(panel.w);
            }
        }
        let mut out = Vec::new();
        if let Some(info) = self.current_window() {
            pane_rects(&info.layout, rect, self.scale.ceil() as u32, &mut out);
        } else {
            out.push((self.pane_id, rect));
        }
        out
    }
    fn default_size(&self) -> Size {
        self.rects()
            .iter()
            .find(|(id, _)| *id == self.pane_id)
            .map(|(_, rect)| self.rect_size(*rect))
            .unwrap_or_default()
    }
    fn rect_size(&self, rect: Rect) -> Size {
        Size {
            cols: (rect.w / self.renderer.cell_width).clamp(2, 400) as u16,
            rows: (rect.h / self.renderer.cell_height)
                .clamp(1, 200)
                .min(40_000 / (rect.w / self.renderer.cell_width).clamp(2, 400))
                as u16,
        }
    }
    fn resize_panes(&mut self) {
        for (pane, rect) in self.rects() {
            let size = self.rect_size(rect);
            if self.sent_sizes.get(&pane) != Some(&size) {
                self.sent_sizes.insert(pane, size);
                let cell = CellPixels {
                    width: self.renderer.cell_width.min(u32::from(u16::MAX)) as u16,
                    height: self.renderer.cell_height.min(u32::from(u16::MAX)) as u16,
                };
                self.send(
                    Request::Resize {
                        pane,
                        size,
                        cell: Some(cell),
                    },
                    false,
                );
            }
        }
        self.update_ime_position();
    }
    fn update_ime_position(&self) {
        if let Some(window) = &self.window
            && let Some((_, rect)) = self.rects().into_iter().find(|(id, _)| *id == self.pane_id)
            && let Some(screen) = self.screen(self.pane_id)
        {
            window.set_ime_cursor_area(
                PhysicalPosition::new(
                    rect.x + (screen.cursor.col as u32 * self.renderer.cell_width) as i32,
                    rect.y + (screen.cursor.row as u32 * self.renderer.cell_height) as i32,
                ),
                PhysicalSize::new(self.renderer.cell_width, self.renderer.cell_height),
            );
        }
    }
    /// Make `pane` the input target, reporting DEC 1004 focus out/in to the
    /// panes that requested focus events.
    fn focus_pane(&mut self, pane: u64) {
        if pane == self.pane_id {
            return;
        }
        let old = self.pane_id;
        self.pane_id = pane;
        self.pending_focus_in = None;
        if !self.focused {
            return;
        }
        if self.screen(old).is_some_and(|s| s.modes.focus_reporting) {
            self.send(
                Request::Input {
                    pane: old,
                    bytes: b"\x1b[O".to_vec(),
                },
                false,
            );
        }
        match self.screen(pane) {
            Some(screen) if screen.modes.focus_reporting => self.send(
                Request::Input {
                    pane,
                    bytes: b"\x1b[I".to_vec(),
                },
                false,
            ),
            Some(_) => {}
            None => self.pending_focus_in = Some(pane),
        }
    }
    fn activate_window(&mut self, id: u64) {
        if id == self.window_id {
            return;
        }
        self.window_id = id;
        self.shared_window.store(id, Ordering::Relaxed);
        if let Some(id) = self
            .current_window()
            .and_then(|info| info.layout.panes().first().copied())
        {
            self.focus_pane(id);
        }
        self.selection = None;
        self.renderer.rows.clear();
        self.resize_panes();
        self.redraw();
    }
    fn cycle_window(&mut self, direction: isize) {
        let windows = self.windows();
        if windows.is_empty() {
            return;
        }
        let position = windows
            .iter()
            .position(|w| w.id == self.window_id)
            .unwrap_or(0) as isize;
        self.activate_window(
            windows[(position + direction).rem_euclid(windows.len() as isize) as usize].id,
        );
    }
    fn select_pane(&mut self, direction: char) {
        let rects = self.rects();
        if rects.is_empty() {
            return;
        }
        if direction == 'o' {
            let index = rects
                .iter()
                .position(|(id, _)| *id == self.pane_id)
                .unwrap_or(0);
            self.focus_pane(rects[(index + 1) % rects.len()].0);
        } else if let Some((_, current)) = rects.iter().find(|(id, _)| *id == self.pane_id) {
            let cx = current.x as f64 + current.w as f64 / 2.0;
            let cy = current.y as f64 + current.h as f64 / 2.0;
            let mut candidates = rects
                .iter()
                .filter_map(|(id, r)| {
                    if *id == self.pane_id {
                        return None;
                    }
                    let dx = r.x as f64 + r.w as f64 / 2.0 - cx;
                    let dy = r.y as f64 + r.h as f64 / 2.0 - cy;
                    let (forward, perpendicular) = match direction {
                        'h' => (-dx, dy.abs()),
                        'l' => (dx, dy.abs()),
                        'j' => (dy, dx.abs()),
                        'k' => (-dy, dx.abs()),
                        _ => (0.0, 0.0),
                    };
                    (forward > 0.0).then_some((*id, forward + perpendicular * 3.0))
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
            if let Some(&(id, _)) = candidates.first() {
                self.focus_pane(id);
            }
        }
        self.selection = None;
        self.update_ime_position();
        self.redraw();
    }
    fn prefix_action(&mut self, key: &Key, event_loop: &ActiveEventLoop) {
        let character = match key {
            Key::Character(text) => text.as_str(),
            Key::Named(NamedKey::Tab) => "o",
            _ => "",
        };
        match character {
            "c" => self.send(
                Request::NewWindow {
                    session: self.session.clone(),
                    shell: self.shell.clone(),
                    size: self.default_size(),
                },
                true,
            ),
            "%" | "\"" => self.send(
                Request::Split {
                    session: self.session.clone(),
                    window: self.window_id,
                    pane: self.pane_id,
                    axis: if character == "%" {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    },
                    shell: self.shell.clone(),
                    size: self.default_size(),
                },
                true,
            ),
            "n" => self.cycle_window(1),
            "p" => self.cycle_window(-1),
            "h" | "j" | "k" | "l" | "o" => self.select_pane(character.chars().next().unwrap()),
            "d" => event_loop.exit(),
            "a" => self.toggle_crow_panel(),
            "x" => self.send(
                Request::ClosePane {
                    session: self.session.clone(),
                    window: self.window_id,
                    pane: self.pane_id,
                },
                false,
            ),
            s if s.len() == 1 && (b'1'..=b'9').contains(&s.as_bytes()[0]) => {
                let index = character.as_bytes()[0] as usize - b'1' as usize;
                if let Some(info) = self.windows().get(index) {
                    self.activate_window(info.id);
                }
            }
            _ => {
                self.status =
                    "Prefix: c window  % / \" split  n/p tabs  h/j/k/l pane  a agents  d detach  x close"
                        .into()
            }
        }
    }
    fn key_event(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) {
        let base = event.key_without_modifiers();
        let input = KeyInput {
            key: &event.logical_key,
            base: &base,
            text: event.text.as_deref(),
            physical: event.physical_key,
            location: event.location,
            mods: self.mods,
            action: match (event.state, event.repeat) {
                (ElementState::Released, _) => KeyAction::Release,
                (_, true) => KeyAction::Repeat,
                _ => KeyAction::Press,
            },
        };
        if input.action == KeyAction::Release {
            // Report a release only for a press the application received.
            if self.pressed_keys.remove(&event.physical_key) {
                let bytes = encode_input(&input, &self.modes());
                self.input(bytes);
            }
            return;
        }
        self.key(&input, event_loop);
    }
    fn key(&mut self, input: &KeyInput, event_loop: &ActiveEventLoop) {
        let key = input.key;
        let c = match key {
            Key::Character(s) => s.to_lowercase(),
            _ => String::new(),
        };
        let shortcut = (self.mods.control_key() && self.mods.shift_key()) || self.mods.super_key();
        if shortcut && c == "c" {
            self.copy_selection();
            return;
        }
        if shortcut && c == "v" {
            self.paste();
            return;
        }
        let is_prefix = prefix_matches(&self.config.prefix, key, self.mods);
        // Pressing a modifier alone (Shift for `%`/`"`, a layout's AltGr) is part
        // of typing the next key, not the key itself: keep the prefix armed.
        if self.prefix && is_modifier_key(key) {
            return;
        }
        if self.prefix {
            self.prefix = false;
            if is_prefix {
                let bytes = encode_input(input, &self.modes());
                if !bytes.is_empty() {
                    self.pressed_keys.insert(input.physical);
                }
                self.input(bytes);
            } else {
                self.prefix_action(key, event_loop);
            }
            self.redraw();
            return;
        }
        if is_prefix {
            self.prefix = true;
            self.redraw();
            return;
        }
        if self.mods.shift_key() && matches!(key, Key::Named(NamedKey::PageUp | NamedKey::PageDown))
        {
            let lines = if matches!(key, Key::Named(NamedKey::PageUp)) {
                10
            } else {
                -10
            };
            self.send(
                Request::Scroll {
                    pane: self.pane_id,
                    lines,
                },
                false,
            );
            return;
        }
        if self.ime.is_empty() || !matches!(key, Key::Character(_)) {
            let bytes = encode_input(input, &self.modes());
            // Only a press the application received may report its release.
            if !bytes.is_empty() {
                self.pressed_keys.insert(input.physical);
            }
            self.input(bytes);
        }
    }
    fn copy_selection(&mut self) {
        let Some(selection) = &self.selection else {
            self.status = "Drag to select text; Shift-drag bypasses application mouse mode".into();
            self.redraw();
            return;
        };
        let Some(screen) = self.screen(selection.pane) else {
            return;
        };
        let text = selected_text(screen, selection);
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        match self
            .clipboard
            .as_mut()
            .map(|clipboard| clipboard.set_text(text))
        {
            Some(Ok(())) => self.status = "Copied selection".into(),
            _ => self.status = "Clipboard unavailable".into(),
        }
        self.redraw();
    }
    fn paste(&mut self) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        match self
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get_text().ok())
        {
            Some(text) => self.input(paste_bytes(&text, self.modes().bracketed_paste)),
            None => {
                self.status = "Clipboard unavailable or has no text".into();
                self.redraw();
            }
        }
    }
    fn point_at(&self, x: f64, y: f64) -> Option<(u64, Point)> {
        self.rects()
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(pane, rect)| {
                let size = self.rect_size(rect);
                (
                    pane,
                    Point {
                        col: (((x - rect.x as f64) / self.renderer.cell_width as f64) as usize)
                            .min(size.cols.saturating_sub(1) as usize),
                        row: (((y - rect.y as f64) / self.renderer.cell_height as f64) as usize)
                            .min(size.rows.saturating_sub(1) as usize),
                    },
                )
            })
    }
    fn mouse_report_to(&mut self, pane: u64, code: u8, point: Point, release: bool) {
        let sgr = self
            .screen(pane)
            .is_some_and(|screen| screen.modes.mouse_sgr);
        // A legacy release must still unstick an application if the drag ended
        // beyond the protocol's byte-coordinate range.
        let point = if release && !sgr {
            Point {
                row: point.row.min(222),
                col: point.col.min(222),
            }
        } else {
            point
        };
        if let Some(bytes) = encode_mouse(code, point, release, self.mods, sgr) {
            self.selection = None;
            self.send(Request::Input { pane, bytes }, false);
        }
    }
    fn release_mouse_capture(&mut self, button: MouseButton) -> bool {
        let Some(origin) = self.mouse_origin.take() else {
            return false;
        };
        if let Some((pane, point)) = captured_mouse_target(
            &self.rects(),
            origin,
            self.cursor.x,
            self.cursor.y,
            self.renderer.cell_width,
            self.renderer.cell_height,
        ) {
            self.mouse_report_to(pane, mouse_code(button), point, true);
        }
        self.mouse_button = None;
        self.dragging = false;
        true
    }
    fn mouse_move(&mut self) {
        if let Some(origin) = self.mouse_origin {
            if !self.mods.shift_key()
                && let Some((pane, point)) = captured_mouse_target(
                    &self.rects(),
                    origin,
                    self.cursor.x,
                    self.cursor.y,
                    self.renderer.cell_width,
                    self.renderer.cell_height,
                )
            {
                let modes = self
                    .screen(pane)
                    .map(|screen| screen.modes.clone())
                    .unwrap_or_default();
                if modes.mouse_motion || (modes.mouse_drag && self.mouse_button.is_some()) {
                    self.mouse_report_to(
                        pane,
                        self.mouse_button.map(mouse_code).unwrap_or(3) + 32,
                        point,
                        false,
                    );
                }
            }
            return;
        }
        if let Some((pane, point)) = self.point_at(self.cursor.x, self.cursor.y) {
            if self.dragging {
                if let Some(selection) = &mut self.selection
                    && selection.pane == pane
                {
                    selection.end = point;
                    self.redraw();
                }
            } else if pane == self.pane_id && !self.mods.shift_key() && self.modes().mouse_motion {
                self.mouse_report_to(pane, 35, point, false);
            }
        }
    }
    fn mouse_click(&mut self, state: ElementState, button: MouseButton) {
        // A release belongs to the application that received the press, even over
        // another pane, the header, or padding. Clamp to that pane's edge cells.
        if state == ElementState::Released && self.release_mouse_capture(button) {
            return;
        }
        if state == ElementState::Pressed
            && button == MouseButton::Left
            && self.cursor.y < HEADER as f64 * self.scale
        {
            let tabs = self.tab_rects();
            if let Some((id, _)) = tabs
                .into_iter()
                .find(|(_, rect)| rect.contains(self.cursor.x, self.cursor.y))
            {
                self.activate_window(id);
            }
            return;
        }
        let Some((pane, point)) = self.point_at(self.cursor.x, self.cursor.y) else {
            if state == ElementState::Released {
                self.dragging = false;
                self.mouse_button = None;
            }
            return;
        };
        if state == ElementState::Pressed {
            self.focus_pane(pane);
            self.mouse_button = Some(button);
        }
        let modes = self.modes();
        if (modes.mouse_click || modes.mouse_motion || modes.mouse_drag) && !self.mods.shift_key() {
            if state == ElementState::Pressed {
                self.mouse_origin = Some(pane);
            }
            self.mouse_report_to(
                pane,
                mouse_code(button),
                point,
                state == ElementState::Released,
            );
        } else if button == MouseButton::Left {
            if state == ElementState::Pressed {
                self.selection = Some(Selection {
                    pane,
                    start: point,
                    end: point,
                });
                self.dragging = true;
            } else {
                self.dragging = false;
            }
        }
        if state == ElementState::Released {
            self.mouse_button = None;
        }
        self.update_ime_position();
        self.redraw();
    }
    fn wheel(&mut self, delta: MouseScrollDelta) {
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => y as f64,
            MouseScrollDelta::PixelDelta(position) => position.y / self.renderer.cell_height as f64,
        };
        self.wheel_remainder += lines;
        let count = self.wheel_remainder.trunc() as i32;
        self.wheel_remainder -= count as f64;
        if count == 0 {
            return;
        }
        if let Some((pane, point)) = self.point_at(self.cursor.x, self.cursor.y) {
            self.focus_pane(pane);
            let modes = self.modes();
            if (modes.mouse_click || modes.mouse_motion || modes.mouse_drag)
                && !self.mods.shift_key()
            {
                for _ in 0..count.unsigned_abs().min(100) {
                    self.mouse_report_to(pane, if count > 0 { 64 } else { 65 }, point, false);
                }
            } else if modes.alternate_scroll && !self.mods.shift_key() {
                // DEC 1007: on the alternate screen the wheel sends cursor keys.
                self.send(
                    Request::Input {
                        pane,
                        bytes: alternate_scroll_bytes(count, modes.application_cursor),
                    },
                    false,
                );
            } else {
                self.selection = None;
                self.send(
                    Request::Scroll {
                        pane,
                        lines: count.saturating_mul(3),
                    },
                    false,
                );
            }
        }
    }
    /// Bell, OSC 52 copy and title changes carried by a new view.
    fn apply_screen_events(&mut self) {
        let Some(view) = &self.view else {
            return;
        };
        if view.screens.is_empty() {
            return;
        }
        let mut rang = HashSet::new();
        let mut copies = Vec::new();
        // Counters are kept only for panes in this view: a pane seen again
        // after a window switch (or a new pane) only records its counters, so
        // bells and copies from while it was hidden are never replayed.
        let mut bells = HashMap::new();
        let mut clipboard_seqs = HashMap::new();
        for (pane, screen) in &view.screens {
            if self.bells.get(pane).is_some_and(|old| *old != screen.bell) {
                rang.insert(*pane);
            }
            if self
                .clipboard_seqs
                .get(pane)
                .is_some_and(|old| *old != screen.clipboard_seq)
            {
                copies.push(*pane);
            }
            bells.insert(*pane, screen.bell);
            clipboard_seqs.insert(*pane, screen.clipboard_seq);
        }
        self.bells = bells;
        self.clipboard_seqs = clipboard_seqs;
        if !rang.is_empty() {
            self.bell_flash = Some((Instant::now() + Duration::from_millis(150), rang));
        }
        for pane in copies {
            self.send(Request::Clipboard { pane }, false);
        }
        let title = match self.screen(self.pane_id).map(|s| s.title.trim()) {
            Some(title) if !title.is_empty() => format!("{title} — crow-term"),
            _ => format!("crow-term — {}", self.session),
        };
        if title != self.window_title {
            if let Some(window) = &self.window {
                window.set_title(&title);
            }
            self.window_title = title;
        }
        if let Some(pane) = self.pending_focus_in
            && let Some(screen) = self.screen(pane)
        {
            let report = self.focused && pane == self.pane_id && screen.modes.focus_reporting;
            self.pending_focus_in = None;
            if report {
                self.send(
                    Request::Input {
                        pane,
                        bytes: b"\x1b[I".to_vec(),
                    },
                    false,
                );
            }
        }
    }
    fn store_clipboard(&mut self, text: String) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if self
            .clipboard
            .as_mut()
            .is_none_or(|clipboard| clipboard.set_text(text).is_err())
        {
            self.status = "OSC 52 copy could not reach the system clipboard".into();
            self.redraw();
        }
    }
    fn tab_rects(&self) -> Vec<(u64, Rect)> {
        let Some(window) = &self.window else {
            return Vec::new();
        };
        let start = (190.0 * self.scale) as i32;
        let available = window
            .inner_size()
            .width
            .saturating_sub(start.max(0) as u32 + 16);
        let windows = self.windows();
        let width = if windows.is_empty() {
            0
        } else {
            (available / windows.len() as u32).min((190.0 * self.scale) as u32)
        };
        windows
            .iter()
            .enumerate()
            .map(|(index, window)| {
                (
                    window.id,
                    Rect {
                        x: start + index as i32 * width as i32,
                        y: 0,
                        w: width,
                        h: (HEADER as f64 * self.scale) as u32,
                    },
                )
            })
            .collect()
    }
    fn draw(&mut self) -> Result<()> {
        let Some(window) = &self.window else {
            return Ok(());
        };
        let size = window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return Ok(());
        };
        let rects = self.rects();
        #[cfg(feature = "frame-dump")]
        let dump_panes: Vec<crate::frame_dump::PaneMeta> = rects
            .iter()
            .map(|&(id, rect)| {
                let grid = self.rect_size(rect);
                crate::frame_dump::PaneMeta {
                    id,
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                    rows: grid.rows,
                    cols: grid.cols,
                }
            })
            .collect();
        let flash_rects = rects.clone();
        let panel_rect = self.panel_rect();
        let tabs = self.tab_rects();
        let windows = self.windows();
        let chrome = Chrome::new(&self.config);
        let Some(surface) = &mut self.surface else {
            return Ok(());
        };
        surface
            .resize(width, height)
            .map_err(|error| anyhow::anyhow!("resize native surface: {error}"))?;
        let mut frame = surface
            .buffer_mut()
            .map_err(|error| anyhow::anyhow!("native framebuffer: {error}"))?;
        let fw = size.width as usize;
        let bounds = Rect {
            x: 0,
            y: 0,
            w: size.width,
            h: size.height,
        };
        frame.fill(parse_color(&self.config.background, 0x111111));
        let header_h = (HEADER as f64 * self.scale) as u32;
        fill(
            &mut frame,
            fw,
            Rect {
                h: header_h,
                ..bounds
            },
            chrome.surface,
        );
        fill(
            &mut frame,
            fw,
            Rect {
                x: 0,
                y: header_h.saturating_sub(1) as i32,
                w: size.width,
                h: 1,
            },
            chrome.border,
        );
        let label = format!("CROW / {}", self.session);
        self.renderer.text(
            &mut frame,
            fw,
            Rect {
                w: (180.0 * self.scale) as u32,
                h: header_h,
                ..bounds
            },
            (14.0 * self.scale) as i32,
            (13.0 * self.scale) as i32,
            &label,
            chrome.text,
            &self.config,
            (14.0 * self.scale) as f32,
        );
        for (id, rect) in tabs {
            let active = id == self.window_id;
            if active {
                fill(&mut frame, fw, rect, chrome.active);
                fill(
                    &mut frame,
                    fw,
                    Rect {
                        x: rect.x,
                        y: rect.h.saturating_sub(2) as i32,
                        w: rect.w,
                        h: 2,
                    },
                    chrome.text,
                );
            }
            if let Some(info) = windows.iter().find(|info| info.id == id) {
                let index = windows
                    .iter()
                    .position(|window| window.id == id)
                    .unwrap_or(0)
                    + 1;
                let text = format!("{index}  {}", info.name);
                self.renderer.text(
                    &mut frame,
                    fw,
                    rect.intersection(bounds),
                    rect.x + (10.0 * self.scale) as i32,
                    (13.0 * self.scale) as i32,
                    &text,
                    if active { chrome.text } else { chrome.muted },
                    &self.config,
                    (14.0 * self.scale) as f32,
                );
            }
        }
        let active_panes = rects.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        self.renderer
            .rows
            .retain(|(id, _), _| active_panes.contains(id));
        let screens = self
            .view
            .as_ref()
            .map(|view| view.screens.as_slice())
            .unwrap_or(&[]);
        for (pane, rect) in rects {
            let clip = rect.intersection(bounds);
            if let Some((_, screen)) = screens.iter().find(|(id, _)| *id == pane) {
                let cw = self.renderer.cell_width;
                let ch = self.renderer.cell_height;
                for (row, cells) in screen.cells.iter().enumerate().take((rect.h / ch) as usize) {
                    let mut selected_cells = None;
                    if let Some(selection) = &self.selection
                        && selection.pane == pane
                        && cells
                            .iter()
                            .enumerate()
                            .any(|(col, _)| selection.contains(row, col))
                    {
                        let mut copy = cells.clone();
                        let color = parse_color(&self.config.selection, 0xdadada);
                        for (col, cell) in copy.iter_mut().enumerate() {
                            if selection.contains(row, col) {
                                cell.bg = ColorSpec::Rgb(
                                    (color >> 16) as u8,
                                    (color >> 8) as u8,
                                    color as u8,
                                );
                            }
                        }
                        selected_cells = Some(copy);
                    }
                    let cached = self.renderer.row(
                        &self.config,
                        pane,
                        row,
                        selected_cells.as_deref().unwrap_or(cells),
                    );
                    blit(
                        &mut frame,
                        fw,
                        &cached.pixels,
                        cached.width,
                        rect.x,
                        rect.y + (row as u32 * ch) as i32,
                        clip,
                    );
                }
                if pane == self.pane_id
                    && screen.cursor.visible
                    && screen.cursor.shape != "hidden"
                    && screen.display_offset == 0
                {
                    let mut cursor = Rect {
                        x: rect.x + (screen.cursor.col as u32 * cw) as i32,
                        y: rect.y + (screen.cursor.row as u32 * ch) as i32,
                        w: cw,
                        h: ch,
                    };
                    let color = parse_color(&self.config.cursor, 0xffffff);
                    let shape = screen.cursor.shape.to_ascii_lowercase();
                    if shape.contains("beam") {
                        cursor.w = (self.scale.ceil() as u32).max(1);
                        fill(&mut frame, fw, cursor.intersection(clip), color);
                    } else if shape.contains("underline") {
                        cursor.y += ch.saturating_sub(2) as i32;
                        cursor.h = 2;
                        fill(&mut frame, fw, cursor.intersection(clip), color);
                    } else if shape.contains("hollow") {
                        let border = cursor.intersection(clip);
                        fill(&mut frame, fw, Rect { h: 1, ..border }, color);
                        fill(
                            &mut frame,
                            fw,
                            Rect {
                                y: border.y + border.h.saturating_sub(1) as i32,
                                h: 1,
                                ..border
                            },
                            color,
                        );
                        fill(&mut frame, fw, Rect { w: 1, ..border }, color);
                        fill(
                            &mut frame,
                            fw,
                            Rect {
                                x: border.x + border.w.saturating_sub(1) as i32,
                                w: 1,
                                ..border
                            },
                            color,
                        );
                    } else {
                        let cursor = cursor.intersection(clip);
                        for y in cursor.y..cursor.y + cursor.h as i32 {
                            for x in cursor.x..cursor.x + cursor.w as i32 {
                                let pixel = &mut frame[y as usize * fw + x as usize];
                                *pixel ^= 0xffffff;
                            }
                        }
                    }
                    if !self.ime.is_empty() {
                        let composition = Rect {
                            x: cursor.x,
                            y: cursor.y,
                            w: ((self.ime.chars().count() + 2) as u32 * cw).min(clip.w),
                            h: ch,
                        }
                        .intersection(clip);
                        fill(&mut frame, fw, composition, chrome.active);
                        self.renderer.text(
                            &mut frame,
                            fw,
                            composition,
                            composition.x,
                            composition.y,
                            &self.ime,
                            chrome.text,
                            &self.config,
                            self.renderer.font_size,
                        );
                    }
                }
                if pane == self.pane_id && active_panes.len() > 1 {
                    fill(
                        &mut frame,
                        fw,
                        Rect {
                            x: rect.x,
                            y: rect.y.saturating_sub(2),
                            w: rect.w,
                            h: 1,
                        }
                        .intersection(bounds),
                        chrome.border,
                    );
                }
            } else {
                self.renderer.text(
                    &mut frame,
                    fw,
                    clip,
                    rect.x,
                    rect.y,
                    "Connecting to persistent session…",
                    chrome.muted,
                    &self.config,
                    self.renderer.font_size,
                );
            }
        }
        if let Some((until, panes)) = &self.bell_flash
            && Instant::now() < *until
        {
            let thickness = (2.0 * self.scale).ceil().max(1.0) as u32;
            for &(pane, rect) in &flash_rects {
                if !panes.contains(&pane) {
                    continue;
                }
                let rect = rect.intersection(bounds);
                for edge in [
                    Rect {
                        h: thickness,
                        ..rect
                    },
                    Rect {
                        y: rect.y + rect.h.saturating_sub(thickness) as i32,
                        h: thickness,
                        ..rect
                    },
                    Rect {
                        w: thickness,
                        ..rect
                    },
                    Rect {
                        x: rect.x + rect.w.saturating_sub(thickness) as i32,
                        w: thickness,
                        ..rect
                    },
                ] {
                    fill(&mut frame, fw, edge.intersection(bounds), chrome.text);
                }
            }
        }
        let footer_y = size
            .height
            .saturating_sub((FOOTER as f64 * self.scale) as u32) as i32;
        let footer = Rect {
            x: 0,
            y: footer_y,
            w: size.width,
            h: size.height.saturating_sub(footer_y as u32),
        };
        fill(&mut frame, fw, footer, chrome.surface);
        let hint = if self.prefix {
            "PREFIX  c window   % / \" split   n/p tabs   h/j/k/l panes   a agents   d detach   x close"
                .to_owned()
        } else if !self.status.is_empty() {
            self.status.clone()
        } else {
            format!(
                "{}  ·  session {}  ·  pane {}  ·  closing this window detaches",
                if self.config.prefix.eq_ignore_ascii_case("none") {
                    "tmux passthrough".to_owned()
                } else {
                    format!("{} prefix", self.config.prefix)
                },
                self.session,
                self.pane_id
            )
        };
        let light = light_background(&self.config);
        // Footer order: prefix help alone; otherwise status/error messages first (they
        // must stay visible), then the crow segments, then the default session hint.
        let hint_color = if self.prefix {
            chrome.text
        } else {
            chrome.muted
        };
        let mut footer_items: Vec<(&str, u32)> = Vec::new();
        let message_first = self.prefix || !self.status.is_empty();
        if message_first {
            footer_items.push((&hint, hint_color));
        }
        if !self.prefix {
            footer_items.extend(self.crow_view.status.iter().map(|segment| {
                (
                    segment.text.as_str(),
                    tone_color(segment.tone, &chrome, light),
                )
            }));
        }
        if !message_first {
            footer_items.push((&hint, hint_color));
        }
        let mut footer_x = (12.0 * self.scale) as i32;
        let gap = (16.0 * self.scale) as i32;
        for (text, color) in footer_items {
            if footer_x >= footer.w as i32 {
                break;
            }
            let width = self.renderer.text(
                &mut frame,
                fw,
                footer,
                footer_x,
                footer_y + (4.0 * self.scale) as i32,
                text,
                color,
                &self.config,
                (11.0 * self.scale) as f32,
            );
            footer_x += width.ceil() as i32 + gap;
        }
        if let Some(panel) = panel_rect {
            let key = {
                use std::hash::{Hash, Hasher};
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                self.crow_view.panel.hash(&mut hash);
                (
                    panel.w,
                    panel.h,
                    self.scale.to_bits(),
                    chrome.surface,
                    light,
                )
                    .hash(&mut hash);
                hash.finish()
            };
            if self
                .panel_layer
                .as_ref()
                .is_none_or(|layer| layer.key != key)
            {
                self.panel_layer = Some(PanelLayer {
                    key,
                    width: panel.w as usize,
                    pixels: render_panel(
                        &mut self.renderer,
                        &self.config,
                        &self.crow_view.panel,
                        panel,
                        self.scale,
                        &chrome,
                        light,
                    ),
                });
            }
            if let Some(layer) = &self.panel_layer {
                blit(
                    &mut frame,
                    fw,
                    &layer.pixels,
                    layer.width,
                    panel.x,
                    panel.y,
                    bounds,
                );
            }
        }
        if let Some(shared) = &self.crow
            && self.crow_pane != self.pane_id
        {
            self.crow_pane = self.pane_id;
            shared.pane.store(self.pane_id, Ordering::Relaxed);
            shared.wake();
        }
        // Row raster cache is bounded by visible panes; bound glyph growth during long sessions.
        if self.renderer.swash.image_cache.len() > 16384 {
            self.renderer.swash.image_cache.clear();
        }
        #[cfg(feature = "frame-dump")]
        if let Some(dump) = &mut self.frame_dump {
            let cursor = self
                .view
                .as_ref()
                .and_then(|view| view.screens.iter().find(|(id, _)| *id == self.pane_id))
                .map(|(_, screen)| crate::frame_dump::CursorMeta {
                    row: screen.cursor.row,
                    col: screen.cursor.col,
                    visible: screen.cursor.visible,
                    shape: screen.cursor.shape.clone(),
                });
            let meta = crate::frame_dump::FrameMeta {
                schema: "crow-frame/1",
                seq: dump.next_seq(),
                renderer: "software",
                width: size.width,
                height: size.height,
                scale: self.scale,
                cell_width: self.renderer.cell_width,
                cell_height: self.renderer.cell_height,
                revision: self.view.as_ref().map_or(0, |view| view.revision),
                session: self.session.clone(),
                window: self.window_id,
                pane: self.pane_id,
                panes: dump_panes,
                cursor,
                focused: self.focused,
                prefix: self.prefix,
                status: self.status.clone(),
                ime: self.ime.clone(),
            };
            if let Err(error) = dump.write(&frame, size.width, size.height, &meta) {
                eprintln!(
                    "crow-term: frame dump to {} failed: {error}",
                    dump.dir().display()
                );
            }
        }
        frame
            .present()
            .map_err(|error| anyhow::anyhow!("present native surface: {error}"))?;
        Ok(())
    }
}
impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let result = (|| -> Result<()> {
            let attributes = Window::default_attributes()
                .with_title(format!("crow-term — {}", self.session))
                .with_theme(Some(if light_background(&self.config) {
                    Theme::Light
                } else {
                    Theme::Dark
                }))
                .with_inner_size(LogicalSize::new(1120.0, 760.0))
                .with_min_inner_size(LogicalSize::new(260.0, 160.0));
            // Terminal semantics: both macOS Option keys are Meta/Alt, not text
            // composition. Winit supplies the untransformed key (Option-f => ESC f,
            // never ESC ƒ). Unicode composition remains available through IME.
            #[cfg(target_os = "macos")]
            let attributes = attributes.with_option_as_alt(OptionAsAlt::Both);
            let window = Arc::new(event_loop.create_window(attributes)?);
            window.set_ime_allowed(true);
            self.scale = window.scale_factor();
            self.renderer.rescale(&self.config, self.scale);
            let context = softbuffer::Context::new(window.clone())
                .map_err(|e| anyhow::anyhow!("native display: {e}"))?;
            let surface = softbuffer::Surface::new(&context, window.clone())
                .map_err(|e| anyhow::anyhow!("native surface: {e}"))?;
            self.surface = Some(surface);
            self.window = Some(window);
            self.resize_panes();
            self.redraw();
            Ok(())
        })();
        if let Err(error) = result {
            self.fatal_error = Some(format!("{error:#}"));
            eprintln!("crow-term: {error:#}");
            event_loop.exit();
        }
    }
    fn suspended(&mut self, _: &ActiveEventLoop) {
        self.surface = None;
        self.window = None;
    }
    fn user_event(&mut self, _: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::ViewReady => {
                let Some((window, mut view)) = self.mailbox.take() else {
                    return;
                };
                // Long-poll results from a previously selected tab must never replace this tab.
                if window != self.window_id {
                    return;
                }
                let revision_changed = self
                    .view
                    .as_ref()
                    .is_none_or(|old| old.revision != view.revision);
                if !revision_changed
                    && view.screens.is_empty()
                    && let Some(old) = &mut self.view
                {
                    view.screens = std::mem::take(&mut old.screens);
                }
                self.view = Some(view);
                let windows = self.windows();
                if !windows.iter().any(|window| window.id == self.window_id)
                    && let Some(window) = windows.first()
                {
                    self.activate_window(window.id);
                }
                if let Some(info) = self.current_window() {
                    let panes = info.layout.panes();
                    if !panes.contains(&self.pane_id)
                        && let Some(&id) = panes.first()
                    {
                        self.focus_pane(id);
                        self.selection = None;
                    }
                }
                self.apply_screen_events();
                self.resize_panes();
                if revision_changed {
                    self.redraw();
                }
            }
            UiEvent::Reply {
                response,
                activate_created,
            } => match response {
                Response::Clipboard(Some((_, text))) => self.store_clipboard(text),
                Response::Created { window, pane } if activate_created => {
                    self.activate_window(window);
                    self.focus_pane(pane);
                    self.resize_panes();
                    self.redraw();
                }
                Response::Error(error) => {
                    self.status = error;
                    self.redraw();
                }
                _ => {}
            },
            UiEvent::Error(error) => {
                self.status = error;
                self.status_expires = None;
                self.redraw();
            }
            UiEvent::Link { status, expires } => {
                self.status = status.unwrap_or_default();
                self.status_expires = expires.then(|| Instant::now() + Duration::from_secs(3));
                self.redraw();
            }
            UiEvent::Reattached { window, pane } => {
                self.view = None;
                self.sent_sizes.clear();
                self.activate_window(window);
                self.focus_pane(pane);
                self.status =
                    "Daemon restarted — a new session replaced the one that ended with it".into();
                self.status_expires = Some(Instant::now() + Duration::from_secs(10));
                self.resize_panes();
                self.redraw();
            }
            UiEvent::Crow(view) => {
                self.crow_view = *view;
                self.redraw();
            }
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.window.as_ref().is_none_or(|window| window.id() != id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.draw() {
                    self.status = error.to_string();
                    eprintln!("crow-term: {error:#}");
                }
            }
            WindowEvent::Resized(_) => {
                self.resize_panes();
                self.redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor;
                self.renderer.rescale(&self.config, self.scale);
                self.sent_sizes.clear();
                self.resize_panes();
                self.redraw();
            }
            WindowEvent::ModifiersChanged(modifiers) => self.mods = modifiers.state(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => self.key_event(&event, event_loop),
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.ime.clear();
                self.input(text.into_bytes());
                self.redraw();
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                self.ime = text;
                self.redraw();
            }
            WindowEvent::Ime(Ime::Disabled) => {
                self.ime.clear();
                self.redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                self.mouse_move();
            }
            WindowEvent::MouseInput { state, button, .. } => self.mouse_click(state, button),
            WindowEvent::MouseWheel { delta, .. } => self.wheel(delta),
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if let Some(shared) = &self.crow {
                    shared.focused.store(focused, Ordering::Relaxed);
                    if focused {
                        shared.wake();
                    }
                }
                if !focused {
                    if let Some(button) = self.mouse_button {
                        self.release_mouse_capture(button);
                    }
                    self.mods = ModifiersState::empty();
                    self.pressed_keys.clear();
                    self.prefix = false;
                    self.dragging = false;
                    self.mouse_button = None;
                }
                if self.modes().focus_reporting {
                    self.send(
                        Request::Input {
                            pane: self.pane_id,
                            bytes: if focused {
                                b"\x1b[I".to_vec()
                            } else {
                                b"\x1b[O".to_vec()
                            },
                        },
                        false,
                    );
                }
                self.redraw();
            }
            _ => {}
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(until) = self.status_expires {
            if Instant::now() >= until {
                self.status_expires = None;
                self.status.clear();
                self.redraw();
            } else if self.bell_flash.is_none() {
                event_loop.set_control_flow(ControlFlow::WaitUntil(until));
                return;
            }
        }
        // The visual bell is the only other timed redraw; otherwise stay event-driven.
        match &self.bell_flash {
            Some((until, _)) if Instant::now() >= *until => {
                self.bell_flash = None;
                self.redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some((until, _)) => event_loop.set_control_flow(ControlFlow::WaitUntil(*until)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// Cursor keys for one wheel step on the alternate screen (3 lines per notch).
fn alternate_scroll_bytes(count: i32, application_cursor: bool) -> Vec<u8> {
    let key: &[u8] = match (count > 0, application_cursor) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1b[A",
        (false, true) => b"\x1bOB",
        (false, false) => b"\x1b[B",
    };
    key.repeat((count.unsigned_abs() as usize).min(100) * 3)
}
/// Keys that only modify another key (never a prefix action on their own).
fn is_modifier_key(key: &Key) -> bool {
    matches!(
        key,
        Key::Named(
            NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::Super
                | NamedKey::Meta
                | NamedKey::Hyper
                | NamedKey::Symbol
                | NamedKey::Fn
                | NamedKey::FnLock
                | NamedKey::CapsLock
                | NamedKey::NumLock
                | NamedKey::ScrollLock
                | NamedKey::SymbolLock
        )
    )
}
fn prefix_matches(prefix: &str, key: &Key, mods: ModifiersState) -> bool {
    if prefix.eq_ignore_ascii_case("none")
        || !mods.control_key()
        || mods.alt_key()
        || mods.super_key()
    {
        return false;
    }
    let Some(character) = prefix
        .strip_prefix("Ctrl-")
        .or_else(|| prefix.strip_prefix("ctrl-"))
    else {
        return false;
    };
    matches!(key, Key::Character(value) if value.eq_ignore_ascii_case(character))
}

fn captured_mouse_target(
    rects: &[(u64, Rect)],
    origin: u64,
    x: f64,
    y: f64,
    cw: u32,
    ch: u32,
) -> Option<(u64, Point)> {
    let (_, rect) = rects.iter().find(|(pane, _)| *pane == origin)?;
    if rect.w == 0 || rect.h == 0 || cw == 0 || ch == 0 {
        return None;
    }
    let cols = (rect.w / cw).max(1);
    let rows = (rect.h / ch).max(1);
    Some((
        origin,
        Point {
            col: ((x - rect.x as f64) / cw as f64)
                .floor()
                .clamp(0.0, cols.saturating_sub(1) as f64) as usize,
            row: ((y - rect.y as f64) / ch as f64)
                .floor()
                .clamp(0.0, rows.saturating_sub(1) as f64) as usize,
        },
    ))
}
fn mouse_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        _ => 3,
    }
}
/// Normal/X10 byte encoding and modern SGR encoding share the same button bits.
/// Legacy coordinates cannot represent cell 224+: refuse instead of wrapping or
/// silently clamping to an unrelated cell. UTF-8 mouse mode (1005) is not exported.
fn encode_mouse(
    code: u8,
    point: Point,
    release: bool,
    mods: ModifiersState,
    sgr: bool,
) -> Option<Vec<u8>> {
    let modifiers = (u8::from(mods.shift_key()) * 4)
        | (u8::from(mods.alt_key()) * 8)
        | (u8::from(mods.control_key()) * 16);
    let x = point.col.checked_add(1)?;
    let y = point.row.checked_add(1)?;
    if sgr {
        return Some(
            format!(
                "\x1b[<{};{x};{y}{}",
                code.checked_add(modifiers)?,
                if release { 'm' } else { 'M' }
            )
            .into_bytes(),
        );
    }
    if x > 223 || y > 223 {
        return None;
    }
    let button = if release { 3 } else { code };
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32u8.checked_add(button)?.checked_add(modifiers)?,
        x as u8 + 32,
        y as u8 + 32,
    ])
}
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    // Do not allow pasted data to terminate bracketed paste and inject commands.
    let text = text.replace('\0', "").replace("\x1b[201~", "");
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\n").replace('\n', "\r").into_bytes()
    }
}
fn selected_text(screen: &Screen, selection: &Selection) -> String {
    let (start, end) = selection.ordered();
    let mut lines = Vec::new();
    for row in start.row..=end.row.min(screen.cells.len().saturating_sub(1)) {
        let Some(cells) = screen.cells.get(row) else {
            break;
        };
        let first = if row == start.row { start.col } else { 0 };
        let last = if row == end.row {
            end.col.saturating_add(1).min(cells.len())
        } else {
            cells.len()
        };
        let text = cells
            .iter()
            .take(last)
            .skip(first)
            .filter(|cell| !cell.wide_spacer)
            .map(|cell| {
                if cell.text.is_empty() {
                    " "
                } else {
                    cell.text.as_str()
                }
            })
            .collect::<String>();
        lines.push(text.trim_end().to_string());
    }
    lines.join("\n")
}
/// Bytes for a named key or a character, as the GUI would send them
/// (testing aid behind the hidden `debug-encode-key` command).
pub fn encode_named_key(name: &str, mods: ModifiersState, release: bool, modes: &Modes) -> Vec<u8> {
    let named = |key| (Key::Named(key), None);
    let (key, text): (Key, Option<String>) = match name {
        "Enter" => (Key::Named(NamedKey::Enter), Some("\r".into())),
        "Tab" => (Key::Named(NamedKey::Tab), Some("\t".into())),
        "Backspace" => named(NamedKey::Backspace),
        "Escape" => named(NamedKey::Escape),
        "Space" => (Key::Named(NamedKey::Space), Some(" ".into())),
        "Up" => named(NamedKey::ArrowUp),
        "Down" => named(NamedKey::ArrowDown),
        "Left" => named(NamedKey::ArrowLeft),
        "Right" => named(NamedKey::ArrowRight),
        "F1" => named(NamedKey::F1),
        "F3" => named(NamedKey::F3),
        "F5" => named(NamedKey::F5),
        other => (Key::Character(other.into()), Some(other.into())),
    };
    let base = key.clone();
    let key = match (&key, mods.shift_key()) {
        (Key::Character(text), true) => Key::Character(text.to_uppercase().into()),
        _ => key,
    };
    let text = match (&key, text) {
        (Key::Character(shifted), Some(_)) => Some(shifted.to_string()),
        (_, text) => text,
    };
    let input = KeyInput {
        key: &key,
        base: &base,
        text: text.as_deref(),
        physical: PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified),
        location: KeyLocation::Standard,
        mods,
        action: if release {
            KeyAction::Release
        } else {
            KeyAction::Press
        },
    };
    encode_input(&input, modes)
}
/// Encode a key for the focused pane: kitty keyboard protocol when the
/// application enabled it, xterm modifyOtherKeys when requested, else legacy.
pub fn encode_input(input: &KeyInput, modes: &Modes) -> Vec<u8> {
    if input.mods.super_key() {
        // Super/Command chords are GUI shortcuts, never PTY input.
        return Vec::new();
    }
    if modes.keyboard != 0 {
        return keyboard::encode_kitty(input, modes.keyboard, modes.application_cursor);
    }
    if input.action == KeyAction::Release {
        return Vec::new();
    }
    if let Some(bytes) = keyboard::encode_modify_other_keys(input, modes.modify_other_keys) {
        return bytes;
    }
    encode_key_event(
        input.key,
        input.text,
        input.physical,
        input.location,
        input.mods,
        modes,
    )
}
fn modifier_parameter(mods: ModifiersState) -> u8 {
    1 + u8::from(mods.shift_key()) + 2 * u8::from(mods.alt_key()) + 4 * u8::from(mods.control_key())
}
fn encode_key_event(
    key: &Key,
    text: Option<&str>,
    physical: PhysicalKey,
    location: KeyLocation,
    mods: ModifiersState,
    modes: &Modes,
) -> Vec<u8> {
    if modes.application_keypad
        && !mods.super_key()
        && let Some(final_char) = keypad_final(physical, location, key)
    {
        let parameter = modifier_parameter(mods);
        // DEC keypad SS3; retain modifier information using the same xterm
        // parameter extension as the other terminal special keys.
        return if parameter == 1 {
            format!("\x1bO{final_char}")
        } else {
            format!("\x1b[1;{parameter}{final_char}")
        }
        .into_bytes();
    }
    encode_key(key, text, mods, modes)
}
fn keypad_final(physical: PhysicalKey, location: KeyLocation, key: &Key) -> Option<char> {
    let physical_final = match physical {
        PhysicalKey::Code(KeyCode::NumpadEnter) => Some('M'),
        PhysicalKey::Code(KeyCode::NumpadDecimal) => Some('n'),
        PhysicalKey::Code(KeyCode::NumpadComma) => Some('l'),
        PhysicalKey::Code(KeyCode::Numpad0) => Some('p'),
        PhysicalKey::Code(KeyCode::Numpad1) => Some('q'),
        PhysicalKey::Code(KeyCode::Numpad2) => Some('r'),
        PhysicalKey::Code(KeyCode::Numpad3) => Some('s'),
        PhysicalKey::Code(KeyCode::Numpad4) => Some('t'),
        PhysicalKey::Code(KeyCode::Numpad5) => Some('u'),
        PhysicalKey::Code(KeyCode::Numpad6) => Some('v'),
        PhysicalKey::Code(KeyCode::Numpad7) => Some('w'),
        PhysicalKey::Code(KeyCode::Numpad8) => Some('x'),
        PhysicalKey::Code(KeyCode::Numpad9) => Some('y'),
        PhysicalKey::Code(KeyCode::NumpadAdd) => Some('k'),
        PhysicalKey::Code(KeyCode::NumpadSubtract) => Some('m'),
        PhysicalKey::Code(KeyCode::NumpadMultiply) => Some('j'),
        PhysicalKey::Code(KeyCode::NumpadDivide) => Some('o'),
        PhysicalKey::Code(KeyCode::NumpadEqual) => Some('X'),
        _ => None,
    };
    if physical_final.is_some() {
        return physical_final;
    }
    if location != KeyLocation::Numpad {
        return None;
    }
    // Some native backends identify a keypad location without a physical code.
    match key {
        Key::Named(NamedKey::Enter) => Some('M'),
        Key::Named(NamedKey::Tab) => Some('I'),
        Key::Named(NamedKey::Space) => Some(' '),
        Key::Named(NamedKey::Insert) => Some('p'),
        Key::Named(NamedKey::End) => Some('q'),
        Key::Named(NamedKey::ArrowDown) => Some('r'),
        Key::Named(NamedKey::PageDown) => Some('s'),
        Key::Named(NamedKey::ArrowLeft) => Some('t'),
        Key::Named(NamedKey::Clear) => Some('u'),
        Key::Named(NamedKey::ArrowRight) => Some('v'),
        Key::Named(NamedKey::Home) => Some('w'),
        Key::Named(NamedKey::ArrowUp) => Some('x'),
        Key::Named(NamedKey::PageUp) => Some('y'),
        Key::Named(NamedKey::Delete) => Some('n'),
        Key::Character(text) => match text.as_str() {
            "0" => Some('p'),
            "1" => Some('q'),
            "2" => Some('r'),
            "3" => Some('s'),
            "4" => Some('t'),
            "5" => Some('u'),
            "6" => Some('v'),
            "7" => Some('w'),
            "8" => Some('x'),
            "9" => Some('y'),
            "+" => Some('k'),
            "-" => Some('m'),
            "*" => Some('j'),
            "/" => Some('o'),
            "=" => Some('X'),
            "." => Some('n'),
            "," => Some('l'),
            _ => None,
        },
        _ => None,
    }
}
fn encode_key(key: &Key, text: Option<&str>, mods: ModifiersState, modes: &Modes) -> Vec<u8> {
    if mods.super_key() {
        return Vec::new();
    }
    let parameter = modifier_parameter(mods);
    let csi = |final_char: char| {
        if parameter == 1 {
            format!(
                "\x1b{}{}",
                if modes.application_cursor { 'O' } else { '[' },
                final_char
            )
        } else {
            format!("\x1b[1;{parameter}{final_char}")
        }
    };
    let tilde = |number: u8| {
        if parameter == 1 {
            format!("\x1b[{number}~")
        } else {
            format!("\x1b[{number};{parameter}~")
        }
    };
    let named = match key {
        Key::Named(NamedKey::ArrowUp) => Some(csi('A')),
        Key::Named(NamedKey::ArrowDown) => Some(csi('B')),
        Key::Named(NamedKey::ArrowRight) => Some(csi('C')),
        Key::Named(NamedKey::ArrowLeft) => Some(csi('D')),
        Key::Named(NamedKey::Home) => Some(csi('H')),
        Key::Named(NamedKey::End) => Some(csi('F')),
        Key::Named(NamedKey::Insert) => Some(tilde(2)),
        Key::Named(NamedKey::Delete) => Some(tilde(3)),
        Key::Named(NamedKey::PageUp) => Some(tilde(5)),
        Key::Named(NamedKey::PageDown) => Some(tilde(6)),
        Key::Named(NamedKey::F1 | NamedKey::F2 | NamedKey::F3 | NamedKey::F4) => {
            let final_char = match key {
                Key::Named(NamedKey::F1) => 'P',
                Key::Named(NamedKey::F2) => 'Q',
                Key::Named(NamedKey::F3) => 'R',
                _ => 'S',
            };
            Some(if parameter == 1 {
                format!("\x1bO{final_char}")
            } else {
                format!("\x1b[1;{parameter}{final_char}")
            })
        }
        Key::Named(NamedKey::F5) => Some(tilde(15)),
        Key::Named(NamedKey::F6) => Some(tilde(17)),
        Key::Named(NamedKey::F7) => Some(tilde(18)),
        Key::Named(NamedKey::F8) => Some(tilde(19)),
        Key::Named(NamedKey::F9) => Some(tilde(20)),
        Key::Named(NamedKey::F10) => Some(tilde(21)),
        Key::Named(NamedKey::F11) => Some(tilde(23)),
        Key::Named(NamedKey::F12) => Some(tilde(24)),
        Key::Named(NamedKey::Tab) if mods.shift_key() => Some("\x1b[Z".into()),
        _ => None,
    };
    if let Some(sequence) = named {
        return sequence.into_bytes();
    }
    let mut bytes = match key {
        Key::Named(NamedKey::Enter) => vec![b'\r'],
        Key::Named(NamedKey::Backspace) => vec![0x7f],
        Key::Named(NamedKey::Tab) => vec![b'\t'],
        Key::Named(NamedKey::Escape) => vec![0x1b],
        Key::Named(NamedKey::Space) => vec![if mods.control_key() { 0 } else { b' ' }],
        Key::Character(character) => {
            if mods.control_key() {
                let mut chars = character.chars();
                let Some(c) = chars.next() else {
                    return Vec::new();
                };
                if chars.next().is_some() {
                    return Vec::new();
                }
                let c = c.to_ascii_lowercase();
                match keyboard::ctrl_byte(c) {
                    Some(byte) => vec![byte],
                    // xterm/kitty legacy: printable ASCII without a C0 form
                    // (Ctrl+. , ; ' 0 1 9 ...) sends the character itself.
                    None if c.is_ascii_graphic() => vec![c as u8],
                    None => return Vec::new(),
                }
            } else {
                text.unwrap_or(character.as_str()).as_bytes().to_vec()
            }
        }
        _ => return Vec::new(),
    };
    if mods.alt_key() {
        bytes.insert(0, 0x1b);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn application_keypad_is_physical_and_keeps_modifiers() {
        let mut modes = Modes {
            application_keypad: true,
            ..Modes::default()
        };
        for (code, final_char) in [
            (KeyCode::NumpadEnter, 'M'),
            (KeyCode::NumpadDecimal, 'n'),
            (KeyCode::Numpad0, 'p'),
            (KeyCode::Numpad1, 'q'),
            (KeyCode::Numpad2, 'r'),
            (KeyCode::Numpad3, 's'),
            (KeyCode::Numpad4, 't'),
            (KeyCode::Numpad5, 'u'),
            (KeyCode::Numpad6, 'v'),
            (KeyCode::Numpad7, 'w'),
            (KeyCode::Numpad8, 'x'),
            (KeyCode::Numpad9, 'y'),
            (KeyCode::NumpadAdd, 'k'),
            (KeyCode::NumpadSubtract, 'm'),
            (KeyCode::NumpadMultiply, 'j'),
            (KeyCode::NumpadDivide, 'o'),
            (KeyCode::NumpadEqual, 'X'),
        ] {
            assert_eq!(
                encode_key_event(
                    &Key::Named(NamedKey::Enter),
                    None,
                    PhysicalKey::Code(code),
                    KeyLocation::Numpad,
                    ModifiersState::empty(),
                    &modes
                ),
                format!("\x1bO{final_char}").as_bytes()
            );
        }
        assert_eq!(
            encode_key_event(
                &Key::Named(NamedKey::Enter),
                None,
                PhysicalKey::Code(KeyCode::NumpadEnter),
                KeyLocation::Numpad,
                ModifiersState::CONTROL | ModifiersState::ALT,
                &modes
            ),
            b"\x1b[1;7M"
        );
        assert_eq!(
            encode_key_event(
                &Key::Named(NamedKey::Enter),
                None,
                PhysicalKey::Code(KeyCode::Enter),
                KeyLocation::Standard,
                ModifiersState::empty(),
                &modes
            ),
            b"\r"
        );
        assert_eq!(
            encode_key_event(
                &Key::Character("1".into()),
                Some("1"),
                PhysicalKey::Code(KeyCode::Digit1),
                KeyLocation::Standard,
                ModifiersState::empty(),
                &modes
            ),
            b"1"
        );
        modes.application_keypad = false;
        assert_eq!(
            encode_key_event(
                &Key::Named(NamedKey::Enter),
                None,
                PhysicalKey::Code(KeyCode::NumpadEnter),
                KeyLocation::Numpad,
                ModifiersState::empty(),
                &modes
            ),
            b"\r"
        );
    }
    #[test]
    fn application_arrows_and_modifiers() {
        let mut modes = Modes::default();
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::ArrowUp),
                None,
                ModifiersState::empty(),
                &modes
            ),
            b"\x1b[A"
        );
        modes.application_cursor = true;
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::ArrowUp),
                None,
                ModifiersState::empty(),
                &modes
            ),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::ArrowLeft),
                None,
                ModifiersState::CONTROL | ModifiersState::SHIFT,
                &modes
            ),
            b"\x1b[1;6D"
        );
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::F12),
                None,
                ModifiersState::ALT,
                &modes
            ),
            b"\x1b[24;3~"
        );
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
        let mut renderer = Renderer::new(&config, 1.0);
        let row = renderer.row(&config, 1, 0, &[curly]);
        let height = row.pixels.len() / row.width;
        let red: Vec<usize> = (0..height)
            .filter(|y| row.pixels[y * row.width..(y + 1) * row.width].contains(&0xff0000))
            .collect();
        assert!(red.len() >= 2, "{red:?}");
        assert!(red.iter().all(|&y| y >= height * 2 / 3), "{red:?}");
    }
    #[test]
    fn alternate_scroll_sends_three_cursor_keys_per_notch() {
        assert_eq!(alternate_scroll_bytes(1, false), b"\x1b[A\x1b[A\x1b[A");
        assert_eq!(alternate_scroll_bytes(-1, true), b"\x1bOB\x1bOB\x1bOB");
        assert_eq!(alternate_scroll_bytes(i32::MIN, false).len(), 100 * 3 * 3);
    }
    #[test]
    fn key_routing_follows_the_application_keyboard_mode() {
        let slash = Key::Character("/".into());
        let dot = Key::Character(".".into());
        let a = Key::Character("a".into());
        let input = |key: &'static Key, code, mods, action| KeyInput {
            key,
            base: key,
            text: None,
            physical: PhysicalKey::Code(code),
            location: KeyLocation::Standard,
            mods,
            action,
        };
        let slash: &'static Key = Box::leak(Box::new(slash));
        let dot: &'static Key = Box::leak(Box::new(dot));
        let a: &'static Key = Box::leak(Box::new(a));
        let mut modes = Modes::default();
        let ctrl_slash = input(
            slash,
            KeyCode::Slash,
            ModifiersState::CONTROL,
            KeyAction::Press,
        );
        let ctrl_dot = input(
            dot,
            KeyCode::Period,
            ModifiersState::CONTROL,
            KeyAction::Press,
        );
        let release = input(
            a,
            KeyCode::KeyA,
            ModifiersState::empty(),
            KeyAction::Release,
        );
        // Legacy: xterm C0 for Ctrl+/ and the plain character for Ctrl+.
        assert_eq!(encode_input(&ctrl_slash, &modes), vec![0x1f]);
        assert_eq!(encode_input(&ctrl_dot, &modes), b".");
        assert_eq!(encode_input(&release, &modes), b"");
        modes.modify_other_keys = 1;
        assert_eq!(encode_input(&ctrl_dot, &modes), b"\x1b[27;5;46~");
        assert_eq!(encode_input(&ctrl_slash, &modes), vec![0x1f]);
        // Kitty mode wins over modifyOtherKeys and reports releases on flag 2.
        modes.keyboard = keyboard::DISAMBIGUATE | keyboard::EVENT_TYPES;
        assert_eq!(encode_input(&ctrl_slash, &modes), b"\x1b[47;5u");
        assert_eq!(encode_input(&release, &modes), b"\x1b[97;1:3u");
        let command = input(a, KeyCode::KeyA, ModifiersState::SUPER, KeyAction::Press);
        assert_eq!(encode_input(&command, &modes), b"");
    }
    #[test]
    fn vim_ctrl_chords_and_utf8_are_not_reinterpreted() {
        let modes = Modes::default();
        assert_eq!(
            encode_key(
                &Key::Character("c".into()),
                None,
                ModifiersState::CONTROL,
                &modes
            ),
            vec![3]
        );
        assert_eq!(
            encode_key(
                &Key::Character("[".into()),
                None,
                ModifiersState::CONTROL,
                &modes
            ),
            vec![27]
        );
        assert_eq!(
            encode_key(
                &Key::Character("j".into()),
                None,
                ModifiersState::ALT,
                &modes
            ),
            b"\x1bj"
        );
        assert_eq!(
            encode_key(
                &Key::Character("é".into()),
                Some("é"),
                ModifiersState::empty(),
                &modes
            ),
            "é".as_bytes()
        );
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::Backspace),
                None,
                ModifiersState::empty(),
                &modes
            ),
            vec![127]
        );
    }
    #[test]
    fn recursive_layout_conserves_space_without_underflow() {
        let layout = Layout::Split {
            axis: Axis::Vertical,
            first: Box::new(Layout::Pane(1)),
            second: Box::new(Layout::Split {
                axis: Axis::Horizontal,
                first: Box::new(Layout::Pane(2)),
                second: Box::new(Layout::Pane(3)),
            }),
        };
        let mut rects = Vec::new();
        pane_rects(
            &layout,
            Rect {
                x: 10,
                y: 20,
                w: 101,
                h: 81,
            },
            1,
            &mut rects,
        );
        assert_eq!(
            rects[0].1,
            Rect {
                x: 10,
                y: 20,
                w: 50,
                h: 81
            }
        );
        assert_eq!(
            rects[1].1,
            Rect {
                x: 61,
                y: 20,
                w: 50,
                h: 40
            }
        );
        assert_eq!(
            rects[2].1,
            Rect {
                x: 61,
                y: 61,
                w: 50,
                h: 40
            }
        );
        rects.clear();
        pane_rects(&layout, Rect::default(), 4, &mut rects);
        assert!(rects.iter().all(|(_, r)| r.w == 0 && r.h == 0));
    }
    #[test]
    fn clipping_does_not_write_outside_destination() {
        let mut pixels = vec![0; 12];
        fill(
            &mut pixels,
            4,
            Rect {
                x: -2,
                y: -2,
                w: 4,
                h: 4,
            },
            7,
        );
        assert_eq!(pixels, vec![7, 7, 0, 0, 7, 7, 0, 0, 0, 0, 0, 0]);
        blit(
            &mut pixels,
            4,
            &[1, 2, 3, 4],
            2,
            3,
            2,
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 3,
            },
        );
        assert_eq!(pixels[11], 1);
    }
    #[test]
    fn paste_protocol_and_escape_injection() {
        assert_eq!(paste_bytes("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(paste_bytes("a\r\nb", false), b"a\rb");
        assert_eq!(paste_bytes("a\x1b[201~b\0", true), b"\x1b[200~ab\x1b[201~");
    }
    #[test]
    fn wide_and_combining_clusters_keep_grid_columns() {
        let mut spacer = cell("");
        spacer.wide_spacer = true;
        let (text, map) = row_text(&[cell("界"), spacer, cell("e\u{301}"), cell("!")]);
        assert_eq!(text, "界e\u{301}!");
        assert_eq!(map, vec![(0, 0), (3, 2), (6, 3)]);
        assert_eq!(column_for_byte(&map, 4), 2);
    }
    #[test]
    fn truecolor_and_extended_palette() {
        let config = Config::default();
        assert_eq!(resolve_color(&config, ColorSpec::Rgb(1, 2, 3)), 0x010203);
        assert_eq!(resolve_color(&config, ColorSpec::Indexed(16)), 0);
        assert_eq!(resolve_color(&config, ColorSpec::Indexed(231)), 0xffffff);
        assert_eq!(resolve_color(&config, ColorSpec::Indexed(255)), 0xeeeeee);
    }
    #[test]
    fn mouse_capture_release_clamps_to_origin_not_hovered_pane() {
        let rects = [
            (
                7,
                Rect {
                    x: 10,
                    y: 50,
                    w: 100,
                    h: 100,
                },
            ),
            (
                8,
                Rect {
                    x: 120,
                    y: 50,
                    w: 100,
                    h: 100,
                },
            ),
        ];
        assert_eq!(
            captured_mouse_target(&rects, 7, 150.0, 80.0, 10, 20),
            Some((7, Point { col: 9, row: 1 }))
        );
        assert_eq!(
            captured_mouse_target(&rects, 7, -20.0, 10.0, 10, 20),
            Some((7, Point { col: 0, row: 0 }))
        );
        assert_eq!(
            captured_mouse_target(&rects, 7, 900.0, 900.0, 10, 20),
            Some((7, Point { col: 9, row: 4 }))
        );
        assert!(captured_mouse_target(&rects, 9, 10.0, 10.0, 10, 20).is_none());
    }
    #[test]
    fn chrome_tracks_light_and_dark_config_without_hardcoded_dark_surfaces() {
        let mut config = Config {
            background: "#FFFFFF".into(),
            foreground: "#444444".into(),
            ..Config::default()
        };
        let light = Chrome::new(&config);
        assert!(light_background(&config));
        assert_eq!(light.text, 0x444444);
        assert!(light.surface > 0xeeeeee);
        config.background = "#111111".into();
        config.foreground = "#EAEAEA".into();
        let dark = Chrome::new(&config);
        assert!(!light_background(&config));
        assert_eq!(dark.text, 0xeaeaea);
        assert!(dark.surface < 0x222222);
    }
    #[test]
    fn legacy_mouse_buttons_release_motion_and_coordinate_limit() {
        let p = Point { row: 0, col: 0 };
        assert_eq!(
            encode_mouse(0, p, false, ModifiersState::empty(), false),
            Some(b"\x1b[M !!".to_vec())
        );
        assert_eq!(
            encode_mouse(2, p, true, ModifiersState::empty(), false),
            Some(b"\x1b[M#!!".to_vec())
        );
        assert_eq!(
            encode_mouse(32, p, false, ModifiersState::CONTROL, false),
            Some(vec![27, 91, 77, 80, 33, 33])
        );
        assert_eq!(
            encode_mouse(35, p, false, ModifiersState::empty(), false),
            Some(vec![27, 91, 77, 67, 33, 33])
        );
        assert_eq!(
            encode_mouse(64, p, false, ModifiersState::empty(), false),
            Some(b"\x1b[M`!!".to_vec())
        );
        assert_eq!(
            encode_mouse(
                0,
                Point { row: 222, col: 222 },
                false,
                ModifiersState::empty(),
                false
            )
            .unwrap()[4..],
            [255, 255]
        );
        assert!(
            encode_mouse(
                0,
                Point { row: 0, col: 223 },
                false,
                ModifiersState::empty(),
                false
            )
            .is_none()
        );
        assert_eq!(
            encode_mouse(
                0,
                Point { row: 0, col: 223 },
                true,
                ModifiersState::empty(),
                true
            ),
            Some(b"\x1b[<0;224;1m".to_vec())
        );
        assert_eq!(
            encode_mouse(
                0,
                p,
                false,
                ModifiersState::CONTROL | ModifiersState::ALT,
                true
            ),
            Some(b"\x1b[<24;1;1M".to_vec())
        );
    }
    #[test]
    fn large_bracketed_paste_is_chunked_as_one_ordered_command() {
        let text = "é中".repeat(300_000);
        let bytes = paste_bytes(&text, true);
        assert!(bytes.len() > 1024 * 1024);
        let mut received = Vec::new();
        let mut panes = Vec::new();
        let mut sender = |request: &Request| -> Result<Response> {
            let Request::Input { pane, bytes } = request else {
                panic!("input only")
            };
            assert!(bytes.len() <= INPUT_CHUNK_BYTES);
            panes.push(*pane);
            received.extend_from_slice(bytes);
            Ok(Response::Ok)
        };
        assert!(matches!(
            execute_request(
                &Request::Input {
                    pane: 7,
                    bytes: bytes.clone()
                },
                &mut sender
            )
            .unwrap(),
            Response::Ok
        ));
        execute_request(
            &Request::Input {
                pane: 8,
                bytes: b"next".to_vec(),
            },
            &mut sender,
        )
        .unwrap();
        assert_eq!(&received[..bytes.len()], bytes);
        assert_eq!(&received[bytes.len()..], b"next");
        assert_eq!(panes.pop(), Some(8));
        assert!(panes.iter().all(|pane| *pane == 7));
        assert_eq!(
            received
                .windows(6)
                .filter(|window| *window == b"\x1b[200~")
                .count(),
            1
        );
        assert_eq!(
            received
                .windows(6)
                .filter(|window| *window == b"\x1b[201~")
                .count(),
            1
        );
    }
    #[test]
    fn chunk_failure_reports_partial_progress_and_stops() {
        let mut calls = 0;
        let error = execute_request(
            &Request::Input {
                pane: 1,
                bytes: vec![b'x'; INPUT_CHUNK_BYTES * 3],
            },
            |_| {
                calls += 1;
                Ok(if calls == 2 {
                    Response::Error("pane closed".into())
                } else {
                    Response::Ok
                })
            },
        )
        .unwrap_err();
        assert_eq!(calls, 2);
        assert!(error.to_string().contains("16384/49152"));
    }
    #[test]
    fn modifier_presses_do_not_count_as_prefix_actions() {
        for named in [
            NamedKey::Shift,
            NamedKey::Control,
            NamedKey::Alt,
            NamedKey::AltGraph,
            NamedKey::Super,
            NamedKey::CapsLock,
        ] {
            assert!(is_modifier_key(&Key::Named(named)), "{named:?}");
        }
        for key in [
            Key::Named(NamedKey::Tab),
            Key::Named(NamedKey::Escape),
            Key::Character("%".into()),
        ] {
            assert!(!is_modifier_key(&key), "{key:?}");
        }
    }
    #[test]
    fn tmux_passthrough_never_consumes_its_prefix() {
        let key = Key::Character("t".into());
        assert!(prefix_matches("Ctrl-t", &key, ModifiersState::CONTROL));
        assert!(!prefix_matches("none", &key, ModifiersState::CONTROL));
        assert!(!prefix_matches("Ctrl-t", &key, ModifiersState::empty()));
        assert_eq!(
            encode_key(&key, None, ModifiersState::CONTROL, &Modes::default()),
            vec![20]
        );
    }
    #[test]
    fn watch_mailbox_coalesces_without_losing_screen_data() {
        let mailbox = ViewMailbox::default();
        let screen = Screen {
            size: Size::default(),
            cells: vec![vec![cell("x")]],
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
        };
        assert!(mailbox.push(
            1,
            View {
                revision: 2,
                sessions: vec![],
                screens: vec![(1, screen)]
            }
        ));
        assert!(!mailbox.push(
            1,
            View {
                revision: 2,
                sessions: vec![],
                screens: vec![]
            }
        ));
        let (_, view) = mailbox.take().unwrap();
        assert_eq!(view.screens[0].1.cells[0][0].text, "x");
        assert!(mailbox.push(
            2,
            View {
                revision: 3,
                sessions: vec![],
                screens: vec![]
            }
        ));
        assert_eq!(mailbox.take().unwrap().0, 2);
    }
    #[test]
    fn shaped_clusters_do_not_shift_following_terminal_cells() {
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
        let cw = renderer.cell_width as usize;
        let mut spacer = cell("");
        spacer.wide_spacer = true;
        for mut prefix in [
            vec![cell("界"), spacer],
            vec![cell("e\u{301}"), cell(" ")],
            vec![cell("!"), cell("=")],
        ] {
            prefix.push(cell("X"));
            let actual = renderer.row(&config, 1, 0, &prefix).pixels.clone();
            let expected = renderer
                .row(&config, 2, 0, &[cell(" "), cell(" "), cell("X")])
                .pixels
                .clone();
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
    fn symbols_family_only_for_icons_the_configured_font_lacks() {
        let config = Config::default();
        let mut renderer = Renderer::new(&config, 1.0);
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
    #[test]
    fn bundled_font_shapes_ligatures_and_raster_cache_reuses_rows() {
        let config = Config {
            ligatures: true,
            ..Config::default()
        };
        let mut renderer = Renderer::new(&config, 1.0);
        let cells = "!=->==="
            .chars()
            .map(|c| cell(&c.to_string()))
            .collect::<Vec<_>>();
        let first = renderer.row(&config, 1, 0, &cells).pixels.as_ptr();
        assert_eq!(first, renderer.row(&config, 1, 0, &cells).pixels.as_ptr());
        assert!(
            renderer
                .row(&config, 1, 0, &cells)
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
