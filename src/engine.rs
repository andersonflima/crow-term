//! PTY ownership and VT emulation. The daemon owns this object, not a GUI view.
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use anyhow::{Context, Result, anyhow, ensure};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::intercept::{Interceptor, Out, Query};
use crate::model::{Cell, ColorSpec, Cursor, Modes, Screen, ShellSpec, Size};

pub type Notify = Arc<dyn Fn() + Send + Sync>;

const MAX_SCROLLBACK: usize = 100_000;
type VtParser = Processor;

#[derive(Clone)]
struct Events(mpsc::Sender<Event>);
impl EventListener for Events {
    fn send_event(&self, event: Event) {
        // This is deliberately a queue: a VT response must never block the reader
        // while it holds the terminal lock (nor call back into the daemon).
        if matches!(
            event,
            Event::Title(_)
                | Event::ResetTitle
                | Event::PtyWrite(_)
                | Event::TextAreaSizeRequest(_)
                | Event::ColorRequest(_, _)
        ) {
            let _ = self.0.send(event);
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

fn checked_size(size: Size) -> Result<Size> {
    ensure!(
        size.rows >= 1 && size.cols >= 2,
        "terminal needs at least 1 row and 2 columns"
    );
    ensure!(
        usize::from(size.rows) * usize::from(size.cols) <= 1_048_576,
        "terminal dimensions exceed the cell limit"
    );
    Ok(size)
}
fn pty_size(size: Size) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

struct State {
    term: Term<Events>,
    parser: VtParser,
    intercept: Interceptor,
    /// Same queue as the parser's own replies, so answers keep stream order.
    replies: mpsc::Sender<Event>,
    pending_events: mpsc::Receiver<Event>,
    size: Size,
    title: String,
    default_colors: [Rgb; alacritty_terminal::term::color::COUNT],
}
impl State {
    fn new(size: Size, scrollback: usize) -> Self {
        let (events, pending_events) = mpsc::channel();
        let config = Config {
            scrolling_history: scrollback.min(MAX_SCROLLBACK),
            // Never let terminal output read or overwrite the system clipboard.
            osc52: Osc52::Disabled,
            // Progressive enhancement: push/pop/query/set (CSI > u, CSI < u,
            // CSI ? u, CSI = u). The stack depth is bounded by `Interceptor`.
            kitty_keyboard: true,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, Events(events.clone())),
            parser: VtParser::new(),
            intercept: Interceptor::default(),
            replies: events,
            pending_events,
            size,
            title: String::new(),
            default_colors: std::array::from_fn(default_rgb),
        }
    }

    fn advance(&mut self, bytes: &[u8]) {
        let Self {
            term,
            parser,
            intercept,
            replies,
            ..
        } = self;
        intercept.scan(bytes, &mut |out| match out {
            Out::Bytes(bytes) => parser.advance(term, bytes),
            Out::Query(query) => {
                let _ = replies.send(Event::PtyWrite(answer(query)));
            }
        });
    }

    fn expire_sync(&mut self) -> bool {
        if self
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.parser.stop_sync(&mut self.term);
            true
        } else {
            false
        }
    }

    fn finish_sync(&mut self) {
        let Self {
            term,
            parser,
            intercept,
            ..
        } = self;
        intercept.flush(&mut |out| {
            if let Out::Bytes(bytes) = out {
                parser.advance(term, bytes);
            }
        });
        self.parser.stop_sync(&mut self.term);
    }

    fn events(&mut self, writer: &mpsc::SyncSender<WriteMessage>) {
        for event in self.pending_events.try_iter() {
            let response = match event {
                Event::Title(title) => {
                    self.title = title;
                    None
                }
                Event::ResetTitle => {
                    self.title.clear();
                    None
                }
                Event::PtyWrite(text) => Some(text),
                Event::TextAreaSizeRequest(format) => Some(format(WindowSize {
                    num_lines: self.size.rows,
                    num_cols: self.size.cols,
                    cell_width: 0,
                    cell_height: 0,
                })),
                Event::ColorRequest(index, format) => {
                    let color = (index < alacritty_terminal::term::color::COUNT)
                        .then(|| self.term.colors()[index])
                        .flatten()
                        .unwrap_or_else(|| {
                            self.default_colors
                                .get(index)
                                .copied()
                                .unwrap_or_else(|| default_rgb(index))
                        });
                    Some(format(color))
                }
                // Clipboard requests are disabled above; the engine has no GUI
                // permissions and intentionally does not service them here.
                _ => None,
            };
            if let Some(text) = response {
                let _ = writer.try_send(WriteMessage::Bytes(text.into_bytes()));
            }
        }
    }

    fn snapshot(&self, alive: bool) -> Screen {
        let content = self.term.renderable_content();
        let mut cells = Vec::with_capacity(self.size.rows as usize);
        let mut row = Vec::with_capacity(self.size.cols as usize);
        for indexed in content.display_iter {
            let cell = indexed.cell;
            let mut fg = convert_color(cell.fg, content.colors, &self.default_colors);
            let mut bg = convert_color(cell.bg, content.colors, &self.default_colors);
            if cell.flags.contains(Flags::DIM) {
                let rgb = resolve_color(cell.fg, content.colors, &self.default_colors);
                fg = ColorSpec::Rgb(
                    ((u16::from(rgb.r) * 2) / 3) as u8,
                    ((u16::from(rgb.g) * 2) / 3) as u8,
                    ((u16::from(rgb.b) * 2) / 3) as u8,
                );
            }
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let hidden = cell.flags.contains(Flags::HIDDEN);
            let mut text = if hidden {
                " ".into()
            } else {
                cell.c.to_string()
            };
            if !hidden && let Some(zero) = cell.zerowidth() {
                text.extend(zero);
            }
            row.push(Cell {
                text,
                fg,
                bg,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                strike: cell.flags.contains(Flags::STRIKEOUT),
                wide_spacer: cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
            });
            if row.len() == self.size.cols as usize {
                cells.push(std::mem::replace(
                    &mut row,
                    Vec::with_capacity(self.size.cols as usize),
                ));
            }
        }
        let cursor = content.cursor;
        let cursor_row = cursor.point.line.0 as i64 + content.display_offset as i64;
        let visible = cursor.shape != CursorShape::Hidden
            && cursor_row >= 0
            && cursor_row < i64::from(self.size.rows);
        let mode = content.mode;
        Screen {
            size: self.size,
            cells,
            cursor: Cursor {
                row: cursor_row.max(0) as usize,
                col: cursor.point.column.0,
                visible,
                shape: match cursor.shape {
                    CursorShape::Beam => "beam",
                    CursorShape::Underline => "underline",
                    CursorShape::HollowBlock => "hollow",
                    CursorShape::Hidden => "hidden",
                    CursorShape::Block => "block",
                }
                .into(),
            },
            modes: Modes {
                bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
                application_cursor: mode.contains(TermMode::APP_CURSOR),
                application_keypad: mode.contains(TermMode::APP_KEYPAD),
                mouse_sgr: mode.contains(TermMode::SGR_MOUSE),
                mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
                mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
                mouse_click: mode.intersects(TermMode::MOUSE_MODE),
                focus_reporting: mode.contains(TermMode::FOCUS_IN_OUT),
                keyboard: keyboard_flags(mode),
                modify_other_keys: self.intercept.modify_other_keys,
            },
            title: self.title.clone(),
            alive,
            display_offset: content.display_offset,
        }
    }
}

/// Kitty progressive-enhancement flags (1 disambiguate, 2 event types,
/// 4 alternate keys, 8 all keys as escapes, 16 associated text).
fn keyboard_flags(mode: TermMode) -> u8 {
    [
        (TermMode::DISAMBIGUATE_ESC_CODES, 1),
        (TermMode::REPORT_EVENT_TYPES, 2),
        (TermMode::REPORT_ALTERNATE_KEYS, 4),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
        (TermMode::REPORT_ASSOCIATED_TEXT, 16),
    ]
    .into_iter()
    .filter(|(flag, _)| mode.contains(*flag))
    .fold(0, |flags, (_, bit)| flags | bit)
}

fn answer(query: Query) -> String {
    match query {
        Query::ModifyOtherKeys(level) => format!("\x1b[>4;{level}m"),
    }
}

fn convert_color(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    defaults: &[Rgb; alacritty_terminal::term::color::COUNT],
) -> ColorSpec {
    match color {
        Color::Spec(rgb) => ColorSpec::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(index) if colors[index as usize].is_none() => ColorSpec::Indexed(index),
        Color::Named(NamedColor::Foreground) if colors[NamedColor::Foreground].is_none() => {
            ColorSpec::Foreground
        }
        Color::Named(NamedColor::Background) if colors[NamedColor::Background].is_none() => {
            ColorSpec::Background
        }
        Color::Named(named) if (named as usize) < 16 && colors[named].is_none() => {
            ColorSpec::Indexed(named as u8)
        }
        _ => {
            let rgb = resolve_color(color, colors, defaults);
            ColorSpec::Rgb(rgb.r, rgb.g, rgb.b)
        }
    }
}
fn resolve_color(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    defaults: &[Rgb; alacritty_terminal::term::color::COUNT],
) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Indexed(index) => colors[index as usize].unwrap_or(defaults[index as usize]),
        Color::Named(named) => colors[named].unwrap_or(defaults[named as usize]),
    }
}
fn default_rgb(index: usize) -> Rgb {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let (r, g, b) = match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let i = index - 16;
            let component = |v: usize| if v == 0 { 0 } else { (55 + 40 * v) as u8 };
            (component(i / 36), component(i / 6 % 6), component(i % 6))
        }
        232..=255 => {
            let v = (8 + 10 * (index - 232)) as u8;
            (v, v, v)
        }
        257 => (0, 0, 0),
        259..=266 => {
            let (r, g, b) = ANSI[index - 259];
            (r / 2, g / 2, b / 2)
        }
        267 => (255, 255, 255),
        _ => (229, 229, 229),
    };
    Rgb { r, g, b }
}

