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
    // The first four goldens pin the renderer itself (they predate
    // minimum_contrast and mix palette colors on palette backgrounds), so they
    // run with the adjustment off; `light-minimum-contrast` covers it.
    let mut base = app(pinned(Config::default()), 1.0, PhysicalSize::new(800, 480));
    base.selection = Some(Selection {
        pane: 11,
        start: Point { row: 6, col: 30 },
        end: Point { row: 2, col: 4 },
    });
    base.ime = "abc".into();
    out.push(("light-selection-block-ime", base));
    let mut hidpi = app(pinned(dark()), 2.0, PhysicalSize::new(1000, 640));
    if let Some(view) = &mut hidpi.view {
        view.screens[0].1.cursor.shape = "Beam".into();
    }
    out.push(("dark-scale2-beam", hidpi));
    let mut panel = app(pinned(Config::default()), 1.0, PhysicalSize::new(900, 420));
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
        pinned(Config {
            ligatures: false,
            ..dark()
        }),
        1.0,
        PhysicalSize::new(261, 161),
    );
    if let Some(view) = &mut tiny.view {
        view.screens[0].1.cursor.shape = "HollowBlock".into();
        view.screens[0].1.cursor.row = 1;
    }
    out.push(("dark-tiny-hollow-noligatures", tiny));
    // Default config (minimum_contrast 4.5) with a dark truecolor theme on the
    // light background: the reported "typed text is unreadable" case.
    let mut contrast = app(Config::default(), 1.0, PhysicalSize::new(800, 420));
    if let Some(view) = &mut contrast.view {
        view.screens[0].1.cells = dark_theme_rows(48);
        view.screens[1].1.cells = dark_theme_rows(40);
    }
    out.push(("light-minimum-contrast", contrast));
    out
}
fn pinned(config: Config) -> Config {
    Config {
        minimum_contrast: 1.0,
        ..config
    }
}
fn painted(text: &str, rgb: u32) -> Vec<Cell> {
    text.chars()
        .map(|ch| Cell {
            fg: ColorSpec::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8),
            ..cell(&ch.to_string())
        })
        .collect()
}
/// A fish prompt line as drawn by a Catppuccin Macchiato (dark) theme in
/// truecolor, plus default-colored text and box drawing.
pub(crate) fn dark_theme_rows(cols: usize) -> Vec<Vec<Cell>> {
    let mut typed = painted("ls ", 0x8aadf4);
    typed.extend(painted("-la ~/projects ", 0xf0c6c6));
    typed.extend(painted("| grep crow", 0xcad3f5));
    typed.extend(painted("  # suggestion", 0x6e738d));
    typed.resize(cols, cell(" "));
    let mut boxes = painted("\u{2500}\u{2500}\u{2502}\u{2588}", 0xcad3f5);
    boxes.resize(cols, cell(" "));
    let mut default = line("default #444444 text stays as it was", cols);
    default.truncate(cols);
    vec![typed, boxes, default, line("$ echo done", cols)]
}

/// The software backend's output for the bundled-font fixture is pinned by
/// committed goldens (generated from the pre-extraction renderer, which was
/// byte-identical on 52 fixture frames). Update procedure: run this test, review
/// the candidate PNG it reports, then copy it into `tests/goldens/render/`.
#[test]
fn minimum_contrast_makes_dark_theme_text_readable_and_keeps_default_text() {
    let config = Config::default();
    assert_eq!(config.minimum_contrast, 4.5);
    let mut shaper = GridShaper::new(&config, 1.0);
    let rows = dark_theme_rows(48);
    let shaped = shaper.shape_row(&config, &rows[0]);
    assert!(!shaped.glyphs.is_empty());
    for glyph in &shaped.glyphs {
        let ratio = crate::render::contrast_ratio(glyph.color.0 & 0xff_ffff, 0xffffff);
        assert!(
            ratio >= 4.5,
            "{:06x}: {ratio:.2}",
            glyph.color.0 & 0xff_ffff
        );
    }
    // Box drawing keeps its color; default text is byte-identical with the
    // adjustment off.
    let boxes = shaper.shape_row(&config, &rows[1]);
    let box_glyphs: Vec<_> = boxes
        .glyphs
        .iter()
        .filter(|g| g.x < 4 * shaper.cell_width as i32)
        .collect();
    assert!(!box_glyphs.is_empty());
    assert!(
        box_glyphs.iter().all(|g| g.color.0 & 0xff_ffff == 0xcad3f5),
        "{box_glyphs:?}"
    );
    let off = pinned(Config::default());
    for row in [&rows[2], &rows[3]] {
        let on =
            crate::render::software::rasterize_row(&shaper.shape_row(&config, row), &mut shaper);
        let mut other = GridShaper::new(&off, 1.0);
        let plain = crate::render::software::rasterize_row(&other.shape_row(&off, row), &mut other);
        assert_eq!(on, plain);
    }
}

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

