//! `crow-term bench-render` (hidden): offscreen frame timings of the software
//! and GPU backends on synthetic terminal grids. Not a stable interface.
use super::gpu::GpuRenderer;
use super::software::SoftwareRenderer;
use super::{GridShaper, Op, Rect, Scene};
use crate::config::Config;
use crate::model::{Cell, ColorSpec, UnderlineStyle};
use anyhow::{Result, bail};
use std::borrow::Cow;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    Software,
    Gpu,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scenario {
    /// The same frame again (every row cached): cost of a redraw request.
    Idle,
    /// Every row changes each frame (output scrolling by one line).
    Scroll,
    /// One row changes each frame (typing, a clock, a progress line).
    OneRow,
}

/// Deterministic terminal-looking text with some colors and attributes.
fn grid_line(seed: usize, cols: usize) -> Vec<Cell> {
    const WORDS: [&str; 12] = [
        "fn",
        "let",
        "match",
        "=>",
        "->",
        "Some(x)",
        "self.cache",
        "// note",
        "0x1f",
        "!=",
        "render",
        "{}",
    ];
    let mut text = String::new();
    let mut i = seed;
    while text.chars().count() < cols {
        text.push_str(WORDS[i % WORDS.len()]);
        text.push(' ');
        i = i.wrapping_mul(31).wrapping_add(7);
    }
    text.chars()
        .take(cols)
        .enumerate()
        .map(|(col, ch)| {
            let tint = (seed + col / 7) % 9;
            Cell {
                text: ch.to_string(),
                fg: if tint < 6 {
                    ColorSpec::Foreground
                } else {
                    ColorSpec::Indexed(tint as u8)
                },
                bg: if tint == 8 {
                    ColorSpec::Indexed(8)
                } else {
                    ColorSpec::Background
                },
                bold: tint == 3,
                italic: tint == 4,
                underline: tint == 5,
                strike: false,
                wide_spacer: false,
                underline_style: if tint == 5 {
                    UnderlineStyle::Curly
                } else {
                    UnderlineStyle::None
                },
                underline_color: None,
                link: None,
            }
        })
        .collect()
}

pub struct Report {
    pub backend: Backend,
    pub adapter: String,
    pub cols: usize,
    pub rows: usize,
    pub width: u32,
    pub height: u32,
    pub first_frame: Duration,
    pub results: Vec<(Scenario, Vec<Duration>)>,
}

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index].as_secs_f64() * 1000.0
}

impl Report {
    pub fn to_json(&self) -> serde_json::Value {
        let scenarios: Vec<_> = self
            .results
            .iter()
            .map(|(scenario, samples)| {
                let mut sorted = samples.clone();
                sorted.sort();
                serde_json::json!({
                    "scenario": format!("{scenario:?}").to_lowercase(),
                    "frames": sorted.len(),
                    "p50_ms": percentile(&sorted, 0.50),
                    "p95_ms": percentile(&sorted, 0.95),
                    "p99_ms": percentile(&sorted, 0.99),
                    "max_ms": percentile(&sorted, 1.0),
                })
            })
            .collect();
        serde_json::json!({
            "schema": "crow-bench-render/1",
            "backend": format!("{:?}", self.backend).to_lowercase(),
            "adapter": self.adapter,
            "grid": format!("{}x{}", self.cols, self.rows),
            "pixels": format!("{}x{}", self.width, self.height),
            "first_frame_ms": self.first_frame.as_secs_f64() * 1000.0,
            "scenarios": scenarios,
            "rss_mib": rss_mib(),
        })
    }
}

/// Resident set size of this process, MiB (sysinfo).
pub fn rss_mib() -> f64 {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};
    let Ok(pid) = get_current_pid() else {
        return 0.0;
    };
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    system
        .process(pid)
        .map_or(0.0, |process| process.memory() as f64 / (1024.0 * 1024.0))
}

enum Renderer {
    Software(SoftwareRenderer, Vec<u32>),
    Gpu(Box<GpuRenderer>),
}
impl Renderer {
    fn frame(&mut self, scene: &Scene<'_>, shaper: &mut GridShaper, config: &Config) -> Result<()> {
        match self {
            Self::Software(software, frame) => {
                frame.resize((scene.width * scene.height) as usize, 0);
                software.render(scene, shaper, config, frame);
                std::hint::black_box(&frame);
                Ok(())
            }
            Self::Gpu(gpu) => gpu.render_offscreen_wait(scene, shaper, config),
        }
    }
}

/// Time `frames` frames per scenario on a `cols` x `rows` grid.
pub fn run(
    config: &Config,
    backend: Backend,
    cols: usize,
    rows: usize,
    frames: usize,
    scenarios: &[Scenario],
) -> Result<Report> {
    if cols == 0 || rows == 0 || frames == 0 {
        bail!("grid and frame count must be positive");
    }
    let mut shaper = GridShaper::new(config, 1.0);
    let (cw, ch) = (shaper.cell_width, shaper.cell_height);
    let (width, height) = (cols as u32 * cw, rows as u32 * ch);
    let (mut renderer, adapter) = match backend {
        Backend::Software => (
            Renderer::Software(SoftwareRenderer::default(), Vec::new()),
            "cpu framebuffer".to_owned(),
        ),
        Backend::Gpu => {
            let gpu = GpuRenderer::headless(true)?;
            let adapter = gpu.adapter().to_string();
            (Renderer::Gpu(Box::new(gpu)), adapter)
        }
    };
    let lines: Vec<Vec<Cell>> = (0..rows + frames + 1)
        .map(|seed| grid_line(seed, cols))
        .collect();
    let bounds = Rect {
        x: 0,
        y: 0,
        w: width,
        h: height,
    };
    let scene = |grid: &[Vec<Cell>]| -> Scene<'static> {
        let mut ops: Vec<Op<'static>> = grid
            .iter()
            .enumerate()
            .map(|(row, cells)| Op::Row {
                pane: 1,
                row,
                // Owned per frame: the copy is part of every backend's cost alike.
                cells: Cow::Owned(cells.clone()),
                x: 0,
                y: (row as u32 * ch) as i32,
                clip: bounds,
            })
            .collect();
        ops.push(Op::Invert {
            rect: Rect {
                x: (cw * 4) as i32,
                y: (ch * (rows as u32 - 1)) as i32,
                w: cw,
                h: ch,
            },
        });
        Scene {
            width,
            height,
            background: super::parse_color(&config.background, 0x111111),
            ops,
        }
    };
    let base: Vec<Vec<Cell>> = lines[..rows].to_vec();
    let started = Instant::now();
    renderer.frame(&scene(&base), &mut shaper, config)?;
    let first_frame = started.elapsed();
    let mut results = Vec::new();
    for &scenario in scenarios {
        // Same warm starting state for every scenario.
        let mut grid = base.clone();
        renderer.frame(&scene(&grid), &mut shaper, config)?;
        let mut samples = Vec::with_capacity(frames);
        for frame in 0..frames {
            match scenario {
                Scenario::Idle => {}
                Scenario::Scroll => {
                    grid.remove(0);
                    grid.push(lines[(rows + frame) % lines.len()].clone());
                }
                Scenario::OneRow => {
                    grid[frame % rows] = lines[(frame * 7 + 3) % lines.len()].clone();
                }
            }
            let current = scene(&grid);
            let start = Instant::now();
            renderer.frame(&current, &mut shaper, config)?;
            samples.push(start.elapsed());
        }
        results.push((scenario, samples));
    }
    Ok(Report {
        backend,
        adapter,
        cols,
        rows,
        width,
        height,
        first_frame,
        results,
    })
}