enum WriteMessage {
    Bytes(Vec<u8>),
    Stop,
}
struct Process {
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    alive: AtomicBool,
    notify: Notify,
}
impl Process {
    fn exited(&self) {
        if self.alive.swap(false, Ordering::AcqRel) {
            (self.notify)();
        }
    }
}

/// A daemon-owned terminal. No thread captures `Engine`, so dropping the final
/// daemon owner cleans up the process; merely disconnecting a client does not.
pub struct Engine {
    state: Arc<Mutex<State>>,
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    writer: mpsc::SyncSender<WriteMessage>,
    write_error: Arc<Mutex<Option<String>>>,
    process: Arc<Process>,
    notify: Notify,
}
// Unix cleanup must run before any wait/try_wait reaps the session leader.
#[cfg(unix)]
fn foreground_group(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Result<Option<rustix::process::Pid>> {
    Ok(master
        .lock()
        .map_err(|_| anyhow!("PTY master poisoned"))?
        .as_ref()
        .and_then(|master| master.process_group_leader())
        .and_then(rustix::process::Pid::from_raw))
}

#[cfg(unix)]
fn reap_unix_child(
    child: &mut (dyn Child + Send + Sync),
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Result<bool> {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let raw = child.process_id().context("PTY shell has no process ID")?;
    let pid = Pid::from_raw(i32::try_from(raw).context("invalid shell PID")?)
        .context("invalid shell PID")?;
    // WNOWAIT is available on both supported Unix targets (Darwin/Linux).
    // Observe exit without reaping, keeping PID/SID reserved while descendants
    // holding the PTY slave are found and terminated, then use the portable
    // child handle to perform the one final reap and retain its exit status.
    let status = waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    )?;
    if status.is_none() {
        return Ok(false);
    }
    kill_unix_session(raw, foreground_group(master)?)?;
    child.wait().context("reap exited terminal shell")?;
    Ok(true)
}

#[cfg(unix)]
fn kill_unix_session(raw_shell: u32, foreground: Option<rustix::process::Pid>) -> Result<()> {
    use rustix::process::{Pid, Signal, getpgrp, getsid, kill_process, kill_process_group};
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let shell = Pid::from_raw(i32::try_from(raw_shell).context("invalid shell PID")?)
        .context("invalid shell PID")?;
    ensure!(
        !shell.is_init() && shell != rustix::process::getpid(),
        "refusing unrelated session"
    );
    // portable-pty calls setsid before exec. Holding the unreaped child
    // reserves this SID: another terminal cannot reuse its session ID.
    // Do not infer membership from ancestry: deliberate setsid-based
    // detached sessions must remain untouched.
    // macOS cannot query getsid on a zombie session leader. Membership
    // is anchored by portable-pty's setsid guarantee and the still-unreaped
    // child PID, not by a live-leader query. Never signal our own session.
    ensure!(shell != getsid(None)?, "refusing own process session");
    let mut system = System::new();
    for _ in 0..2 {
        // No environment, command line, executable or task enumeration.
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        );
        let sid = sysinfo::Pid::from_u32(raw_shell);
        let mut candidates: Vec<_> = system
            .processes()
            .values()
            .filter(|process| process.pid() != sid && process.session_id() == Some(sid))
            .collect();
        // Kill deeper descendants first, leaving their parents (and our
        // shell last) able to reap children for as long as possible.
        candidates.sort_by_key(|process| {
            let mut depth = 0usize;
            let mut parent = process.parent();
            while let Some(pid) = parent {
                depth += 1;
                if pid == sid || depth == 64 {
                    break;
                }
                parent = system.process(pid).and_then(|process| process.parent());
            }
            std::cmp::Reverse(depth)
        });
        for process in candidates {
            let Some(pid) = i32::try_from(process.pid().as_u32())
                .ok()
                .and_then(Pid::from_raw)
            else {
                continue;
            };
            // session_id is a live OS query. Recheck immediately before
            // signalling, rejecting exited/reused PIDs and new sessions.
            if getsid(Some(pid)).ok() != Some(shell) {
                continue;
            }
            if let Err(error) = kill_process(pid, Signal::KILL)
                && error != rustix::io::Errno::SRCH
            {
                return Err(error).context("terminate PTY session member");
            }
        }
    }
    // Fallback covers the foreground and shell groups even if platform
    // process enumeration missed a member. Groups cannot span sessions.
    for group in [foreground, Some(shell)].into_iter().flatten() {
        if group == getpgrp() || group.is_init() || getsid(Some(group)).ok() != Some(shell) {
            continue;
        }
        if let Err(error) = kill_process_group(group, Signal::KILL)
            && error != rustix::io::Errno::SRCH
        {
            return Err(error).context("terminate terminal process group");
        }
    }
    Ok(())
}

