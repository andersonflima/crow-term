//! Native, event-driven software frontend. PTYs live in the mux server, not this window.
//! The two RPC workers keep long polls and command I/O off the native event thread.
use crate::config::Config;
use crate::crow::{self, CrowView, Segment, Tone};
use crate::ipc::rpc;
use crate::keyboard::{self, KeyAction, KeyInput};
use crate::model::*;
use crate::render::software::SoftwareRenderer;
use crate::render::{GridShaper, Layer, Op, Rect, Scene, parse_color};
use anyhow::{Context as _, Result};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
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
    /// A pane is not reading its input (`None`: no pane is stuck).
    Stuck(Option<(u64, usize)>),
    /// A command failed; ignored when it addressed a daemon that was replaced.
    CommandError {
        epoch: u64,
        message: String,
    },
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
    let command_proxy = proxy.clone();
    let crow_proxy = proxy.clone();
    // Input lanes per pane, a structural lane and a coalesced lane (see
    // lanes.rs): a pane that does not read its input never holds up others.
    let dispatcher = Arc::new(crate::lanes::Dispatcher::new(
        running.clone(),
        crate::ipc::rpc_patient,
        |error| {
            // Never reached a live daemon (full accept backlog, busy): the
            // same bytes are sent again. A pane closed on purpose (or its
            // program exited) drops what waited for it quietly.
            if crate::link::not_sent(error) && crate::ipc::daemon_alive() {
                crate::lanes::Outcome::Retry
            } else if crate::link::pane_gone(error) {
                crate::lanes::Outcome::PaneGone
            } else {
                crate::lanes::Outcome::Failed
            }
        },
        move |event| {
            let event = match event {
                crate::lanes::Event::Reply {
                    response,
                    activate_created,
                } => UiEvent::Reply {
                    response,
                    activate_created,
                },
                crate::lanes::Event::Error { epoch, message } => {
                    eprintln!("crow-term: {message}");
                    UiEvent::CommandError { epoch, message }
                }
                crate::lanes::Event::Stuck(stuck) => UiEvent::Stuck(stuck),
            };
            command_proxy.send_event(event).is_ok()
        },
    ));
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
                                        // Keep looking: when the user starts crow-term
                                        // again, this window reattaches to it.
                                        thread::sleep(Duration::from_secs(2));
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
            {
                // Guarded like every pane-addressed request: never the cwd of a
                // restarted daemon's pane that reuses this id.
                let dispatcher = dispatcher.clone();
                move |pane| match crate::lanes::guard(
                    Request::PaneCwd { pane },
                    dispatcher.instance(),
                )
                .map(|request| rpc(&request))
                {
                    Some(Ok(Response::Cwd(cwd))) => Ok(cwd),
                    _ => Err(()),
                }
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
        dispatcher.clone(),
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
    // Lanes observe the shutdown within about a second even with a frozen
    // daemon. The watch thread is left to end on its own (its long poll must
    // not delay closing the window).
    dispatcher.shutdown();
    drop(watch_worker);
    result.context("native event loop")?;
    if let Some(error) = fatal_error {
        anyhow::bail!(error);
    }
    Ok(())
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
/// The crow agent list as a layer in panel-local coordinates (cached by the caller).
// Layout inputs are deliberately explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn render_panel(
    shaper: &mut GridShaper,
    config: &Config,
    lines: &[Vec<Segment>],
    panel: Rect,
    scale: f64,
    chrome: &Chrome,
    light: bool,
    key: u64,
) -> Layer {
    let h = panel.h as usize;
    let local = Rect {
        x: 0,
        y: 0,
        w: panel.w,
        h: panel.h,
    };
    let mut ops = Vec::new();
    // Divider on the edge facing the panes.
    let divider = if config.crow.panel_position == "bottom" {
        Rect { h: 1, ..local }
    } else {
        Rect { w: 1, ..local }
    };
    ops.push(Op::Fill {
        rect: divider,
        color: chrome.border,
    });
    let pad = (PADDING * 1.4 * scale as f32) as i32;
    let line = (PANEL_LINE * scale as f32) as i32;
    let size = (12.0 * scale) as f32;
    let gap = (8.0 * scale) as i32;
    let mut y = pad;
    let capacity = ((h as i32 - pad - pad / 2) / line.max(1)).max(0) as usize;
    for segments in &crow::fit_lines(lines, capacity) {
        let mut x = pad;
        for segment in segments {
            let width = text_op(
                &mut ops,
                shaper,
                config,
                local,
                x,
                y,
                &segment.text,
                tone_color(segment.tone, chrome, light),
                size,
            );
            x += width.ceil() as i32 + gap;
        }
        y += line;
    }
    Layer {
        key,
        width: panel.w,
        height: panel.h,
        background: chrome.surface,
        ops,
    }
}

