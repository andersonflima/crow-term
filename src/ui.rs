//! Native, event-driven software frontend. PTYs live in the mux server, not this window.
//! The two RPC workers keep long polls and command I/O off the native event thread.
use crate::config::Config;
use crate::config::RendererMode;
use crate::crow::{self, CrowView, Segment, Tone};
use crate::ipc::rpc;
use crate::keyboard::{self, KeyAction, KeyInput};
use crate::model::*;
use crate::render::gpu::GpuRenderer;
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
    crate::render::is_light(parse_color(&config.background, 0xffffff))
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
        (Tone::Warn, true) => 0x985a00,
        (Tone::Warn, false) => 0xe6b450,
        (Tone::Alert, true) => 0xc0392b,
        (Tone::Alert, false) => 0xf07167,
    }
}
/// Tab-context tones painted with the crow palette.
fn context_tone(tone: crate::tab_context::Tone) -> Tone {
    match tone {
        crate::tab_context::Tone::Plain => Tone::Plain,
        crate::tab_context::Tone::On => Tone::On,
        crate::tab_context::Tone::Accent => Tone::Accent,
        crate::tab_context::Tone::Warn => Tone::Warn,
        crate::tab_context::Tone::Alert => Tone::Alert,
        crate::tab_context::Tone::Muted => Tone::Muted,
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

/// Compose the scene of `$app` at `$size`. A macro rather than a method so the
/// scene can borrow the view while the shaper and backends are borrowed
/// mutably (disjoint fields).
macro_rules! scene_of {
    ($app:ident, $size:expr) => {{
        let size = $size;
        let rects = $app.rects();
        let panel = $app.panel_rect().map(|rect| (rect, $app.panel_layer(rect)));
        let (tab_labels, context) = $app.prepare_context();
        let parts = FrameParts {
            config: &$app.config,
            session: &$app.session,
            window_id: $app.window_id,
            pane_id: $app.pane_id,
            scale: $app.scale,
            view: $app.view.as_ref(),
            selection: $app.selection.as_ref(),
            hover_link: $app.hover_link.as_ref(),
            ime: &$app.ime,
            status: &$app.status,
            prefix: $app.prefix,
            bell_flash: $app.bell_flash.as_ref(),
            stuck: $app.stuck,
            rects,
            tabs: $app.tab_rects(),
            windows: $app.windows(),
            tab_labels,
            context,
            panel,
        };
        compose(&parts, &mut $app.shaper, size, &mut $app.drawn)
    }};
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
    /// Effective renderer choice (already resolved from CLI/env/config).
    renderer_mode: RendererMode,
    /// Present when frames are drawn by the GPU; otherwise `surface` is used.
    gpu: Option<GpuRenderer>,
    /// GPU frame failures since the last presented frame.
    gpu_failures: u32,
    /// Skipped GPU frame (occluded, timeout, surface lost): when to draw
    /// again, and the current backoff.
    retry_due: Option<Instant>,
    retry_delay: Duration,
    /// `CROW_TERM_FRAME_STATS=1`: draw+present durations, printed on exit.
    frame_stats: Option<FrameStats>,
    /// The last `draw` presented a frame (frame stats skip empty draws).
    presented: bool,
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
    /// The link under the pointer while the link modifier is held.
    hover_link: Option<(u64, crate::hyperlinks::Hit)>,
    /// A modifier+click opened (or asked about) a link: its release is ours.
    link_press: bool,
    /// A link waiting for a deliberate second modifier+click (file:, a
    /// misleading OSC 8 text).
    link_confirm: crate::hyperlinks::Confirm,
    /// The status text showing the hovered link's target, to clear it after.
    hover_status: Option<String>,
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
    /// Tab labels and the footer's pane context (see tab_context.rs).
    tab_context: crate::tab_context::State,
    /// What the last frame showed: shortened footer parts and tabs (for a
    /// click or hover) and the drawn texts (frame dumps).
    drawn: Drawn,
    /// A debounced tab label is due to change then.
    label_due: Option<Instant>,
    /// Shortened tab labels by (index and full label, room in px, scale):
    /// shaping runs only when one of them changes.
    label_fit: HashMap<(String, u32, u64), String>,
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
        let renderer_mode = config.renderer;
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
            renderer_mode,
            gpu: None,
            gpu_failures: 0,
            retry_due: None,
            retry_delay: FRAME_RETRY_MIN,
            frame_stats: FrameStats::from_env(),
            presented: false,
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
            hover_link: None,
            link_press: false,
            link_confirm: Default::default(),
            hover_status: None,
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
            tab_context: Default::default(),
            drawn: Drawn::default(),
            label_due: None,
            label_fit: HashMap::new(),
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
        self.invalidate_rows();
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
        self.invalidate_rows();
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
        match clipboard_shortcut(self.mods, &c) {
            Some(ClipboardShortcut::Copy) => return self.copy_selection(),
            Some(ClipboardShortcut::Paste) => return self.paste(),
            None => {}
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
        match self.clipboard.as_mut().and_then(crate::clip::read) {
            Some(content) => self.paste_content(content),
            None => {
                self.status = "Clipboard unavailable or empty".into();
                self.redraw();
            }
        }
    }
    /// Files become quoted paths, an image the path of a PNG written for it,
    /// text a (bracketed) paste.
    fn paste_content(&mut self, content: crate::clip::Content) {
        use crate::clip::Content;
        let text = match content {
            Content::Text(text) => Ok(text),
            Content::Files(paths) => crate::clip::paths_text(&paths),
            Content::Image {
                width,
                height,
                rgba,
            } => {
                match crate::clip::image_dir()
                    .and_then(|dir| crate::clip::save_png(&dir, width, height, &rgba))
                {
                    Ok(path) => crate::clip::paths_text(&[path]),
                    Err(error) => {
                        self.status = format!("Pasted image not saved: {error:#}");
                        self.status_expires = None;
                        self.redraw();
                        return;
                    }
                }
            }
        };
        let text = match text {
            Ok(text) => text,
            Err(why) => {
                self.status = format!("Not pasted: {why}");
                self.status_expires = Some(Instant::now() + Duration::from_secs(8));
                self.redraw();
                return;
            }
        };
        if text.is_empty() {
            return;
        }
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
    /// Cmd (macOS) or Ctrl (Linux, Windows) held: clicks open links.
    fn link_modifier(&self) -> bool {
        link_modifier_held(self.mods)
    }
    fn link_under_pointer(&self) -> Option<(u64, crate::hyperlinks::Hit)> {
        let (pane, point) = self.point_at(self.cursor.x, self.cursor.y)?;
        let hit = crate::hyperlinks::link_at(self.screen(pane)?, point.row, point.col)?;
        Some((pane, hit))
    }
    /// Underline the link under the pointer and show the hand cursor while
    /// the link modifier is held.
    fn update_hover(&mut self) {
        let hover = if self.link_modifier() && self.mouse_origin.is_none() && !self.dragging {
            self.link_under_pointer()
        } else {
            None
        };
        if hover != self.hover_link {
            if let Some(window) = &self.window {
                window.set_cursor(if hover.is_some() {
                    winit::window::CursorIcon::Pointer
                } else {
                    winit::window::CursorIcon::Default
                });
            }
            // The real target, before any click: an OSC 8 link's text can
            // say anything.
            if let Some(previous) = self.hover_status.take()
                && self.status == previous
            {
                self.status.clear();
            }
            if let Some((_, hit)) = &hover {
                let text = format!("Link: {}", crate::hyperlinks::display(&hit.url));
                self.status = text.clone();
                self.status_expires = None;
                self.hover_status = Some(text);
            }
            self.hover_link = hover;
            self.redraw();
        }
    }
    /// Modifier+click on a link: open it (http, https, mailto), ask first
    /// (file:, an OSC 8 text naming another host: a deliberate second click
    /// confirms) or refuse.
    fn click_link(&mut self, hit: crate::hyperlinks::Hit) {
        use crate::hyperlinks::{Opened, Policy, decide, display};
        let shown = display(&hit.url);
        let decision = decide(&hit);
        self.status = match decision {
            Policy::Refuse => format!("Link not opened (not allowed): {shown}"),
            Policy::Confirm if !self.link_confirm.click(&hit.url, Instant::now()) => format!(
                "Open {shown}{}? {}+click again to confirm",
                if hit.osc8 && crate::hyperlinks::policy(&hit.url) == Policy::Open {
                    format!(" (the text shows {:?})", hit.text.trim())
                } else {
                    String::new()
                },
                if cfg!(target_os = "macos") {
                    "Cmd"
                } else {
                    "Ctrl"
                }
            ),
            Policy::Open | Policy::Confirm => match crate::hyperlinks::open(&hit.url) {
                Ok(Opened::Revealed(path)) => {
                    format!("Shown in the file manager, not run: {}", path.display())
                }
                Ok(_) => format!("Opened {shown}"),
                Err(error) => format!("Link not opened: {error:#}"),
            },
        };
        self.hover_status = None;
        self.status_expires = Some(Instant::now() + Duration::from_secs(5));
        self.redraw();
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
    /// The full text of a shortened footer part or tab label under (x, y).
    fn context_text_at(&self, x: f64, y: f64) -> Option<String> {
        let size = self.window_size()?;
        let footer_y = size
            .height
            .saturating_sub((FOOTER as f64 * self.scale) as u32) as f64;
        if y >= footer_y {
            let x = x as i32;
            return self
                .drawn
                .footer_full
                .iter()
                .find(|(from, to, _)| (*from..=*to).contains(&x))
                .map(|(_, _, full)| full.clone());
        }
        if y < HEADER as f64 * self.scale {
            let (id, _) = self
                .tab_rects()
                .into_iter()
                .find(|(_, rect)| rect.contains(x, y))?;
            return self
                .drawn
                .tab_full
                .iter()
                .find(|(w, _)| *w == id)
                .map(|(_, full)| full.clone());
        }
        None
    }
    fn mouse_move(&mut self) {
        // Hovering a shortened tab label shows it whole in the status line.
        if self.cursor.y < HEADER as f64 * self.scale
            && let Some(full) = self.context_text_at(self.cursor.x, self.cursor.y)
            && self.status != full
        {
            self.status = full;
            self.status_expires = Some(Instant::now() + Duration::from_secs(3));
            self.redraw();
        }
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
        // A shortened footer part shows its full text when clicked.
        if state == ElementState::Pressed
            && button == MouseButton::Left
            && self.cursor.y >= HEADER as f64 * self.scale
            && let Some(full) = self.context_text_at(self.cursor.x, self.cursor.y)
        {
            self.status = full;
            self.status_expires = Some(Instant::now() + Duration::from_secs(6));
            self.redraw();
            return;
        }
        // Modifier+click on a link is the terminal's, never the program's
        // (press and release alike).
        if button == MouseButton::Left {
            if state == ElementState::Released && std::mem::take(&mut self.link_press) {
                self.link_confirm.released();
                return;
            }
            if state == ElementState::Pressed
                && self.link_modifier()
                && self.mouse_origin.is_none()
                && let Some((pane, hit)) = self.link_under_pointer()
            {
                self.link_press = true;
                self.focus_pane(pane);
                self.click_link(hit);
                return;
            }
        }
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
            (available / windows.len() as u32).min((240.0 * self.scale) as u32)
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
    fn invalidate_rows(&mut self) {
        self.software.invalidate();
        if let Some(gpu) = &mut self.gpu {
            gpu.invalidate();
        }
    }
    /// Create the frame backend for `window` according to `renderer_mode`.
    /// `auto` falls back to software on any GPU error (reason in the status
    /// line and on stderr); `gpu` reports the error.
    fn start_renderer(&mut self, window: Arc<Window>) -> Result<()> {
        if self.renderer_mode != RendererMode::Software {
            let allow_cpu = self.renderer_mode == RendererMode::Gpu;
            match GpuRenderer::for_window(window.clone(), allow_cpu) {
                Ok(gpu) => {
                    if std::env::var_os("CROW_TERM_RENDERER_LOG").is_some() {
                        eprintln!("crow-term: GPU renderer on {}", gpu.adapter());
                    }
                    self.gpu = Some(gpu);
                    return Ok(());
                }
                Err(error) if self.renderer_mode == RendererMode::Auto => {
                    self.status = format!("GPU renderer unavailable, using software: {error:#}");
                    eprintln!("crow-term: {}", self.status);
                }
                Err(error) => return Err(error.context("--renderer gpu")),
            }
        }
        self.start_software(window)
    }
    fn start_software(&mut self, window: Arc<Window>) -> Result<()> {
        self.gpu = None;
        let context = softbuffer::Context::new(window.clone())
            .map_err(|e| anyhow::anyhow!("native display: {e}"))?;
        let surface = softbuffer::Surface::new(&context, window)
            .map_err(|e| anyhow::anyhow!("native surface: {e}"))?;
        self.surface = Some(surface);
        Ok(())
    }
    fn draw(&mut self) -> Result<()> {
        let started = Instant::now();
        self.presented = false;
        let result = self.draw_frame();
        if let Some(stats) = &mut self.frame_stats
            && self.presented
        {
            stats.record(
                started,
                if self.gpu.is_some() {
                    "gpu"
                } else {
                    "software"
                },
            );
        }
        result
    }
    fn draw_frame(&mut self) -> Result<()> {
        let Some(size) = self.window_size() else {
            return Ok(());
        };
        if self.gpu.is_some() {
            return self.draw_gpu(size);
        }
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
            self.dump_frame(&frame, size, "software");
            frame
                .present()
                .map_err(|error| anyhow::anyhow!("present native surface: {error}"))
        })();
        self.surface = Some(surface);
        self.presented = result.is_ok();
        result
    }
    /// GPU frame. Failures go through [`Self::recover_from_gpu_error`];
    /// skipped frames through [`Self::note_gpu_frame`].
    fn draw_gpu(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let scene = scene_of!(self, size);
        let Some(gpu) = self.gpu.as_mut() else {
            return Ok(());
        };
        #[cfg(not(feature = "frame-dump"))]
        let result = gpu.render(&scene, &mut self.shaper, &self.config);
        // Dump the presented texture when it can be copied, else an identical
        // offscreen render of the same scene.
        #[cfg(feature = "frame-dump")]
        let (result, dumped) = if self.frame_dump.is_some() {
            match gpu.render_and_capture(&scene, &mut self.shaper, &self.config) {
                Ok((true, Some(pixels))) => (Ok(true), Some(pixels)),
                Ok((true, None)) => (
                    Ok(true),
                    // Surface not copyable here: dump the same scene offscreen.
                    gpu.render_offscreen(&scene, &mut self.shaper, &self.config)
                        .ok(),
                ),
                other => (other.map(|(presented, _)| presented), None),
            }
        } else {
            (gpu.render(&scene, &mut self.shaper, &self.config), None)
        };
        drop(scene);
        self.after_paint();
        if let Some(gpu) = &mut self.gpu {
            let dropped = gpu.take_dropped_glyphs();
            if dropped > 0 {
                self.status =
                    format!("GPU glyph atlas full: {dropped} glyphs not drawn in this frame");
            }
        }
        match result {
            Ok(presented) => {
                self.presented = presented;
                #[cfg(feature = "frame-dump")]
                if let Some(pixels) = dumped {
                    self.dump_frame(&pixels, size, "gpu");
                }
                self.note_gpu_frame(presented);
                Ok(())
            }
            Err(error) => {
                let Some(window) = self.window.clone() else {
                    return Err(error);
                };
                let gpu_window = window.clone();
                self.recover_from_gpu_error(
                    error,
                    move || GpuRenderer::for_window(gpu_window, true),
                    move |app| app.start_software(window),
                )
            }
        }
    }
    /// Bookkeeping after a GPU frame that did not fail: a presented frame
    /// clears the failure count and the retry backoff; a skipped one
    /// (occluded, timeout, surface lost twice) schedules another attempt with
    /// a growing delay (50 ms up to 1 s), so stale content is replaced
    /// without busy-looping while the window stays hidden.
    fn note_gpu_frame(&mut self, presented: bool) {
        if presented {
            self.gpu_failures = 0;
            self.retry_due = None;
            self.retry_delay = FRAME_RETRY_MIN;
        } else {
            self.retry_due = Some(Instant::now() + self.retry_delay);
            self.retry_delay = (self.retry_delay * 2).min(FRAME_RETRY_MAX);
        }
    }
    /// A GPU frame failed. `auto` switches to the software framebuffer;
    /// `gpu` gets one new device and, if frames fail again before one is
    /// presented, ends with the error (no device-recreation loop). If the
    /// replacement backend cannot start, the session ends with the error
    /// instead of a frozen window.
    fn recover_from_gpu_error(
        &mut self,
        error: anyhow::Error,
        new_gpu: impl FnOnce() -> Result<GpuRenderer>,
        new_software: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        self.gpu = None;
        self.gpu_failures += 1;
        match gpu_failure_action(self.renderer_mode, self.gpu_failures) {
            GpuFailure::RecreateDevice => match new_gpu() {
                Ok(gpu) => {
                    self.gpu = Some(gpu);
                    self.status = format!("GPU device recreated after: {error:#}");
                    eprintln!("crow-term: {}", self.status);
                }
                Err(again) => {
                    self.fatal_error = Some(format!(
                        "GPU renderer failed: {error:#}; restart failed: {again:#}"
                    ));
                    return Err(again);
                }
            },
            GpuFailure::Fatal => {
                self.fatal_error = Some(format!(
                    "GPU renderer failed again after a new device: {error:#}"
                ));
                return Err(error);
            }
            GpuFailure::SwitchToSoftware => {
                if let Err(software) = new_software(self) {
                    self.fatal_error = Some(format!(
                        "GPU renderer failed ({error:#}) and the software renderer could not start: {software:#}"
                    ));
                    return Err(software);
                }
                self.status = format!("GPU renderer failed, switched to software: {error:#}");
                eprintln!("crow-term: {}", self.status);
            }
        }
        self.invalidate_rows();
        self.redraw();
        Ok(())
    }
    /// Compose the frame and draw it with the software backend.
    fn paint(&mut self, frame: &mut [u32], size: PhysicalSize<u32>) {
        let scene = scene_of!(self, size);
        self.software
            .render(&scene, &mut self.shaper, &self.config, frame);
        drop(scene);
        self.after_paint();
    }
    /// Tab labels and the footer context of the next frame (tab_context.rs);
    /// `compose` paints them and records what it drew in `self.drawn`.
    #[allow(clippy::type_complexity)]
    fn prepare_context(
        &mut self,
    ) -> (
        Vec<(u64, String)>,
        Option<(
            Vec<crate::tab_context::Segment>,
            Option<crate::tab_context::Segment>,
        )>,
    ) {
        let now = Instant::now();
        let tabs = self.tab_rects();
        let windows = self.windows();
        let mut tab_labels = Vec::new();
        let mut tab_full = Vec::new();
        // Real glyph widths, as `compose` will draw them.
        let tab_size = (14.0 * self.scale) as f32;
        for &(id, rect) in &tabs {
            let Some(info) = windows.iter().find(|info| info.id == id) else {
                continue;
            };
            let full = if self.config.tab_labels {
                let pane = if id == self.window_id {
                    Some(self.pane_id)
                } else {
                    info.layout.panes().first().copied()
                };
                self.tab_context.tab_label(
                    id,
                    &info.name,
                    pane,
                    &self.crow_view.context.agents,
                    &self.shell.program,
                    now,
                )
            } else {
                crate::tab_context::sanitize(&info.name)
            };
            let index = windows.iter().position(|w| w.id == id).unwrap_or(0) + 1;
            let room = rect.w as f32 - 20.0 * self.scale as f32;
            let key = (
                format!("{index}\0{full}"),
                room.max(0.0) as u32,
                self.scale.to_bits(),
            );
            let label = match self.label_fit.get(&key) {
                Some(label) => label.clone(),
                None => {
                    let (config, shaper) = (&self.config, &mut self.shaper);
                    let label = crate::tab_context::shorten(
                        &full,
                        crate::tab_context::Shorten::Middle,
                        |text| {
                            shaper
                                .shape_text(config, 0, 0, &format!("{index}  {text}"), 0, tab_size)
                                .1
                                <= room
                        },
                    )
                    .unwrap_or_default();
                    if self.label_fit.len() >= 256 {
                        self.label_fit.clear();
                    }
                    self.label_fit.insert(key, label.clone());
                    label
                }
            };
            if label != full {
                tab_full.push((id, full));
            }
            tab_labels.push((id, label));
        }
        let context = (!self.prefix).then(|| {
            let sessions = self.view.as_ref().map_or(0, |v| v.sessions.len());
            let session = (sessions > 1).then_some(self.session.as_str());
            let home = std::env::var("HOME").ok();
            self.tab_context.footer(
                self.pane_id,
                &self.crow_view.context,
                session,
                home.as_deref(),
            )
        });
        self.label_due = self.tab_context.next_change(now).map(|after| now + after);
        self.drawn = Drawn {
            tab_full,
            ..Drawn::default()
        };
        (tab_labels, context)
    }
    /// Effects of having drawn a frame for the focused pane.
    fn after_paint(&mut self) {
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
    fn dump_frame(&mut self, frame: &[u32], size: PhysicalSize<u32>, renderer: &'static str) {
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
            renderer,
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
            footer: self.drawn.footer.clone(),
            tabs: self.drawn.tabs.clone(),
            hover_link: self.hover_link.as_ref().map(|(_, hit)| hit.url.clone()),
        };
        if let Err(error) = dump.write(frame, size.width, size.height, &meta) {
            eprintln!(
                "crow-term: frame dump to {} failed: {error}",
                dump.dir().display()
            );
        }
    }
}

/// First and longest delay before retrying a skipped GPU frame.
const FRAME_RETRY_MIN: Duration = Duration::from_millis(50);
const FRAME_RETRY_MAX: Duration = Duration::from_secs(1);

/// What to do after a GPU frame failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GpuFailure {
    RecreateDevice,
    SwitchToSoftware,
    Fatal,
}
/// `failures` counts failed frames since the last presented one (>= 1).
fn gpu_failure_action(mode: RendererMode, failures: u32) -> GpuFailure {
    match mode {
        RendererMode::Gpu if failures <= 1 => GpuFailure::RecreateDevice,
        RendererMode::Gpu => GpuFailure::Fatal,
        RendererMode::Auto | RendererMode::Software => GpuFailure::SwitchToSoftware,
    }
}

/// Opt-in frame timing (`CROW_TERM_FRAME_STATS`): wall time of each
/// draw+present and the time from GUI start to the first frame. Printed on
/// exit; when the variable names a file, it is also rewritten every 60 frames
/// (benchmarks that stop the GUI with a signal).
struct FrameStats {
    file: Option<std::path::PathBuf>,
    started: Instant,
    first_frame: Option<Duration>,
    renderer: &'static str,
    samples: Vec<Duration>,
}
impl FrameStats {
    fn from_env() -> Option<Self> {
        let value = std::env::var_os("CROW_TERM_FRAME_STATS")?;
        // Unset, empty or "0": off; "1": stderr on exit; anything else: a file.
        if value.is_empty() || value == "0" {
            return None;
        }
        Some(Self {
            file: (value != "1").then(|| value.into()),
            started: Instant::now(),
            first_frame: None,
            renderer: "software",
            samples: Vec::new(),
        })
    }
    fn record(&mut self, frame_started: Instant, renderer: &'static str) {
        self.renderer = renderer;
        self.first_frame
            .get_or_insert_with(|| self.started.elapsed());
        // Bounded: a long session keeps the latest 100k frames.
        if self.samples.len() >= 100_000 {
            self.samples.drain(..50_000);
        }
        self.samples.push(frame_started.elapsed());
        if let Some(file) = &self.file
            && (self.samples.len() == 1 || self.samples.len().is_multiple_of(60))
        {
            let _ = std::fs::write(file, self.summary() + "\n");
        }
    }
    fn summary(&self) -> String {
        let mut sorted = self.samples.clone();
        sorted.sort();
        let at = |p: f64| {
            sorted
                .get(((sorted.len().saturating_sub(1)) as f64 * p).round() as usize)
                .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
        };
        format!(
            "frame stats renderer={} frames={} first_frame_ms={:.1} p50_ms={:.2} p95_ms={:.2} p99_ms={:.2} max_ms={:.2}",
            self.renderer,
            sorted.len(),
            self.first_frame.map_or(0.0, |d| d.as_secs_f64() * 1000.0),
            at(0.5),
            at(0.95),
            at(0.99),
            at(1.0)
        )
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
    /// The link under the pointer while the link modifier is held (underlined).
    hover_link: Option<&'a (u64, crate::hyperlinks::Hit)>,
    ime: &'a str,
    status: &'a str,
    prefix: bool,
    bell_flash: Option<&'a (Instant, HashSet<u64>)>,
    stuck: Option<(u64, usize)>,
    rects: Vec<(u64, Rect)>,
    tabs: Vec<(u64, Rect)>,
    windows: Vec<WindowInfo>,
    /// Tab labels as they fit (tab_context.rs).
    tab_labels: Vec<(u64, String)>,
    /// Footer context of the focused pane and the global part (not while
    /// the prefix help is shown).
    context: Option<(
        Vec<crate::tab_context::Segment>,
        Option<crate::tab_context::Segment>,
    )>,
    panel: Option<(Rect, Arc<Layer>)>,
}

/// What a frame showed, for clicks/hover on shortened text and frame dumps.
#[derive(Default)]
struct Drawn {
    /// Shortened footer parts: (x from, x to, full text).
    footer_full: Vec<(i32, i32, String)>,
    /// Shortened tab labels: (window, full label).
    tab_full: Vec<(u64, String)>,
    footer: Vec<String>,
    tabs: Vec<String>,
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
    drawn: &mut Drawn,
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
            let label = parts
                .tab_labels
                .iter()
                .find(|(w, _)| *w == id)
                .map_or(info.name.as_str(), |(_, label)| label.as_str());
            drawn.tabs.push(label.to_owned());
            let text = format!("{index}  {label}");
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
            if let Some((link_pane, hit)) = parts.hover_link
                && *link_pane == pane
            {
                for &(_, col) in hit.cells.iter().filter(|&&(r, _)| r == row) {
                    if let Some(cell) = row_cells.to_mut().get_mut(col) {
                        cell.underline = true;
                        cell.underline_style = UnderlineStyle::Single;
                    }
                }
            }
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
    let light = light_background(config);
    let size_px = (11.0 * scale) as f32;
    let text_y = footer_y + (4.0 * scale) as i32;
    let gap = (16.0 * scale) as f32;
    let mut items: Vec<(String, u32)> = Vec::new();
    // Shown whatever else the footer says: input is waiting, not lost.
    if let Some((pane, kib)) = parts.stuck {
        items.push((
            format!("pane {pane}: the program is not reading input — {kib} KB waiting"),
            chrome.text,
        ));
    }
    if parts.prefix {
        // Static help lives here, not in the everyday footer.
        items.push((
            "PREFIX  c window  % / \" split  n/p tabs  h/j/k/l panes  a agents  d detach  x close  · closing the window detaches"
                .to_owned(),
            chrome.text,
        ));
    } else if !parts.status.is_empty() {
        items.push((parts.status.to_owned(), chrome.text));
    }
    let mut x = (12.0 * scale) as i32;
    for (text, color) in &items {
        let width = text_op(
            &mut ops, shaper, config, footer, x, text_y, text, *color, size_px,
        );
        x += (width + gap).ceil() as i32;
    }
    if let Some((segments, global)) = &parts.context {
        // The focused pane's context, fitted to what is left: whole
        // low-priority parts go first (tab_context::fit).
        // Real glyph widths, as drawn.
        let mut measure = |text: &str| shaper.shape_text(config, 0, 0, text, 0, size_px).1;
        let right = (12.0 * scale) as i32;
        let global_width = global.as_ref().map_or(0.0, |g| measure(&g.text));
        let room = (footer.w as f32 - x as f32 - right as f32 - global_width - gap).max(0.0);
        let fitted = crate::tab_context::fit(segments.clone(), room, gap, measure);
        // Separators are the only muted ink.
        let separator = mix_rgb(chrome.surface, chrome.muted, 60);
        for (index, segment) in fitted.iter().enumerate() {
            if index > 0 {
                text_op(
                    &mut ops,
                    shaper,
                    config,
                    footer,
                    x - (gap * 0.6) as i32,
                    text_y,
                    "·",
                    separator,
                    size_px,
                );
            }
            let color = tone_color(context_tone(segment.tone), &chrome, light);
            let width = text_op(
                &mut ops,
                shaper,
                config,
                footer,
                x,
                text_y,
                &segment.text,
                color,
                size_px,
            );
            drawn.footer.push(segment.text.clone());
            if segment.text != segment.full {
                drawn
                    .footer_full
                    .push((x, x + width.ceil() as i32, segment.full.clone()));
            }
            x += (width + gap).ceil() as i32;
        }
        if let Some(global) = global {
            let gx = footer.w as i32 - right - global_width.ceil() as i32;
            if gx > x {
                let color = tone_color(context_tone(global.tone), &chrome, light);
                text_op(
                    &mut ops,
                    shaper,
                    config,
                    footer,
                    gx,
                    text_y,
                    &global.text,
                    color,
                    size_px,
                );
                drawn.footer.push(global.text.clone());
            }
        }
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
            self.invalidate_rows();
            self.window = Some(window.clone());
            self.start_renderer(window)?;
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
        self.gpu = None;
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
                self.tab_context.update(&view.panes);
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
                self.update_hover();
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
                self.invalidate_rows();
                self.status =
                    "Daemon restarted — a new session replaced the one that ended with it".into();
                self.status_expires = Some(Instant::now() + Duration::from_secs(10));
                self.resize_panes();
                self.redraw();
            }
            UiEvent::Crow(view) => {
                // Ask the daemon which pane runs each live agent session
                // (it scans processes only while there are any).
                let pids = |view: &CrowView| -> Vec<u32> {
                    view.context.agents.iter().filter_map(|a| a.pid).collect()
                };
                let asked = pids(&view);
                if asked != pids(&self.crow_view) {
                    self.send(Request::AgentPids { pids: asked }, false);
                }
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
                if self.fatal_error.is_some() {
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(_) => {
                self.resize_panes();
                self.redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor;
                self.shaper.rescale(&self.config, self.scale);
                self.invalidate_rows();
                self.sent_sizes.clear();
                self.resize_panes();
                self.redraw();
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.mods = modifiers.state();
                self.update_hover();
            }
            WindowEvent::DroppedFile(path) => {
                self.paste_content(crate::clip::Content::Files(vec![path]))
            }
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
                self.update_hover();
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
        // Timed redraws: transient status notes, the visual bell and retries
        // of skipped GPU frames; otherwise stay event-driven.
        let now = Instant::now();
        if let Some(until) = self.status_expires
            && now >= until
        {
            self.status_expires = None;
            self.status.clear();
            self.redraw();
        }
        if let Some((until, _)) = &self.bell_flash
            && now >= *until
        {
            self.bell_flash = None;
            self.redraw();
        }
        if let Some(due) = self.retry_due
            && now >= due
        {
            self.retry_due = None;
            self.redraw();
        }
        // A debounced tab label becomes due: draw it then.
        if let Some(due) = self.label_due
            && now >= due
        {
            self.label_due = None;
            self.redraw();
        }
        let next = [
            self.status_expires,
            self.bell_flash.as_ref().map(|(until, _)| *until),
            self.retry_due,
            self.label_due,
        ]
        .into_iter()
        .flatten()
        .min();
        event_loop.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(stats) = &self.frame_stats {
            eprintln!("crow-term: {}", stats.summary());
            if let Some(file) = &stats.file {
                let _ = std::fs::write(file, stats.summary() + "\n");
            }
        }
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
    // Pasted data can never end bracketed paste (or start any sequence): every
    // ESC and C1 CSI is removed, so no marker can be rebuilt from pieces
    // (`\e[20\e[201~1~` is harmless). NUL is dropped too.
    let text: String = text
        .chars()
        .filter(|c| !matches!(c, '\0' | '\x1b' | '\u{9b}'))
        .collect();
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\n").replace('\n', "\r").into_bytes()
    }
}
fn selected_text(screen: &Screen, selection: &Selection) -> String {
    let (start, end) = selection.ordered();
    let mut out = String::new();
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
        // A soft-wrapped row continues on the next one: no line break, and
        // its trailing spaces are content.
        let wraps = row < end.row && screen.wrapped.get(row).copied().unwrap_or(false);
        if wraps {
            out.push_str(&text);
        } else {
            out.push_str(text.trim_end());
            if row < end.row {
                out.push('\n');
            }
        }
    }
    out
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardShortcut {
    Copy,
    Paste,
}
/// Cmd+C/V (macOS) or Ctrl+Shift+C/V: the terminal's clipboard. Plain Ctrl+C
/// and Ctrl+V are keys for the program (an agent's image paste, an interrupt).
fn clipboard_shortcut(mods: ModifiersState, key: &str) -> Option<ClipboardShortcut> {
    let shortcut = (mods.control_key() && mods.shift_key()) || mods.super_key();
    match key {
        "c" if shortcut => Some(ClipboardShortcut::Copy),
        "v" if shortcut => Some(ClipboardShortcut::Paste),
        _ => None,
    }
}
/// The modifier that turns a click into "open this link".
fn link_modifier_held(mods: ModifiersState) -> bool {
    if cfg!(target_os = "macos") {
        mods.super_key()
    } else {
        mods.control_key()
    }
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
    fn plain_ctrl_v_and_ctrl_c_stay_keys_for_the_program() {
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(clipboard_shortcut(ctrl, "v"), None);
        assert_eq!(clipboard_shortcut(ctrl, "c"), None);
        assert_eq!(
            clipboard_shortcut(ctrl | ModifiersState::SHIFT, "v"),
            Some(ClipboardShortcut::Paste)
        );
        assert_eq!(
            clipboard_shortcut(ModifiersState::SUPER, "c"),
            Some(ClipboardShortcut::Copy)
        );
        // What the program gets for Ctrl+V: the control byte, or the kitty
        // keyboard report when it asked for one (agents read images on it).
        assert_eq!(
            encode_named_key("v", ctrl, false, &Modes::default()),
            b"\x16"
        );
        let kitty = Modes {
            keyboard: 1,
            ..Modes::default()
        };
        assert_eq!(encode_named_key("v", ctrl, false, &kitty), b"\x1b[118;5u");
    }
    #[test]
    fn copy_joins_soft_wrapped_rows_and_keeps_wide_characters() {
        let row = |text: &str| -> Vec<Cell> {
            let mut cells = Vec::new();
            for c in text.chars() {
                let wide = unicode_width::UnicodeWidthChar::width(c) == Some(2);
                cells.push(cell(&c.to_string()));
                if wide {
                    let mut spacer = cell("");
                    spacer.wide_spacer = true;
                    cells.push(spacer);
                }
            }
            cells.resize(8, cell(" "));
            cells
        };
        let screen = Screen {
            size: Size { rows: 3, cols: 8 },
            cells: vec![row("echo 界 "), row("🦀done  "), row("next   ")],
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
            wrapped: vec![true, false, false],
        };
        let all = Selection {
            pane: 1,
            start: Point { row: 0, col: 0 },
            end: Point { row: 2, col: 7 },
        };
        // The wrapped row keeps its trailing space (it is content); hard
        // breaks become newlines with trailing blanks trimmed.
        assert_eq!(selected_text(&screen, &all), "echo 界 🦀done\nnext");
    }
    #[test]
    fn paste_protocol_and_escape_injection() {
        assert_eq!(paste_bytes("a\nb", true), b"\x1b[200~a\nb\x1b[201~");
        assert_eq!(paste_bytes("a\r\nb", false), b"a\rb");
        assert_eq!(
            paste_bytes("a\x1b[201~b\0", true),
            b"\x1b[200~a[201~b\x1b[201~"
        );
        // A marker nested in a marker: one pass of removal used to rebuild it.
        let nested = paste_bytes("x\x1b[20\x1b[201~1~\x15echo PWNED\r", true);
        assert_eq!(nested, b"\x1b[200~x[20[201~1~\x15echo PWNED\r\x1b[201~");
        assert_eq!(nested.windows(6).filter(|w| w == b"\x1b[201~").count(), 1);
        assert!(
            !paste_bytes("\u{9b}201~", false).contains(&0xc2),
            "C1 CSI removed"
        );
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
            wrapped: Vec::new(),
        };
        assert!(mailbox.push(
            1,
            View {
                instance: 1,
                revision: 2,
                sessions: vec![],
                screens: vec![(1, screen)],
                panes: vec![],
            }
        ));
        assert!(!mailbox.push(
            1,
            View {
                instance: 1,
                revision: 2,
                sessions: vec![],
                screens: vec![],
                panes: vec![],
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
                screens: vec![],
                panes: vec![],
            }
        ));
        assert_eq!(mailbox.take().unwrap().0, 2);
    }
    #[test]
    fn every_footer_tone_is_readable_on_light_and_dark_footers() {
        use crate::render::contrast_ratio as contrast;
        use crate::tab_context::Tone as T;
        let light = Config::default();
        let dark = Config {
            background: "#1E1E1E".into(),
            foreground: "#D4D4D4".into(),
            ..Config::default()
        };
        for config in [&light, &dark] {
            let chrome = Chrome::new(config);
            let is_light = light_background(config);
            for tone in [T::Plain, T::On, T::Accent, T::Warn, T::Alert] {
                let ratio = contrast(
                    tone_color(context_tone(tone), &chrome, is_light),
                    chrome.surface,
                );
                assert!(
                    ratio >= 4.5,
                    "{tone:?} on the {} footer: {ratio:.2}:1",
                    if is_light { "light" } else { "dark" }
                );
            }
        }
    }
}
