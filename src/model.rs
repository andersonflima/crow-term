use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}
impl Default for Size {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}
/// Character cell size in physical pixels, reported by the GUI that renders a pane.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CellPixels {
    pub width: u16,
    pub height: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ColorSpec {
    Foreground,
    Background,
    Indexed(u8),
    Rgb(u8, u8, u8),
}
/// SGR 4 / 4:x underline variants. `None` when the cell is not underlined.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Cell {
    pub text: String,
    pub fg: ColorSpec,
    pub bg: ColorSpec,
    pub bold: bool,
    pub italic: bool,
    /// Any underline style; kept for consumers of the JSON capture.
    pub underline: bool,
    pub strike: bool,
    pub wide_spacer: bool,
    #[serde(default)]
    pub underline_style: UnderlineStyle,
    /// SGR 58 underline color; `None` draws in the foreground color.
    #[serde(default)]
    pub underline_color: Option<ColorSpec>,
    /// OSC 8 hyperlink: index into `Screen::links`.
    #[serde(default)]
    pub link: Option<u16>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    pub shape: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Modes {
    pub bracketed_paste: bool,
    pub application_cursor: bool,
    #[serde(default)]
    pub application_keypad: bool,
    pub mouse_sgr: bool,
    pub mouse_motion: bool,
    #[serde(default)]
    pub mouse_drag: bool,
    pub mouse_click: bool,
    pub focus_reporting: bool,
    /// Active kitty keyboard enhancement flags; 0 means legacy encoding.
    #[serde(default)]
    pub keyboard: u8,
    /// xterm modifyOtherKeys level requested by the application (0-2).
    #[serde(default)]
    pub modify_other_keys: u8,
    /// Alternate screen with DEC 1007 alternate scroll: the wheel sends arrows.
    #[serde(default)]
    pub alternate_scroll: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Screen {
    pub size: Size,
    pub cells: Vec<Vec<Cell>>,
    pub cursor: Cursor,
    pub modes: Modes,
    pub title: String,
    pub alive: bool,
    pub display_offset: usize,
    /// Bell (BEL) count since the pane started; a change means "ring".
    #[serde(default)]
    pub bell: u64,
    /// OSC 8 hyperlink targets referenced by `Cell::link`.
    #[serde(default)]
    pub links: Vec<String>,
    /// OSC 52 copies accepted from this pane (only with `terminal.osc52 = "copy"`);
    /// a change means a new text is available through `Request::Clipboard`.
    #[serde(default)]
    pub clipboard_seq: u64,
    /// `wrapped[row]`: the row continues on the next one (soft wrap), so
    /// copying and link detection join them without a line break.
    #[serde(default)]
    pub wrapped: Vec<bool>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Layout {
    Pane(u64),
    Split {
        axis: Axis,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}
impl Layout {
    pub fn panes(&self) -> Vec<u64> {
        match self {
            Self::Pane(id) => vec![*id],
            Self::Split { first, second, .. } => {
                let mut p = first.panes();
                p.extend(second.panes());
                p
            }
        }
    }
    pub fn split(&mut self, target: u64, new: u64, axis: Axis) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    axis,
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(new)),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.split(target, new, axis) || second.split(target, new, axis)
            }
            _ => false,
        }
    }
    pub fn remove(self, target: u64) -> Option<Self> {
        match self {
            Self::Pane(id) => {
                if id == target {
                    None
                } else {
                    Some(Self::Pane(id))
                }
            }
            Self::Split {
                axis,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(a), Some(b)) => Some(Self::Split {
                    axis,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (a, b) => a.or(b),
            },
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub name: String,
    pub layout: Layout,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    pub windows: Vec<WindowInfo>,
}
/// What a pane runs, for tab labels and the status bar (wire v8).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PaneSummary {
    /// The program's title (OSC 0/2), empty when it set none.
    pub title: String,
    /// Executable name of the foreground process ("nvim", "-zsh").
    pub process: String,
    /// At most one argument, only when it cannot be a secret (see
    /// `tab_context::safe_args`); filtered by the daemon.
    pub args: Vec<String>,
    pub pid: Option<u32>,
    /// The pane's terminal device (`/dev/ttys004`), empty when unknown.
    pub tty: String,
    pub cwd: Option<String>,
    /// (repository name, branch) of `cwd`, read from `.git` files only.
    pub repo: Option<(String, String)>,
    /// The agent CLI in the foreground, recognized by the daemon from the
    /// program name or the script a runtime runs (argv itself is never sent).
    pub agent: Option<String>,
    /// Process ids of the foreground job and its descendants (bounded).
    pub pids: Vec<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    /// Random per daemon process (wire v6): commands carry it so a restarted
    /// daemon, whose pane ids start over, rejects commands meant for its
    /// predecessor.
    pub instance: u64,
    pub revision: u64,
    pub sessions: Vec<SessionInfo>,
    pub screens: Vec<(u64, Screen)>,
    /// Summaries of the watched session's panes (all its windows), sent
    /// with screens; empty means "unchanged".
    #[serde(default)]
    pub panes: Vec<(u64, PaneSummary)>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    List,
    Watch {
        session: String,
        window: u64,
        since: u64,
    },
    Ensure {
        session: String,
        shell: ShellSpec,
        size: Size,
    },
    NewWindow {
        session: String,
        shell: ShellSpec,
        size: Size,
    },
    Split {
        session: String,
        window: u64,
        pane: u64,
        axis: Axis,
        shell: ShellSpec,
        size: Size,
    },
    Input {
        pane: u64,
        bytes: Vec<u8>,
    },
    Resize {
        pane: u64,
        size: Size,
        /// Cell metrics of the GUI rendering this pane (pixel size reports).
        cell: Option<CellPixels>,
    },
    Scroll {
        pane: u64,
        lines: i32,
    },
    ClosePane {
        session: String,
        window: u64,
        pane: u64,
    },
    RenameWindow {
        session: String,
        window: u64,
        name: String,
    },
    KillSession {
        session: String,
    },
    Shutdown {
        force: bool,
    },
    /// Working directory of a pane's foreground process (status bar git branch).
    /// Appended last: earlier variant indices of the internal wire format are unchanged.
    PaneCwd {
        pane: u64,
    },
    /// Latest OSC 52 copy of a pane (appended last, wire v5).
    Clipboard {
        pane: u64,
    },
    /// `request` only if the daemon is still the instance the client saw
    /// (wire v6). GUIs send every pane- or window-addressed command this way.
    Guarded {
        instance: u64,
        request: Box<Request>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Created {
        window: u64,
        pane: u64,
    },
    View(View),
    Error(String),
    Cwd(Option<String>),
    /// `(clipboard_seq, text)` of the latest accepted OSC 52 copy.
    Clipboard(Option<(u64, String)>),
}