struct App {
    config: Config,
    session: String,
    shell: ShellSpec,
    window_id: u64,
    pane_id: u64,
    shared_window: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    dispatcher: Arc<crate::lanes::Dispatcher>,
    mailbox: Arc<ViewMailbox>,
    fatal_error: Option<String>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    shaper: GridShaper,
    software: SoftwareRenderer,
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
    /// A pane whose program is not reading its input: (pane, KiB waiting).
    stuck: Option<(u64, usize)>,
    clipboard: Option<arboard::Clipboard>,
    crow: Option<Arc<crow::Shared>>,
    crow_view: CrowView,
    crow_panel: bool,
    crow_pane: u64,
    panel_layer: Option<Arc<Layer>>,
    /// Offscreen tests draw without a window.
    #[cfg(test)]
    size_override: Option<PhysicalSize<u32>>,
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
        dispatcher: Arc<crate::lanes::Dispatcher>,
        shared_window: Arc<AtomicU64>,
        running: Arc<AtomicBool>,
        mailbox: Arc<ViewMailbox>,
        crow: Option<Arc<crow::Shared>>,
    ) -> Self {
        let shaper = GridShaper::new(&config, 1.0);
        let crow_panel = crow.is_some() && config.crow.panel;
        Self {
            config,
            session,
            shell,
            window_id,
            pane_id,
            shared_window,
            running,
            dispatcher,
            mailbox,
            fatal_error: None,
            window: None,
            surface: None,
            shaper,
            software: SoftwareRenderer::default(),
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
            stuck: None,
            clipboard: None,
            crow,
            crow_view: CrowView::default(),
            crow_panel,
            crow_pane: pane_id,
            panel_layer: None,
            #[cfg(test)]
            size_override: None,
            #[cfg(feature = "frame-dump")]
            frame_dump: crate::frame_dump::FrameDump::from_env(),
        }
    }
    /// Crow panel area, or `None` when hidden or the window is too small for it.
    fn panel_rect(&self) -> Option<Rect> {
        if !self.crow_panel {
            return None;
        }
        let size = self.window_size()?;
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
        self.software.invalidate();
        self.resize_panes();
        self.redraw();
    }
    fn send(&self, request: Request, activate_created: bool) {
        match request {
            Request::Input { pane, bytes } => self.dispatcher.input(pane, bytes),
            Request::Resize { pane, size, cell } => self.dispatcher.resize(pane, size, cell),
            Request::Scroll { pane, lines } => self.dispatcher.scroll(pane, lines),
            other => self.dispatcher.structural(other, activate_created),
        }
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
        let Some(size) = self.window_size() else {
            return Vec::new();
        };
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
            cols: (rect.w / self.shaper.cell_width).clamp(2, 400) as u16,
            rows: (rect.h / self.shaper.cell_height)
                .clamp(1, 200)
                .min(40_000 / (rect.w / self.shaper.cell_width).clamp(2, 400))
                as u16,
        }
    }
    fn resize_panes(&mut self) {
        for (pane, rect) in self.rects() {
            let size = self.rect_size(rect);
            if self.sent_sizes.get(&pane) != Some(&size) {
                self.sent_sizes.insert(pane, size);
                let cell = CellPixels {
                    width: self.shaper.cell_width.min(u32::from(u16::MAX)) as u16,
                    height: self.shaper.cell_height.min(u32::from(u16::MAX)) as u16,
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
                    rect.x + (screen.cursor.col as u32 * self.shaper.cell_width) as i32,
                    rect.y + (screen.cursor.row as u32 * self.shaper.cell_height) as i32,
                ),
                PhysicalSize::new(self.shaper.cell_width, self.shaper.cell_height),
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
        self.software.invalidate();
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
            Some(text) => {
                self.selection = None;
                let bytes = paste_bytes(&text, self.modes().bracketed_paste);
                if let Err(waiting) = self.dispatcher.paste(self.pane_id, bytes) {
                    self.status = format!(
                        "Paste refused: pane {} is not reading its input ({} KB already waiting)",
                        self.pane_id, waiting
                    );
                    self.status_expires = None;
                    self.redraw();
                }
            }
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
                        col: (((x - rect.x as f64) / self.shaper.cell_width as f64) as usize)
                            .min(size.cols.saturating_sub(1) as usize),
                        row: (((y - rect.y as f64) / self.shaper.cell_height as f64) as usize)
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
            // Motion reports (bit 32) supersede each other: coalesced in the
            // pane's lane, never ahead of keys or clicks.
            if code & 32 != 0 && !release {
                self.dispatcher.motion(pane, bytes);
            } else {
                self.send(Request::Input { pane, bytes }, false);
            }
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
            self.shaper.cell_width,
            self.shaper.cell_height,
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
                    self.shaper.cell_width,
                    self.shaper.cell_height,
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
            MouseScrollDelta::PixelDelta(position) => position.y / self.shaper.cell_height as f64,
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
        let Some(size) = self.window_size() else {
            return Vec::new();
        };
        let start = (190.0 * self.scale) as i32;
        let available = size.width.saturating_sub(start.max(0) as u32 + 16);
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
    /// Physical size of the drawable area.
    fn window_size(&self) -> Option<PhysicalSize<u32>> {
        #[cfg(test)]
        if let Some(size) = self.size_override {
            return Some(size);
        }
        Some(self.window.as_ref()?.inner_size())
    }
    fn draw(&mut self) -> Result<()> {
        let Some(size) = self.window_size() else {
            return Ok(());
        };
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return Ok(());
        };
        let Some(mut surface) = self.surface.take() else {
            return Ok(());
        };
        let result = (|| -> Result<()> {
            surface
                .resize(width, height)
                .map_err(|error| anyhow::anyhow!("resize native surface: {error}"))?;
            let mut frame = surface
                .buffer_mut()
                .map_err(|error| anyhow::anyhow!("native framebuffer: {error}"))?;
            self.paint(&mut frame, size);
            #[cfg(feature = "frame-dump")]
            self.dump_frame(&frame, size);
            frame
                .present()
                .map_err(|error| anyhow::anyhow!("present native surface: {error}"))
        })();
        self.surface = Some(surface);
        result
    }
    /// Compose the frame and draw it with the software backend.
    fn paint(&mut self, frame: &mut [u32], size: PhysicalSize<u32>) {
        let rects = self.rects();
        let panel = self.panel_rect().map(|rect| (rect, self.panel_layer(rect)));
        let parts = FrameParts {
            config: &self.config,
            session: &self.session,
            window_id: self.window_id,
            pane_id: self.pane_id,
            scale: self.scale,
            view: self.view.as_ref(),
            selection: self.selection.as_ref(),
            ime: &self.ime,
            status: &self.status,
            prefix: self.prefix,
            bell_flash: self.bell_flash.as_ref(),
            crow_status: &self.crow_view.status,
            stuck: self.stuck,
            rects,
            tabs: self.tab_rects(),
            windows: self.windows(),
            panel,
        };
        let scene = compose(&parts, &mut self.shaper, size);
        self.software
            .render(&scene, &mut self.shaper, &self.config, frame);
        if let Some(shared) = &self.crow
            && self.crow_pane != self.pane_id
        {
            self.crow_pane = self.pane_id;
            shared.pane.store(self.pane_id, Ordering::Relaxed);
            shared.wake();
        }
    }
    /// The crow panel as a cached layer, reshaped only when its lines, size,
    /// scale or colors change.
    fn panel_layer(&mut self, panel: Rect) -> Arc<Layer> {
        let chrome = Chrome::new(&self.config);
        let light = light_background(&self.config);
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
        if let Some(layer) = &self.panel_layer
            && layer.key == key
        {
            return layer.clone();
        }
        let layer = Arc::new(render_panel(
            &mut self.shaper,
            &self.config,
            &self.crow_view.panel,
            panel,
            self.scale,
            &chrome,
            light,
            key,
        ));
        self.panel_layer = Some(layer.clone());
        layer
    }
    #[cfg(feature = "frame-dump")]
    fn dump_frame(&mut self, frame: &[u32], size: PhysicalSize<u32>) {
        if self.frame_dump.is_none() {
            return;
        }
        let panes = self
            .rects()
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
        let Some(dump) = &mut self.frame_dump else {
            return;
        };
        let meta = crate::frame_dump::FrameMeta {
            schema: "crow-frame/1",
            seq: dump.next_seq(),
            renderer: "software",
            width: size.width,
            height: size.height,
            scale: self.scale,
            cell_width: self.shaper.cell_width,
            cell_height: self.shaper.cell_height,
            revision: self.view.as_ref().map_or(0, |view| view.revision),
            session: self.session.clone(),
            window: self.window_id,
            pane: self.pane_id,
            panes,
            cursor,
            focused: self.focused,
            prefix: self.prefix,
            status: self.status.clone(),
            ime: self.ime.clone(),
            stuck_pane: self.stuck.map(|(pane, _)| pane),
        };
        if let Err(error) = dump.write(frame, size.width, size.height, &meta) {
            eprintln!(
                "crow-term: frame dump to {} failed: {error}",
                dump.dir().display()
            );
        }
    }
}

/// Everything a frame shows, borrowed from the app so composing a scene does
/// not copy terminal cells.
struct FrameParts<'a> {
    config: &'a Config,
    session: &'a str,
    window_id: u64,
    pane_id: u64,
    scale: f64,
    view: Option<&'a View>,
    selection: Option<&'a Selection>,
    ime: &'a str,
    status: &'a str,
    prefix: bool,
    bell_flash: Option<&'a (Instant, HashSet<u64>)>,
    crow_status: &'a [Segment],
    stuck: Option<(u64, usize)>,
    rects: Vec<(u64, Rect)>,
    tabs: Vec<(u64, Rect)>,
    windows: Vec<WindowInfo>,
    panel: Option<(Rect, Arc<Layer>)>,
}