/// GPU adapter for comparison tests. Without one the tests are skipped with
/// the reason on stderr, unless `CROW_TERM_GPU_TESTS=1` requires it (CI GPU jobs).
pub(crate) fn gpu_or_skip(test: &str) -> Option<crate::render::gpu::GpuRenderer> {
    let required = std::env::var("CROW_TERM_GPU_TESTS").is_ok_and(|value| value == "1");
    match crate::render::gpu::GpuRenderer::headless(true) {
        Ok(gpu) => {
            eprintln!("{test}: GPU adapter {}", gpu.adapter());
            Some(gpu)
        }
        Err(error) if required => panic!("{test}: CROW_TERM_GPU_TESTS=1 but no GPU: {error:#}"),
        Err(error) => {
            eprintln!("{test}: skipped, no GPU adapter: {error:#}");
            None
        }
    }
}
/// Tolerance for GPU-vs-software comparisons on this adapter.
pub(crate) fn gpu_tolerance(gpu: &crate::render::gpu::GpuRenderer) -> golden::Tolerance {
    if gpu.adapter().cpu {
        golden::CPU_ADAPTER
    } else {
        golden::RENDERER
    }
}
pub(crate) fn gpu_frame(app: &mut App, gpu: &mut crate::render::gpu::GpuRenderer) -> Vec<u32> {
    let size = app.window_size().unwrap();
    let scene = scene_of!(app, size);
    gpu.render_offscreen(&scene, &mut app.shaper, &app.config)
        .expect("GPU offscreen frame")
}
/// Platform fallback fonts (CJK, combining marks, color emoji, Nerd icons when
/// installed): not in committed goldens, but both backends must agree.
fn fallback_rows(cols: usize) -> Vec<Vec<Cell>> {
    let mut rows = styled_rows(cols);
    let mut wide = Vec::new();
    for text in [
        "界", "", "e\u{301}", " ", "😀", "", " ", "\u{E702}", " ", "─", "│", "┌", "┐", " ", "λ",
        "→", " ", "🦀", "",
    ] {
        let mut c = cell(text);
        c.wide_spacer = text.is_empty();
        wide.push(c);
    }
    wide.resize(cols, cell(" "));
    rows.insert(4, wide);
    rows
}

#[test]
fn gpu_frames_match_software_within_tolerance() {
    let Some(mut gpu) = gpu_or_skip("gpu_frames_match_software_within_tolerance") else {
        return;
    };
    let mut cases = cases();
    let mut fallback = app(Config::default(), 2.0, PhysicalSize::new(1200, 700));
    if let Some(view) = &mut fallback.view {
        view.screens[0].1.cells = fallback_rows(48);
        view.screens[1].1.cells = fallback_rows(40);
    }
    fallback.ime = "かな".into();
    cases.push(("fallback-fonts-scale2", fallback));
    for (index, (name, mut app)) in cases.into_iter().enumerate() {
        // Each fixture app has its own font system (font ids and glyph cache
        // keys are per font system), so each gets its own GPU renderer, as in
        // the application (one shaper per renderer).
        if index > 0 {
            gpu = gpu_or_skip(name).expect("GPU adapter disappeared");
        }
        let (expected, w, h) = software_frame(&mut app);
        let actual = gpu_frame(&mut app, &mut gpu);
        let tolerance = gpu_tolerance(&gpu);
        let diff = golden::diff(&actual, &expected, tolerance);
        eprintln!("{name}: {diff:?}");
        if !diff.within(tolerance) {
            golden::write_png(
                &golden::candidate_path(&format!("gpu-{name}")),
                &actual,
                w,
                h,
            );
            golden::write_png(
                &golden::candidate_path(&format!("cpu-{name}")),
                &expected,
                w,
                h,
            );
            panic!(
                "{name}: GPU frame differs from software beyond tolerance: {diff:?}; see {}",
                golden::candidate_path(&format!("gpu-{name}")).display()
            );
        }
        // Warm GPU caches (rows, layer, atlas) draw the same frame again.
        let again = gpu_frame(&mut app, &mut gpu);
        assert_eq!(again, actual, "{name}: warm GPU frame differs");
    }
}