impl Engine {
    pub fn new(spec: &ShellSpec, size: Size, scrollback: usize, notify: Notify) -> Result<Self> {
        Self::new_with_theme(spec, size, scrollback, notify, None)
    }

    pub fn new_with_theme(
        spec: &ShellSpec,
        size: Size,
        scrollback: usize,
        notify: Notify,
        theme: Option<&crate::config::Config>,
    ) -> Result<Self> {
        let size = checked_size(size)?;
        ensure!(!spec.program.is_empty(), "shell program is empty");
        let pair = portable_pty::native_pty_system()
            .openpty(pty_size(size))
            .context("open terminal PTY")?;
        let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let mut pty_writer = pair.master.take_writer().context("open PTY writer")?;
        let mut command = CommandBuilder::new(&spec.program);
        command.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            command.cwd(cwd);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "crow-term");
        // A nested shell must not inherit another multiplexer's control socket.
        command.env_remove("TMUX");
        command.env_remove("STY");
        let child = pair
            .slave
            .spawn_command(command)
            .context("spawn terminal shell")?;
        drop(pair.slave);
        let (writer, writes) = mpsc::sync_channel(32);
        let state = Arc::new(Mutex::new(State::new(size, scrollback)));
        let write_error = Arc::new(Mutex::new(None));
        let process = Arc::new(Process {
            child: Mutex::new(Some(child)),
            alive: AtomicBool::new(true),
            notify: notify.clone(),
        });
        let engine = Self {
            state: state.clone(),
            master: Arc::new(Mutex::new(Some(pair.master))),
            writer: writer.clone(),
            write_error: write_error.clone(),
            process: process.clone(),
            notify: notify.clone(),
        };

