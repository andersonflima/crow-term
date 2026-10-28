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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ColorSpec {
    Foreground,
    Background,
    Indexed(u8),
    Rgb(u8, u8, u8),
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub fg: ColorSpec,
    pub bg: ColorSpec,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub wide_spacer: bool,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub revision: u64,
    pub sessions: Vec<SessionInfo>,
    pub screens: Vec<(u64, Screen)>,
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
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Created { window: u64, pane: u64 },
    View(View),
    Error(String),
}