#[test]
fn gpu_atlas_growth_and_row_changes_keep_frames_correct() {
    let Some(mut gpu) = gpu_or_skip("gpu_atlas_growth_and_row_changes_keep_frames_correct") else {
        return;
    };
    let tolerance = gpu_tolerance(&gpu);
    // Tiny first pages: the first frame must grow them several times mid-frame
    // and rebuild; changed rows are reshaped, unchanged ones reused.
    gpu.set_atlas_limits(64, 8192);
    let (start, _, _) = gpu.atlas_state();
    let mut app = app(Config::default(), 3.0, PhysicalSize::new(1600, 1000));
    for step in 0..3 {
        if let Some(view) = &mut app.view {
            let rows = &mut view.screens[0].1.cells;
            rows[step] = line(&format!("changed row {step} !@#$%^&*()_+{{}}|:<>?~"), 48);
        }
        let (expected, _, _) = software_frame(&mut app);
        let actual = gpu_frame(&mut app, &mut gpu);
        let diff = golden::diff(&actual, &expected, tolerance);
        assert!(diff.within(tolerance), "step {step}: {diff:?}");
        assert_eq!(gpu.take_dropped_glyphs(), 0);
    }
    let (generation, mask, _) = gpu.atlas_state();
    assert!(
        generation >= start + 2 && mask >= 256,
        "atlas never grew: {:?}",
        gpu.atlas_state()
    );
}

#[test]
fn gpu_atlas_overflow_draws_the_rest_of_the_frame_and_recovers() {
    let Some(mut gpu) = gpu_or_skip("gpu_atlas_overflow_draws_the_rest_of_the_frame_and_recovers")
    else {
        return;
    };
    let tolerance = gpu_tolerance(&gpu);
    // Pages capped far below what one frame needs: glyphs that do not fit are
    // skipped, but fills, other glyphs and later ops are still drawn.
    gpu.set_atlas_limits(32, 64);
    let mut app = app(Config::default(), 2.0, PhysicalSize::new(1000, 640));
    let (expected, _, _) = software_frame(&mut app);
    let degraded = gpu_frame(&mut app, &mut gpu);
    assert!(gpu.take_dropped_glyphs() > 0);
    let diff = golden::diff(&degraded, &expected, tolerance);
    // Missing glyphs only: most of the frame (backgrounds, chrome) matches.
    assert!(
        (diff.differing as f64) < 0.1 * diff.total as f64,
        "frame mostly blank: {diff:?}"
    );
    // With room again the next frame is complete (skipped glyphs are not cached).
    gpu.set_atlas_limits(1024, 8192);
    let actual = gpu_frame(&mut app, &mut gpu);
    let diff = golden::diff(&actual, &expected, tolerance);
    assert!(diff.within(tolerance), "{diff:?}");
    assert_eq!(gpu.take_dropped_glyphs(), 0);
}