/// Push shaped UI text and return its width.
// Text origin, clip, color and size are deliberately explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn text_op(
    ops: &mut Vec<Op<'_>>,
    shaper: &mut GridShaper,
    config: &Config,
    clip: Rect,
    x: i32,
    y: i32,
    text: &str,
    color: u32,
    size: f32,
) -> f32 {
    let (glyphs, width) = shaper.shape_text(config, x, y, text, color, size);
    ops.push(Op::Glyphs { clip, glyphs });
    width
}

/// Build the frame: chrome, panes, cursor, overlays, footer and crow panel,
/// in painting order.
fn compose<'a>(
    parts: &FrameParts<'a>,
    shaper: &mut GridShaper,
    size: PhysicalSize<u32>,
) -> Scene<'a> {
    let config = parts.config;
    let scale = parts.scale;
    let chrome = Chrome::new(config);
    let mut ops: Vec<Op<'a>> = Vec::new();
    let bounds = Rect {
        x: 0,
        y: 0,
        w: size.width,
        h: size.height,
    };
    let header_h = (HEADER as f64 * scale) as u32;
    ops.push(Op::Fill {
        rect: Rect {
            h: header_h,
            ..bounds
        },
        color: chrome.surface,
    });
    ops.push(Op::Fill {
        rect: Rect {
            x: 0,
            y: header_h.saturating_sub(1) as i32,
            w: size.width,
            h: 1,
        },
        color: chrome.border,
    });
    let label = format!("CROW / {}", parts.session);
    text_op(
        &mut ops,
        shaper,
        config,
        Rect {
            w: (180.0 * scale) as u32,
            h: header_h,
            ..bounds
        },
        (14.0 * scale) as i32,
        (13.0 * scale) as i32,
        &label,
        chrome.text,
        (14.0 * scale) as f32,
    );
    for &(id, rect) in &parts.tabs {
        let active = id == parts.window_id;
        if active {
            ops.push(Op::Fill {
                rect,
                color: chrome.active,
            });
            ops.push(Op::Fill {
                rect: Rect {
                    x: rect.x,
                    y: rect.h.saturating_sub(2) as i32,
                    w: rect.w,
                    h: 2,
                },
                color: chrome.text,
            });
        }
        if let Some(info) = parts.windows.iter().find(|info| info.id == id) {
            let index = parts
                .windows
                .iter()
                .position(|window| window.id == id)
                .unwrap_or(0)
                + 1;
            let text = format!("{index}  {}", info.name);
            text_op(
                &mut ops,
                shaper,
                config,
                rect.intersection(bounds),
                rect.x + (10.0 * scale) as i32,
                (13.0 * scale) as i32,
                &text,
                if active { chrome.text } else { chrome.muted },
                (14.0 * scale) as f32,
            );
        }
    }
    let live_panes = parts.rects.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let screens = parts
        .view
        .map(|view| view.screens.as_slice())
        .unwrap_or(&[]);
    let cw = shaper.cell_width;
    let ch = shaper.cell_height;
    for &(pane, rect) in &parts.rects {
        let clip = rect.intersection(bounds);
        let Some((_, screen)) = screens.iter().find(|(id, _)| *id == pane) else {
            text_op(
                &mut ops,
                shaper,
                config,
                clip,
                rect.x,
                rect.y,
                "Connecting to persistent session…",
                chrome.muted,
                shaper.font_size,
            );
            continue;
        };
        for (row, cells) in screen.cells.iter().enumerate().take((rect.h / ch) as usize) {
            let mut row_cells = Cow::Borrowed(cells.as_slice());
            if let Some(selection) = parts.selection
                && selection.pane == pane
                && (0..cells.len()).any(|col| selection.contains(row, col))
            {
                let color = parse_color(&config.selection, 0xdadada);
                for (col, cell) in row_cells.to_mut().iter_mut().enumerate() {
                    if selection.contains(row, col) {
                        cell.bg =
                            ColorSpec::Rgb((color >> 16) as u8, (color >> 8) as u8, color as u8);
                    }
                }
            }
            ops.push(Op::Row {
                pane,
                row,
                cells: row_cells,
                x: rect.x,
                y: rect.y + (row as u32 * ch) as i32,
                clip,
            });
        }
        if pane == parts.pane_id
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
            let color = parse_color(&config.cursor, 0xffffff);
            let shape = screen.cursor.shape.to_ascii_lowercase();
            if shape.contains("beam") {
                cursor.w = (scale.ceil() as u32).max(1);
                ops.push(Op::Fill {
                    rect: cursor.intersection(clip),
                    color,
                });
            } else if shape.contains("underline") {
                cursor.y += ch.saturating_sub(2) as i32;
                cursor.h = 2;
                ops.push(Op::Fill {
                    rect: cursor.intersection(clip),
                    color,
                });
            } else if shape.contains("hollow") {
                let border = cursor.intersection(clip);
                for rect in [
                    Rect { h: 1, ..border },
                    Rect {
                        y: border.y + border.h.saturating_sub(1) as i32,
                        h: 1,
                        ..border
                    },
                    Rect { w: 1, ..border },
                    Rect {
                        x: border.x + border.w.saturating_sub(1) as i32,
                        w: 1,
                        ..border
                    },
                ] {
                    ops.push(Op::Fill { rect, color });
                }
            } else {
                ops.push(Op::Invert {
                    rect: cursor.intersection(clip),
                });
            }
            if !parts.ime.is_empty() {
                let composition = Rect {
                    x: cursor.x,
                    y: cursor.y,
                    w: ((parts.ime.chars().count() + 2) as u32 * cw).min(clip.w),
                    h: ch,
                }
                .intersection(clip);
                ops.push(Op::Fill {
                    rect: composition,
                    color: chrome.active,
                });
                text_op(
                    &mut ops,
                    shaper,
                    config,
                    composition,
                    composition.x,
                    composition.y,
                    parts.ime,
                    chrome.text,
                    shaper.font_size,
                );
            }
        }
        if pane == parts.pane_id && live_panes.len() > 1 {
            ops.push(Op::Fill {
                rect: Rect {
                    x: rect.x,
                    y: rect.y.saturating_sub(2),
                    w: rect.w,
                    h: 1,
                }
                .intersection(bounds),
                color: chrome.border,
            });
        }
    }
    if let Some((until, panes)) = parts.bell_flash
        && Instant::now() < *until
    {
        let thickness = (2.0 * scale).ceil().max(1.0) as u32;
        for &(pane, rect) in &parts.rects {
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
                ops.push(Op::Fill {
                    rect: edge.intersection(bounds),
                    color: chrome.text,
                });
            }
        }
    }
    let footer_y = size.height.saturating_sub((FOOTER as f64 * scale) as u32) as i32;
    let footer = Rect {
        x: 0,
        y: footer_y,
        w: size.width,
        h: size.height.saturating_sub(footer_y as u32),
    };
    ops.push(Op::Fill {
        rect: footer,
        color: chrome.surface,
    });
    let hint = if parts.prefix {
        "PREFIX  c window   % / \" split   n/p tabs   h/j/k/l panes   a agents   d detach   x close"
            .to_owned()
    } else if !parts.status.is_empty() {
        parts.status.to_owned()
    } else {
        format!(
            "{}  ·  session {}  ·  pane {}  ·  closing this window detaches",
            if config.prefix.eq_ignore_ascii_case("none") {
                "tmux passthrough".to_owned()
            } else {
                format!("{} prefix", config.prefix)
            },
            parts.session,
            parts.pane_id
        )
    };
    let light = light_background(config);
    // Footer order: prefix help alone; otherwise status/error messages first (they
    // must stay visible), then the crow segments, then the default session hint.
    let hint_color = if parts.prefix {
        chrome.text
    } else {
        chrome.muted
    };
    let mut footer_items: Vec<(&str, u32)> = Vec::new();
    // Shown whatever else the footer says: input is waiting, not lost.
    let stuck = parts.stuck.map(|(pane, kib)| {
        format!("pane {pane}: the program is not reading input — {kib} KB waiting")
    });
    if let Some(stuck) = &stuck {
        footer_items.push((stuck, chrome.text));
    }
    let message_first = parts.prefix || !parts.status.is_empty();
    if message_first {
        footer_items.push((&hint, hint_color));
    }
    if !parts.prefix {
        footer_items.extend(parts.crow_status.iter().map(|segment| {
            (
                segment.text.as_str(),
                tone_color(segment.tone, &chrome, light),
            )
        }));
    }
    if !message_first {
        footer_items.push((&hint, hint_color));
    }
    let mut footer_x = (12.0 * scale) as i32;
    let gap = (16.0 * scale) as i32;
    for (text, color) in footer_items {
        if footer_x >= footer.w as i32 {
            break;
        }
        let width = text_op(
            &mut ops,
            shaper,
            config,
            footer,
            footer_x,
            footer_y + (4.0 * scale) as i32,
            text,
            color,
            (11.0 * scale) as f32,
        );
        footer_x += width.ceil() as i32 + gap;
    }
    if let Some((rect, layer)) = &parts.panel {
        ops.push(Op::Layer {
            rect: *rect,
            clip: bounds,
            layer: layer.clone(),
        });
    }
    Scene {
        width: size.width,
        height: size.height,
        background: parse_color(&config.background, 0x111111),
        live_panes,
        ops,
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
            self.shaper.rescale(&self.config, self.scale);
            self.software.invalidate();
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
                self.dispatcher.set_instance(view.instance);
                // Lanes of panes that no longer exist (closed, exited) end.
                if view
                    .sessions
                    .iter()
                    .any(|session| session.name == self.session)
                {
                    let live: Vec<u64> = view
                        .sessions
                        .iter()
                        .flat_map(|session| &session.windows)
                        .flat_map(|window| window.layout.panes())
                        .collect();
                    self.dispatcher.retain(&live);
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
            UiEvent::Stuck(stuck) => {
                self.stuck = stuck;
                self.redraw();
            }
            UiEvent::CommandError { epoch, message } => {
                // Pane ids of a replaced daemon mean nothing to the new one.
                if epoch == self.dispatcher.epoch() {
                    self.status = message;
                    self.status_expires = None;
                    self.redraw();
                }
            }
            UiEvent::Link { status, expires } => {
                self.status = status.unwrap_or_default();
                self.status_expires = expires.then(|| Instant::now() + Duration::from_secs(3));
                self.redraw();
            }
            UiEvent::Reattached { window, pane } => {
                eprintln!(
                    "crow-term: reattached to window {window}, pane {pane} (was window {}, pane {})",
                    self.window_id, self.pane_id
                );
                self.dispatcher.reattached();
                // Switch ids directly: the old ones belong to the replaced
                // daemon and must not be resized or focused in the new one.
                self.view = None;
                self.sent_sizes.clear();
                self.window_id = window;
                self.shared_window.store(window, Ordering::Relaxed);
                self.pane_id = pane;
                self.pending_focus_in = None;
                self.selection = None;
                self.software.invalidate();
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
                self.shaper.rescale(&self.config, self.scale);
                self.software.invalidate();
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
#[path = "ui_fixture.rs"]
mod fixture;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::resolve_color;
    use crate::render::shaper::{column_for_byte, row_text};
    use crate::render::software::{blit, fill};
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
                instance: 1,
                revision: 2,
                sessions: vec![],
                screens: vec![(1, screen)]
            }
        ));
        assert!(!mailbox.push(
            1,
            View {
                instance: 1,
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
                instance: 1,
                revision: 3,
                sessions: vec![],
                screens: vec![]
            }
        ));
        assert_eq!(mailbox.take().unwrap().0, 2);
    }
}
