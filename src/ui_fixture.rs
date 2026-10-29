//! Offscreen frames of the real UI composition for renderer goldens. Content
//! uses only the bundled Crowquill Mono faces (ASCII, ligatures, styles), so the
//! committed goldens do not depend on the platform's fallback fonts.
use super::*;
use crate::crow::{CrowView, Segment, Tone};
use crate::render::golden;

pub(crate) fn cell(text: &str) -> Cell {
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
fn line(text: &str, cols: usize) -> Vec<Cell> {
    let mut out: Vec<Cell> = text.chars().map(|ch| cell(&ch.to_string())).collect();
    out.resize(cols, cell(" "));
    out
}
/// Rows exercising every cell attribute the renderer draws.
pub(crate) fn styled_rows(cols: usize) -> Vec<Vec<Cell>> {
    let mut rows = vec![line("Hello, world! -> != === <= >= :: www |> <|", cols)];
    let mut styled = line("bold italic both strike under", cols);
    for (i, cell) in styled.iter_mut().enumerate() {
        match i {
            0..=3 => cell.bold = true,
            5..=10 => cell.italic = true,
            12..=15 => {
                cell.bold = true;
                cell.italic = true;
            }
            17..=22 => cell.strike = true,
            24..=28 => cell.underline = true,
            _ => {}
        }
    }
    rows.push(styled);
    let mut colors = line("ansi 0123456789abcdef rgb truecolor 256", cols);
    for (i, cell) in colors.iter_mut().enumerate() {
        if (5..21).contains(&i) {
            cell.fg = ColorSpec::Indexed((i - 5) as u8);
            cell.bg = ColorSpec::Indexed(((i - 5) as u8 + 8) % 16);
        }
        if (22..25).contains(&i) {
            cell.fg = ColorSpec::Rgb(200, 30, 90);
        }
        if (26..35).contains(&i) {
            cell.bg = ColorSpec::Rgb(10, 120, 200);
            cell.fg = ColorSpec::Rgb(250, 250, 250);
        }
        if (36..39).contains(&i) {
            cell.fg = ColorSpec::Indexed(196);
            cell.bg = ColorSpec::Indexed(240);
        }
    }
    rows.push(colors);
    let mut unders = line("single double curly dotted dashed red", cols);
    for (range, style, color) in [
        (0..6, UnderlineStyle::Single, None),
        (
            7..13,
            UnderlineStyle::Double,
            Some(ColorSpec::Rgb(0, 128, 255)),
        ),
        (
            14..19,
            UnderlineStyle::Curly,
            Some(ColorSpec::Rgb(255, 0, 0)),
        ),
        (20..26, UnderlineStyle::Dotted, None),
        (27..33, UnderlineStyle::Dashed, Some(ColorSpec::Indexed(2))),
        (34..37, UnderlineStyle::Curly, None),
    ] {
        for i in range {
            unders[i].underline = true;
            unders[i].underline_style = style;
            unders[i].underline_color = color;
        }
    }
    rows.push(unders);
    rows.push(line(
        "$ ls -la ~/projects/crow-term && cargo test --all",
        cols,
    ));
    for i in 0..4 {
        rows.push(line(
            &format!("line {i}: the quick brown fox jumps over the lazy dog"),
            cols,
        ));
    }
    rows
}
pub(crate) fn screen(rows: Vec<Vec<Cell>>, cursor: (usize, usize), shape: &str) -> Screen {
    let cols = rows.first().map_or(0, Vec::len) as u16;
    Screen {
        size: Size {
            rows: rows.len() as u16,
            cols,
        },
        cells: rows,
        cursor: Cursor {
            row: cursor.0,
            col: cursor.1,
            visible: true,
            shape: shape.into(),
        },
        modes: Modes::default(),
        title: String::new(),
        alive: true,
        display_offset: 0,
        bell: 0,
        links: Vec::new(),
        clipboard_seq: 0,
    }
}
/// An app with two windows (tabs) and a vertical split, never connected to a mux.
pub(crate) fn app(config: Config, scale: f64, size: PhysicalSize<u32>) -> App {
    // No mux in offscreen tests: requests are answered locally and events
    // have nowhere to go.
    let tx = Arc::new(crate::lanes::Dispatcher::new(
        Arc::new(AtomicBool::new(false)),
        |_, _| Ok(Response::Ok),
        |_| crate::lanes::Outcome::Failed,
        |_| false,
    ));
    let mut app = App::new(
        config,
        "main".into(),
        1,
        11,
        ShellSpec {
            program: "/bin/sh".into(),
            args: vec![],
            cwd: None,
        },
        tx,
        Arc::new(AtomicU64::new(1)),
        Arc::new(AtomicBool::new(true)),
        Arc::new(ViewMailbox::default()),
        None,
    );
    app.scale = scale;
    app.shaper.rescale(&app.config, scale);
    app.software.invalidate();
    app.size_override = Some(size);
    // ASCII status: the default hint uses U+00B7, which may come from a fallback font.
    app.status = "status: offscreen golden".into();
    app.view = Some(View {
        instance: 1,
        revision: 7,
        sessions: vec![SessionInfo {
            name: "main".into(),
            windows: vec![
                WindowInfo {
                    id: 1,
                    name: "editor".into(),
                    layout: Layout::Split {
                        axis: Axis::Vertical,
                        first: Box::new(Layout::Pane(11)),
                        second: Box::new(Layout::Pane(12)),
                    },
                },
                WindowInfo {
                    id: 2,
                    name: "logs".into(),
                    layout: Layout::Pane(21),
                },
            ],
        }],
        screens: vec![
            (11, screen(styled_rows(48), (4, 7), "Block")),
            (12, screen(styled_rows(40), (0, 0), "Block")),
        ],
    });
    app
}
pub(crate) fn dark() -> Config {
    Config {
        background: "#111111".into(),
        foreground: "#EAEAEA".into(),
        selection: "#3A3A5A".into(),
        cursor: "#F0F0F0".into(),
        ..Config::default()
    }
}
pub(crate) fn software_frame(app: &mut App) -> (Vec<u32>, u32, u32) {
    let size = app.window_size().unwrap();
    let mut frame = vec![0u32; (size.width * size.height) as usize];
    app.paint(&mut frame, size);
    (frame, size.width, size.height)
}
/// Named fixture states: (name, app). Each covers different ops and clipping.
pub(crate) fn cases() -> Vec<(&'static str, App)> {
    let mut out = Vec::new();
    let mut base = app(Config::default(), 1.0, PhysicalSize::new(800, 480));
    base.selection = Some(Selection {
        pane: 11,
        start: Point { row: 6, col: 30 },
        end: Point { row: 2, col: 4 },
    });
    base.ime = "abc".into();
    out.push(("light-selection-block-ime", base));
    let mut hidpi = app(dark(), 2.0, PhysicalSize::new(1000, 640));
    if let Some(view) = &mut hidpi.view {
        view.screens[0].1.cursor.shape = "Beam".into();
    }
    out.push(("dark-scale2-beam", hidpi));
    let mut panel = app(Config::default(), 1.0, PhysicalSize::new(900, 420));
    panel.prefix = true;
    panel.crow_panel = true;
    panel.bell_flash = Some((
        Instant::now() + Duration::from_secs(600),
        HashSet::from([12u64]),
    ));
    panel.crow_view = CrowView {
        status: vec![Segment {
            text: "crow on".into(),
            tone: Tone::On,
        }],
        panel: vec![
            vec![Segment {
                text: "AGENTS".into(),
                tone: Tone::Plain,
            }],
            vec![
                Segment {
                    text: "claude".into(),
                    tone: Tone::Accent,
                },
                Segment {
                    text: "running".into(),
                    tone: Tone::On,
                },
            ],
            vec![
                Segment {
                    text: "codex".into(),
                    tone: Tone::Plain,
                },
                Segment {
                    text: "failed".into(),
                    tone: Tone::Alert,
                },
            ],
        ],
    };
    if let Some(view) = &mut panel.view {
        view.screens[0].1.cursor.shape = "Underline".into();
    }
    out.push(("light-panel-prefix-bell-underline", panel));
    let mut tiny = app(
        Config {
            ligatures: false,
            ..dark()
        },
        1.0,
        PhysicalSize::new(261, 161),
    );
    if let Some(view) = &mut tiny.view {
        view.screens[0].1.cursor.shape = "HollowBlock".into();
        view.screens[0].1.cursor.row = 1;
    }
    out.push(("dark-tiny-hollow-noligatures", tiny));
    out
}

/// The software backend's output for the bundled-font fixture is pinned by
/// committed goldens (generated from the pre-extraction renderer, which was
/// byte-identical on 52 fixture frames). Update procedure: run this test, review
/// the candidate PNG it reports, then copy it into `tests/goldens/render/`.
#[test]
fn software_frames_match_committed_goldens() {
    for (name, mut app) in cases() {
        let (frame, w, h) = software_frame(&mut app);
        // A second frame from warm row/layer caches is identical.
        assert_eq!(
            software_frame(&mut app).0,
            frame,
            "{name}: warm frame differs"
        );
        golden::assert_golden(&format!("software-{name}"), &frame, w, h);
    }
}