#[test]
fn gpu_failure_policy_recreates_once_then_ends() {
    use crate::config::RendererMode;
    assert_eq!(
        gpu_failure_action(RendererMode::Gpu, 1),
        GpuFailure::RecreateDevice
    );
    assert_eq!(gpu_failure_action(RendererMode::Gpu, 2), GpuFailure::Fatal);
    assert_eq!(gpu_failure_action(RendererMode::Gpu, 9), GpuFailure::Fatal);
    for failures in [1, 2, 9] {
        assert_eq!(
            gpu_failure_action(RendererMode::Auto, failures),
            GpuFailure::SwitchToSoftware
        );
    }
}

#[test]
fn skipped_gpu_frames_retry_with_capped_backoff_and_presenting_resets() {
    let mut app = app(Config::default(), 1.0, PhysicalSize::new(400, 300));
    let mut delays = Vec::new();
    for _ in 0..8 {
        let before = Instant::now();
        app.note_gpu_frame(false);
        let due = app.retry_due.expect("retry scheduled");
        delays.push((due - before).as_millis());
    }
    // 50, 100, 200, 400, 800, then capped at one second: never a busy loop.
    assert!(delays[0] >= 50 && delays[0] < 80, "{delays:?}");
    assert!(
        delays.windows(2).all(|pair| pair[1] >= pair[0]),
        "{delays:?}"
    );
    assert!(delays[7] >= 1000 && delays[7] < 1100, "{delays:?}");
    app.gpu_failures = 1;
    app.note_gpu_frame(true);
    assert_eq!((app.gpu_failures, app.retry_due), (0, None));
    assert_eq!(app.retry_delay, FRAME_RETRY_MIN);
}

#[test]
fn auto_falls_back_to_software_and_a_failed_fallback_ends_the_session() {
    use crate::config::RendererMode;
    let mut app = app(Config::default(), 1.0, PhysicalSize::new(400, 300));
    app.renderer_mode = RendererMode::Auto;
    let result = app.recover_from_gpu_error(
        anyhow::anyhow!("device lost"),
        || panic!("auto never recreates the GPU device"),
        |_| Ok(()),
    );
    assert!(result.is_ok());
    assert!(app.fatal_error.is_none());
    assert!(
        app.status.contains("switched to software"),
        "{}",
        app.status
    );
    // The software backend cannot start either: no frozen window, the
    // session ends with both errors.
    let result = app.recover_from_gpu_error(
        anyhow::anyhow!("device lost"),
        || panic!("auto never recreates the GPU device"),
        |_| Err(anyhow::anyhow!("no native surface")),
    );
    assert!(result.is_err());
    let fatal = app.fatal_error.clone().expect("fatal error set");
    assert!(
        fatal.contains("device lost") && fatal.contains("no native surface"),
        "{fatal}"
    );
}

#[test]
fn gpu_mode_recreates_the_device_once_per_presented_frame() {
    use crate::config::RendererMode;
    let mut app = app(Config::default(), 1.0, PhysicalSize::new(400, 300));
    app.renderer_mode = RendererMode::Gpu;
    // Recreation itself failing ends the session.
    let result = app.recover_from_gpu_error(
        anyhow::anyhow!("device lost"),
        || Err(anyhow::anyhow!("no adapter")),
        |_| panic!("gpu mode never switches to software"),
    );
    assert!(result.is_err());
    assert!(
        app.fatal_error
            .take()
            .is_some_and(|f| f.contains("no adapter"))
    );
    let Some(gpu) = gpu_or_skip("gpu_mode_recreates_the_device_once_per_presented_frame") else {
        return;
    };
    app.gpu_failures = 0;
    let result = app.recover_from_gpu_error(
        anyhow::anyhow!("device lost"),
        move || Ok(gpu),
        |_| panic!("gpu mode never switches to software"),
    );
    assert!(result.is_ok() && app.gpu.is_some() && app.fatal_error.is_none());
    // A second failure before any frame was presented must not loop on new
    // devices: it ends with the error.
    let result = app.recover_from_gpu_error(
        anyhow::anyhow!("device lost again"),
        || panic!("no second device without a presented frame"),
        |_| panic!("gpu mode never switches to software"),
    );
    assert!(result.is_err());
    assert!(
        app.fatal_error
            .as_deref()
            .is_some_and(|f| f.contains("device lost again"))
    );
}