        // Set defaults before the reader/parser can answer an application's first OSC query.
        if let Some(theme) = theme {
            engine.set_colors(&theme.palette, &theme.foreground, &theme.background)?;
        }

        thread::Builder::new()
            .name("crow-pty-write".into())
            .spawn(move || {
                while let Ok(message) = writes.recv() {
                    match message {
                        WriteMessage::Stop => break,
                        WriteMessage::Bytes(bytes) => {
                            if let Err(error) = pty_writer
                                .write_all(&bytes)
                                .and_then(|_| pty_writer.flush())
                            {
                                if let Ok(mut slot) = write_error.lock() {
                                    *slot = Some(error.to_string());
                                }
                                break;
                            }
                        }
                    }
                }
            })
            .context("start PTY writer")?;
        let reader_state = state.clone();
        thread::Builder::new()
            .name("crow-pty-read".into())
            .spawn(move || {
                let mut buffer = [0; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let Ok(mut state) = reader_state.lock() else {
                                break;
                            };
                            state.advance(&buffer[..count]);
                            state.events(&writer);
                            drop(state);
                            notify();
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break, // Unix PTYs may report EIO instead of EOF.
                    }
                }
                // EOF commits the last partial batch even if the child exits
                // before its synchronized-output deadline.
                if let Ok(mut state) = reader_state.lock() {
                    state.finish_sync();
                    state.events(&writer);
                    drop(state);
                    notify();
                }
            })
            .context("start PTY reader")?;
        let writer = engine.writer.clone();
        #[cfg(unix)]
        let master = engine.master.clone();
        thread::Builder::new()
            .name("crow-pty-reap".into())
            .spawn(move || {
                loop {
                    // The existing reaper also services the VT's 150ms deadline.
                    // Reader I/O remains blocking without trapping a batch when
                    // the application sends BSU and then becomes silent.
                    if let Ok(mut state) = state.lock()
                        && state.expire_sync()
                    {
                        state.events(&writer);
                        drop(state);
                        (process.notify)();
                    }
                    let finished = {
                        let Ok(mut child) = process.child.lock() else {
                            break;
                        };
                        match child.as_mut() {
                            Some(handle) => {
                                #[cfg(unix)]
                                let finished = reap_unix_child(handle.as_mut(), &master);
                                #[cfg(not(unix))]
                                let finished = handle
                                    .try_wait()
                                    .map(|status| status.is_some())
                                    .map_err(anyhow::Error::from);
                                match finished {
                                    Ok(true) => {
                                        child.take();
                                        true
                                    }
                                    // Preserve the unreaped handle on transient
                                    // errors, so no SID can be reused before cleanup.
                                    Ok(false) | Err(_) => false,
                                }
                            }
                            None => true,
                        }
                    };
                    if finished {
                        process.exited();
                        let _ = writer.try_send(WriteMessage::Stop);
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            })
            .context("start shell reaper")?;
        Ok(engine)
    }

    /// Theme defaults are separate from application OSC overrides, so OSC reset
    /// returns to the configured theme rather than to a hard-coded palette.
    pub fn set_colors(&self, palette: &[String], foreground: &str, background: &str) -> Result<()> {
        fn parse(value: &str) -> Result<Rgb> {
            let hex = value.strip_prefix('#').context("color must start with #")?;
            let expanded;
            let hex = if hex.len() == 3 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                expanded = hex.chars().flat_map(|c| [c, c]).collect::<String>();
                expanded.as_str()
            } else {
                hex
            };
            ensure!(
                hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
                "color must be #RGB or #RRGGBB"
            );
            Ok(Rgb {
                r: u8::from_str_radix(&hex[0..2], 16)?,
                g: u8::from_str_radix(&hex[2..4], 16)?,
                b: u8::from_str_radix(&hex[4..6], 16)?,
            })
        }
        ensure!(
            palette.len() == 16,
            "terminal palette needs exactly 16 colors"
        );
        let palette = palette
            .iter()
            .map(|color| parse(color))
            .collect::<Result<Vec<_>>>()?;
        let fg = parse(foreground)?;
        let bg = parse(background)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("terminal state poisoned"))?;
        state.default_colors[..16].copy_from_slice(&palette);
        for (index, rgb) in palette.iter().take(8).enumerate() {
            state.default_colors[259 + index] = Rgb {
                r: rgb.r / 2,
                g: rgb.g / 2,
                b: rgb.b / 2,
            };
        }
        state.default_colors[256] = fg;
        state.default_colors[257] = bg;
        state.default_colors[258] = fg;
        state.default_colors[267] = fg;
        state.default_colors[268] = Rgb {
            r: fg.r / 2,
            g: fg.g / 2,
            b: fg.b / 2,
        };
        drop(state);
        (self.notify)();
        Ok(())
    }

    /// Queue input without blocking the PTY output consumer on a full OS pipe.
    pub fn input(&self, bytes: &[u8]) -> Result<()> {
        ensure!(self.alive(), "terminal process has exited");
        ensure!(
            bytes.len() <= 1024 * 1024,
            "PTY input limit is 1 MiB per message"
        );
        if let Some(error) = self
            .write_error
            .lock()
            .map_err(|_| anyhow!("PTY writer poisoned"))?
            .as_ref()
        {
            return Err(anyhow!("PTY write failed: {error}"));
        }
        let mut message = WriteMessage::Bytes(bytes.to_vec());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match self.writer.try_send(message) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Full(pending)) if std::time::Instant::now() < deadline => {
                    message = pending;
                    thread::sleep(Duration::from_millis(1));
                }
                Err(mpsc::TrySendError::Full(_)) => {
                    return Err(anyhow!("PTY input queue remained full for 5 seconds"));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(anyhow!("PTY writer stopped"));
                }
            }
        }
    }
    pub fn resize(&self, size: Size) -> Result<()> {
        let size = checked_size(size)?;
        // One order for both locks. The reader only locks state, never master.
        let master = self
            .master
            .lock()
            .map_err(|_| anyhow!("PTY master poisoned"))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("terminal state poisoned"))?;
        if state.size == size {
            return Ok(());
        }
        master
            .as_ref()
            .context("terminal PTY closed")?
            .resize(pty_size(size))?;
        state.term.resize(size);
        state.size = size;
        drop(state);
        drop(master);
        (self.notify)();
        Ok(())
    }
    /// Positive lines move back into history; negative lines approach live output.
    pub fn scroll(&self, lines: i32) -> Result<()> {
        self.state
            .lock()
            .map_err(|_| anyhow!("terminal state poisoned"))?
            .term
            .scroll_display(Scroll::Delta(lines));
        (self.notify)();
        Ok(())
    }
    pub fn snapshot(&self) -> Result<Screen> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("terminal state poisoned"))?
            .snapshot(self.alive()))
    }
    pub fn alive(&self) -> bool {
        self.process.alive.load(Ordering::Acquire)
    }
    /// Working directory of the pane's foreground process (falling back to its
    /// shell). One targeted OS query: no process enumeration, environment or
    /// command line is read. `None` when the platform does not expose it.
    pub fn cwd(&self) -> Option<String> {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        if !self.alive() {
            return None;
        }
        #[cfg(unix)]
        let foreground = foreground_group(&self.master)
            .ok()
            .flatten()
            .map(|pid| pid.as_raw_nonzero().get() as u32);
        #[cfg(not(unix))]
        let foreground: Option<u32> = None;
        let shell = self.process.child.lock().ok()?.as_ref()?.process_id();
        let mut system = System::new();
        for raw in [foreground, shell].into_iter().flatten() {
            let pid = Pid::from_u32(raw);
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false,
                ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
            );
            if let Some(cwd) = system.process(pid).and_then(|process| process.cwd())
                && !cwd.as_os_str().is_empty()
            {
                return Some(cwd.to_string_lossy().into_owned());
            }
        }
        None
    }
    pub fn kill(&self) -> Result<()> {
        {
            let mut child = self
                .process
                .child
                .lock()
                .map_err(|_| anyhow!("shell handle poisoned"))?;
            if let Some(handle) = child.as_mut() {
                #[cfg(unix)]
                if let Some(raw_shell) = handle.process_id() {
                    // Do not call try_wait first: it could reap an already-exited
                    // shell, release its SID and make descendant cleanup unsafe.
                    kill_unix_session(raw_shell, foreground_group(&self.master)?)?;
                }
                if handle.try_wait()?.is_none() {
                    // Portable Windows fallback; no job-tree guarantee is made.
                    handle.kill().context("terminate terminal shell")?;
                    handle.wait().context("reap terminal shell")?;
                }
                child.take();
            }
        }
        self.process.exited();
        let _ = self.writer.try_send(WriteMessage::Stop);
        self.master
            .lock()
            .map_err(|_| anyhow!("PTY master poisoned"))?
            .take();
        Ok(())
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vt(size: Size, history: usize, bytes: &[u8]) -> Screen {
        let (writer, _) = mpsc::sync_channel(32);
        let mut state = State::new(size, history);
        state.advance(bytes);
        state.events(&writer);
        state.snapshot(true)
    }
    #[test]
    fn engine_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<Engine>();
    }
    #[test]
    fn vt_unicode_attributes_modes_and_title() {
        let screen = vt(Size { rows: 4, cols: 20 }, 20,
            "\x1b]2;editor\x07\x1b[1;3;4;9;38;2;12;34;56m界e\u{301}\x1b[?2004h\x1b[?1h\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[6 q".as_bytes());
        assert_eq!(screen.title, "editor");
        let cell = &screen.cells[0][0];
        assert_eq!(cell.text, "界");
        assert_eq!(cell.fg, ColorSpec::Rgb(12, 34, 56));
        assert!(cell.bold && cell.italic && cell.underline && cell.strike);
        assert!(screen.cells[0][1].wide_spacer);
        assert_eq!(screen.cells[0][2].text, "e\u{301}");
        assert!(screen.modes.bracketed_paste && screen.modes.application_cursor);
        assert!(
            screen.modes.mouse_sgr
                && screen.modes.mouse_motion
                && screen.modes.mouse_click
                && screen.modes.focus_reporting
        );
        assert_eq!(screen.cursor.shape, "beam");
    }
    #[test]
    fn vt_alt_screen_restores_primary_and_hides_cursor() {
        let screen = vt(
            Size { rows: 3, cols: 12 },
            10,
            b"primary\x1b[?1049halt\x1b[?1049l\x1b[?25l",
        );
        assert_eq!(screen.cells[0][0].text, "p");
        assert!(!screen.cursor.visible);
    }
    #[test]
    fn history_is_bounded_and_cursor_is_off_viewport() {
        let mut state = State::new(Size { rows: 2, cols: 10 }, 3);
        state.advance(b"0\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6");
        assert_eq!(state.term.grid().history_size(), 3);
        state.term.scroll_display(Scroll::Delta(100));
        let screen = state.snapshot(true);
        assert_eq!(screen.display_offset, 3);
        assert!(!screen.cursor.visible);
        assert_eq!(screen.cells[0][0].text, "2");
    }
    #[test]
    fn vt_answers_queries_without_reentrant_locking() {
        let (writer, writes) = mpsc::sync_channel(32);
        let mut state = State::new(Size { rows: 5, cols: 40 }, 0);
        state.advance(b"\x1b[6n\x1b[18t\x1b]11;?\x07");
        state.events(&writer);
        let replies: Vec<_> = writes
            .try_iter()
            .filter_map(|message| match message {
                WriteMessage::Bytes(bytes) => Some(String::from_utf8(bytes).unwrap()),
                _ => None,
            })
            .collect();
        assert!(
            replies.iter().any(|text| text == "\x1b[1;1R"),
            "{replies:?}"
        );
        assert!(
            replies.iter().any(|text| text == "\x1b[8;5;40t"),
            "{replies:?}"
        );
        assert!(
            replies
                .iter()
                .any(|text| text.contains("rgb:0000/0000/0000")),
            "{replies:?}"
        );
    }
    fn replies(state: &mut State) -> Vec<String> {
        let (writer, writes) = mpsc::sync_channel(64);
        state.events(&writer);
        writes
            .try_iter()
            .filter_map(|message| match message {
                WriteMessage::Bytes(bytes) => Some(String::from_utf8(bytes).unwrap()),
                _ => None,
            })
            .collect()
    }
    #[test]
    fn kitty_keyboard_stack_and_modify_other_keys_reach_the_snapshot() {
        let mut state = State::new(Size { rows: 3, cols: 20 }, 0);
        state.advance(b"\x1b[?u\x1b[>1u\x1b[?u\x1b[c");
        assert_eq!(state.snapshot(true).modes.keyboard, 1);
        // Answers stay in stream order with the parser's own replies.
        assert_eq!(replies(&mut state), ["\x1b[?0u", "\x1b[?1u", "\x1b[?6c"]);
        state.advance(b"\x1b[>31u");
        assert_eq!(state.snapshot(true).modes.keyboard, 31);
        // The alternate screen has its own stack.
        state.advance(b"\x1b[?1049h");
        assert_eq!(state.snapshot(true).modes.keyboard, 0);
        state.advance(b"\x1b[?1049l\x1b[<u");
        assert_eq!(state.snapshot(true).modes.keyboard, 1);
        state.advance(b"\x1b[<u");
        assert_eq!(state.snapshot(true).modes.keyboard, 0);
        state.advance(b"\x1b[>4;2m\x1b[?4m\x1b[5n");
        assert_eq!(state.snapshot(true).modes.modify_other_keys, 2);
        assert_eq!(replies(&mut state), ["\x1b[>4;2m", "\x1b[0n"]);
        // A flood of pushes is bounded instead of reaching the parser's limit.
        let flood = b"\x1b[>1u".repeat(10_000);
        state.advance(&flood);
        assert_eq!(state.snapshot(true).modes.keyboard, 1);
        state.advance(b"\x1bc");
        let modes = state.snapshot(true).modes;
        assert_eq!((modes.keyboard, modes.modify_other_keys), (0, 0));
    }
    #[cfg(unix)]
    fn wait_for(engine: &Engine, predicate: impl Fn(&Screen) -> bool) -> Screen {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let screen = engine.snapshot().unwrap();
            if predicate(&screen) {
                return screen;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "PTY timed out: {screen:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(unix)]
    fn text(screen: &Screen) -> String {
        screen
            .cells
            .iter()
            .flatten()
            .map(|cell| cell.text.as_str())
            .collect()
    }
    #[test]
    #[cfg(unix)]
    fn real_shell_output_input_resize_exit_and_reap() {
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = changes.clone();
        let shell = ShellSpec { program: "/bin/sh".into(),
            args: vec!["-c".into(), "printf '\\033]2;PTY test\\007READY\\n'; read value; stty size; printf 'VALUE:%s\\n' \"$value\"".into()], cwd: None };
        let engine = Engine::new(
            &shell,
            Size { rows: 5, cols: 40 },
            10,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .unwrap();
        let palette = vec!["#abc".to_owned(); 16];
        engine.set_colors(&palette, "#123456", "#fedcba").unwrap();
        assert_eq!(
            engine.state.lock().unwrap().default_colors[257],
            Rgb {
                r: 254,
                g: 220,
                b: 186
            }
        );
        assert!(engine.set_colors(&palette, "invalid", "#000").is_err());
        assert_eq!(
            engine.state.lock().unwrap().default_colors[256],
            Rgb {
                r: 18,
                g: 52,
                b: 86
            }
        );
        wait_for(&engine, |s| text(s).contains("READY"));
        engine.resize(Size { rows: 8, cols: 50 }).unwrap();
        engine.input(b"hello\r").unwrap();
        let screen = wait_for(&engine, |s| text(s).contains("VALUE:hello") && !s.alive);
        assert!(text(&screen).contains("8 50"));
        assert_eq!(screen.title, "PTY test");
        assert!(changes.load(Ordering::Relaxed) >= 3);
        assert!(engine.process.child.lock().unwrap().is_none());
        engine.kill().unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn explicit_kill_reaps_shell_ignoring_hangup() {
        let shell = ShellSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "trap '' HUP; printf 'STARTED'; while :; do :; done".into(),
            ],
            cwd: None,
        };
        let engine = Engine::new(&shell, Size::default(), 0, Arc::new(|| {})).unwrap();
        wait_for(&engine, |s| text(s).contains("STARTED"));
        engine.kill().unwrap();
        assert!(!engine.alive());
        assert!(engine.process.child.lock().unwrap().is_none());
    }
    #[test]
    #[cfg(unix)]
    fn explicit_kill_terminates_foreground_job_group() {
        let shell = ShellSpec { program: "/bin/sh".into(), args: vec!["-c".into(),
            r#"set -m; /bin/sh -c 'trap "" HUP; printf "CHILD:%s\n" "$$"; while :; do :; done'; printf DONE"#.into()], cwd: None };
        let engine = Engine::new(&shell, Size::default(), 0, Arc::new(|| {})).unwrap();
        let screen = wait_for(&engine, |s| text(s).contains("CHILD:"));
        let output = text(&screen);
        let pid: i32 = output
            .split("CHILD:")
            .nth(1)
            .unwrap()
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap();
        assert_eq!(
            engine
                .master
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .process_group_leader(),
            Some(pid)
        );
        engine.kill().unwrap();
        let child = rustix::process::Pid::from_raw(pid).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while rustix::process::test_kill_process(child).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "foreground job survived terminal close"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn synchronized_output_is_atomic_until_end_marker() {
        let mut state = State::new(Size { rows: 2, cols: 20 }, 0);
        state.advance(b"OLD");
        state.advance(b"\x1b[?2026h\x1b[2J\x1b[HNEW");
        assert_eq!(state.snapshot(true).cells[0][0].text, "O");
        assert!(!state.expire_sync());
        state.advance(b"\x1b[?2026l");
        assert_eq!(state.snapshot(true).cells[0][0].text, "N");
        assert!(state.parser.sync_timeout().sync_timeout().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn synchronized_output_times_out_without_further_pty_bytes() {
        let shell = ShellSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "printf 'READY'; read value; printf '\\033[?2026h\\033[2J\\033[HAFTER'; read value"
                    .into(),
            ],
            cwd: None,
        };
        let engine = Engine::new(&shell, Size::default(), 0, Arc::new(|| {})).unwrap();
        wait_for(&engine, |screen| text(screen).contains("READY"));
        engine.input(b"next\r").unwrap();
        // Synchronize with the reader having received BSU, not shell scheduling.
        let deadline = Instant::now() + Duration::from_secs(5);
        while engine
            .state
            .lock()
            .unwrap()
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_none()
        {
            assert!(Instant::now() < deadline, "BSU never reached parser");
            thread::sleep(Duration::from_millis(2));
        }
        assert!(!text(&engine.snapshot().unwrap()).contains("AFTER"));
        let observed = Instant::now();
        let screen = wait_for(&engine, |screen| text(screen).contains("AFTER"));
        assert!(
            observed.elapsed() >= Duration::from_millis(100),
            "batch committed before timeout"
        );
        assert!(
            screen.alive,
            "fixture must remain silent and alive during expiry"
        );
        engine.kill().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn explicit_kill_terminates_background_session_jobs_only() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut unrelated = ChildGuard(
            std::process::Command::new("/bin/sleep")
                .arg("120")
                .spawn()
                .unwrap(),
        );
        let shell = ShellSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "set -m; sleep 120 & printf 'BG:%s\\n' \"$!\"; wait".into(),
            ],
            cwd: None,
        };
        let engine = Engine::new(&shell, Size::default(), 0, Arc::new(|| {})).unwrap();
        let output = text(&wait_for(&engine, |screen| text(screen).contains("BG:")));
        let pid: i32 = output
            .split("BG:")
            .nth(1)
            .unwrap()
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap();
        engine.kill().unwrap();
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rustix::process::test_kill_process(pid).is_ok() {
            assert!(
                Instant::now() < deadline,
                "background terminal job survived close"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            unrelated.0.try_wait().unwrap().is_none(),
            "unrelated session was signalled"
        );
    }
    #[test]
    #[cfg(unix)]
    fn natural_shell_exit_cleans_background_before_reaping_session_leader() {
        let directory = tempfile::tempdir().unwrap();
        let shell = ShellSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "set -m; sleep 120 & printf '%s\\n' \"$!\" > background.pid".into(),
            ],
            cwd: Some(directory.path().to_string_lossy().into_owned()),
        };
        let engine = Engine::new(&shell, Size::default(), 0, Arc::new(|| {})).unwrap();
        // No wait in the shell, and no call to kill: natural reaping itself must
        // clean jobs, even while they retain the slave and prevent reader EOF.
        wait_for(&engine, |screen| !screen.alive);
        let pid: i32 = std::fs::read_to_string(directory.path().join("background.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rustix::process::test_kill_process(pid).is_ok() {
            assert!(
                Instant::now() < deadline,
                "background job survived natural shell exit"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            engine.process.child.lock().unwrap().is_none(),
            "shell was not reaped"
        );
        engine.kill().unwrap(); // Idempotent close after natural reaping.
    }
    #[test]
    #[cfg(unix)]
    fn cwd_follows_the_foreground_process() {
        let start = tempfile::tempdir().unwrap();
        let next = tempfile::tempdir().unwrap();
        let canonical = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        let shell = ShellSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "read target; cd \"$target\"; sleep 30".into()],
            cwd: Some(start.path().to_string_lossy().into_owned()),
        };
        let engine = Engine::new(&shell, Size::default(), 0, Arc::new(|| {})).unwrap();
        let read = |engine: &Engine| engine.cwd().map(|c| canonical(std::path::Path::new(&c)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while read(&engine) != Some(canonical(start.path())) {
            assert!(Instant::now() < deadline, "initial cwd {:?}", engine.cwd());
            thread::sleep(Duration::from_millis(20));
        }
        engine
            .input(format!("{}\r", next.path().display()).as_bytes())
            .unwrap();
        while read(&engine) != Some(canonical(next.path())) {
            assert!(Instant::now() < deadline, "changed cwd {:?}", engine.cwd());
            thread::sleep(Duration::from_millis(20));
        }
        engine.kill().unwrap();
        assert_eq!(engine.cwd(), None, "dead panes report no cwd");
    }
}
