//! Read-only Crow agent status (`crow-skills events snapshot --json`).
//!
//! The source contract is versioned (`crow-snapshot/1`, `crow-session/1`,
//! `crow-resources/1`) and additive: unknown fields are ignored, missing or
//! mistyped fields degrade to "absent", and an unknown major version is reported
//! instead of being misread. The source never contains prompt, message or tool
//! I/O text; this module only renders ids, names, states, counters and times.
//!
//! Acquisition is cheap by construction: one worker thread runs at most one
//! subprocess at a time, only when the crow state files changed or the data is
//! stale, with a hard timeout, a stdout cap and bounded parsed collections.
use crate::config::CrowConfig;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Minimum spacing of snapshots while agents are working (files change constantly).
const BUSY_GAP: Duration = Duration::from_secs(5);
/// Retry spacing for the pane cwd request after repeated failures.
const CWD_RETRY: Duration = Duration::from_secs(60);
const MAX_STDOUT: u64 = 2 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SESSIONS: usize = 64;
const MAX_RUNS: usize = 16;
const MAX_AGENTS: usize = 16;
const MAX_TEXT: usize = 96;
/// Rows shown in the panel; the remainder is summarised as "+N more".
const PANEL_SESSIONS: usize = 12;
const PANEL_RUNS: usize = 3;
const PANEL_AGENTS: usize = 8;

// ---------------------------------------------------------------------------
// Parsed model

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentSession {
    pub host: String,
    pub state: String,
    pub blocked: bool,
    pub cwd: String,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub tool: Option<String>,
    pub last_action: Option<String>,
    pub subagents: u64,
    pub cost_usd: Option<f64>,
    pub context_percent: Option<f64>,
    pub tokens: Option<u64>,
    pub age_seconds: u64,
}
impl AgentSession {
    pub fn live(&self) -> bool {
        self.state != "ended"
    }
    fn rank(&self) -> u8 {
        match (self.blocked, self.state.as_str()) {
            (true, _) | (_, "blocked") => 0,
            (_, "error") => 1,
            (_, "working") => 2,
            (_, "idle") => 3,
            (_, "ended") => 5,
            _ => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunAgent {
    pub owner: String,
    pub activity: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Run {
    pub title: String,
    pub cwd: String,
    pub status: String,
    pub done: u64,
    pub failed: u64,
    pub running: u64,
    pub total: u64,
    pub agents: Vec<RunAgent>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Memory {
    /// `normal`, `warn`, `critical` or `unknown` (other values are kept verbatim).
    pub level: String,
    pub used_percent: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CrowState {
    /// `None` when the snapshot omits the mode.
    pub mode_on: Option<bool>,
    pub sessions: Vec<AgentSession>,
    pub runs: Vec<Run>,
    pub memory: Option<Memory>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    Json,
    NotOk,
    Unsupported(String),
}
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json => f.write_str("invalid snapshot JSON"),
            Self::NotOk => f.write_str("crow-skills reported an error"),
            Self::Unsupported(schema) => write!(f, "unsupported {schema}"),
        }
    }
}

fn clean(text: &str) -> String {
    let mut out: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect();
    if text.chars().filter(|c| !c.is_control()).count() > MAX_TEXT {
        out.pop();
        out.push('…');
    }
    out.trim().to_owned()
}
fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(clean)
        .filter(|s| !s.is_empty())
}
fn uint(value: &Value, key: &str) -> Option<u64> {
    let v = value.get(key)?;
    v.as_u64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as u64)
    })
}
fn float(value: &Value, key: &str) -> Option<f64> {
    value.get(key)?.as_f64().filter(|f| f.is_finite())
}
/// `name/1` or `name/1.2` → true for major 1. Missing schema is tolerated where
/// the parent document already carried a supported schema.
fn major_one(schema: &str, name: &str) -> bool {
    schema
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|version| version.split('.').next())
        == Some("1")
}

/// Parse the `crow-skills` JSON envelope (or a bare `crow-snapshot/1` object).
pub fn parse_snapshot(bytes: &[u8]) -> Result<CrowState, ParseError> {
    let root: Value = serde_json::from_slice(bytes).map_err(|_| ParseError::Json)?;
    let data = if root.get("data").is_some() || root.get("ok").is_some() {
        if root.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(ParseError::NotOk);
        }
        root.get("data").ok_or(ParseError::Json)?
    } else {
        &root
    };
    if !data.is_object() {
        return Err(ParseError::Json);
    }
    let schema = data.get("schema").and_then(Value::as_str).unwrap_or("");
    if !major_one(schema, "crow-snapshot") {
        return Err(ParseError::Unsupported(if schema.is_empty() {
            "snapshot without schema".into()
        } else {
            clean(schema)
        }));
    }
    let mode_on = data
        .get("mode")
        .and_then(|mode| mode.get("mode").or(Some(mode)))
        .and_then(Value::as_str)
        .and_then(|mode| match mode {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        });
    let sessions = data
        .get("sessions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.is_object())
                .filter(|item| {
                    item.get("schema")
                        .and_then(Value::as_str)
                        .is_none_or(|s| major_one(s, "crow-session"))
                })
                .take(MAX_SESSIONS)
                .map(parse_session)
                .collect()
        })
        .unwrap_or_default();
    let runs = data
        .get("runs")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.is_object())
                .take(MAX_RUNS)
                .map(parse_run)
                .collect()
        })
        .unwrap_or_default();
    let memory = data
        .get("resources")
        .filter(|r| {
            r.get("schema")
                .and_then(Value::as_str)
                .is_none_or(|s| major_one(s, "crow-resources"))
        })
        .and_then(|r| {
            let memory = r.get("memory");
            let used = memory.and_then(|m| float(m, "usedPercent"));
            let level = r.get("pressure").and_then(|p| text(p, "level"));
            (used.is_some() || level.is_some()).then(|| Memory {
                level: level.unwrap_or_else(|| "unknown".into()),
                used_percent: used.map(|u| u.clamp(0.0, 100.0)),
            })
        });
    Ok(CrowState {
        mode_on,
        sessions,
        runs,
        memory,
    })
}

fn parse_session(item: &Value) -> AgentSession {
    let usage = item.get("usage");
    let tokens = usage.and_then(|u| {
        let input = uint(u, "total_input_tokens");
        let output = uint(u, "total_output_tokens");
        (input.is_some() || output.is_some())
            .then(|| input.unwrap_or(0).saturating_add(output.unwrap_or(0)))
    });
    AgentSession {
        host: text(item, "host").unwrap_or_else(|| "?".into()),
        state: text(item, "state").unwrap_or_else(|| "unknown".into()),
        blocked: item.get("blocked").and_then(Value::as_bool) == Some(true),
        cwd: text(item, "cwd").unwrap_or_default(),
        branch: text(item, "branch"),
        model: text(item, "model"),
        tool: text(item, "currentTool"),
        last_action: text(item, "lastAction"),
        subagents: uint(item, "subagents").unwrap_or(0),
        cost_usd: float(item, "costUsd").filter(|c| *c >= 0.0),
        context_percent: usage
            .and_then(|u| float(u, "used_percentage"))
            .map(|p| p.clamp(0.0, 100.0)),
        tokens,
        age_seconds: uint(item, "ageSeconds").unwrap_or(0),
    }
}

fn parse_run(item: &Value) -> Run {
    Run {
        title: text(item, "title")
            .or_else(|| text(item, "id"))
            .unwrap_or_else(|| "run".into()),
        cwd: text(item, "cwd").unwrap_or_default(),
        status: text(item, "status").unwrap_or_else(|| "unknown".into()),
        done: uint(item, "done").unwrap_or(0),
        failed: uint(item, "failed").unwrap_or(0),
        running: uint(item, "running").unwrap_or(0),
        total: uint(item, "total").unwrap_or(0),
        agents: item
            .get("agents")
            .and_then(Value::as_array)
            .map(|agents| {
                agents
                    .iter()
                    .filter(|a| a.is_object())
                    .take(MAX_AGENTS)
                    .map(|a| RunAgent {
                        owner: text(a, "owner")
                            .or_else(|| text(a, "unit"))
                            .unwrap_or_else(|| "agent".into()),
                        activity: text(a, "activity").unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Presentation (pure; colours are chosen by the UI from the tone)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    Plain,
    Muted,
    On,
    Off,
    Accent,
    Warn,
    Alert,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Segment {
    pub text: String,
    pub tone: Tone,
}
fn seg(text: impl Into<String>, tone: Tone) -> Segment {
    Segment {
        text: text.into(),
        tone,
    }
}
pub type Line = Vec<Segment>;

/// What the worker publishes; the UI only paints it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct CrowView {
    pub status: Vec<Segment>,
    pub panel: Vec<Line>,
}

/// Snapshot availability as seen by the worker.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Not fetched yet.
    Pending,
    /// `crow-skills` not found: crow segments are hidden.
    Missing,
    Failed(String),
    Ready(CrowState),
}

pub fn age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}
fn tokens(count: u64) -> String {
    match count {
        0..1000 => format!("{count} tok"),
        1000..1_000_000 => format!("{}k tok", count / 1000),
        _ => format!("{:.1}M tok", count as f64 / 1e6),
    }
}
fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
fn basename(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}
fn is_ancestor(ancestor: &str, path: &str) -> bool {
    let ancestor = ancestor.trim_end_matches(['/', '\\']);
    !ancestor.is_empty()
        && path.starts_with(ancestor)
        && path[ancestor.len()..]
            .chars()
            .next()
            .is_none_or(|c| c == '/' || c == '\\')
}

/// Prefer the running crow-behaviour run whose directory contains the focused
/// pane's cwd (deepest first), then the first running run.
pub fn active_run<'a>(state: &'a CrowState, cwd: Option<&str>) -> Option<(&'a Run, usize)> {
    let running: Vec<&Run> = state
        .runs
        .iter()
        .filter(|r| r.status == "running")
        .collect();
    let chosen = cwd
        .and_then(|cwd| {
            running
                .iter()
                .filter(|r| is_ancestor(&r.cwd, cwd))
                .max_by_key(|r| r.cwd.len())
                .copied()
        })
        .or_else(|| running.first().copied())?;
    Some((chosen, running.len()))
}

/// The footer line: `▸▸ crow on · claude 2 · codex 1 · ⚠ 1 blocked · <run> 3/7 · mem 41% · ⎇ main`.
pub fn status_segments(source: &Source, branch: Option<&str>, cwd: Option<&str>) -> Vec<Segment> {
    let mut out = Vec::new();
    match source {
        Source::Pending | Source::Missing => {}
        Source::Failed(error) => {
            out.push(seg("▸▸ crow ?", Tone::Muted));
            out.push(seg(short(error, 40), Tone::Muted));
        }
        Source::Ready(state) => {
            out.push(match state.mode_on {
                Some(true) | None => seg("▸▸ crow on", Tone::On),
                Some(false) => seg("▸▸ crow off", Tone::Off),
            });
            let mut hosts: Vec<(String, usize)> = Vec::new();
            for session in state.sessions.iter().filter(|s| s.live()) {
                match hosts.iter_mut().find(|(host, _)| *host == session.host) {
                    Some((_, count)) => *count += 1,
                    None => hosts.push((session.host.clone(), 1)),
                }
            }
            if hosts.is_empty() {
                out.push(seg("no agents", Tone::Muted));
            } else {
                hosts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let text = hosts
                    .iter()
                    .map(|(host, count)| format!("{host} {count}"))
                    .collect::<Vec<_>>()
                    .join("  ");
                out.push(seg(text, Tone::Plain));
            }
            let blocked = state
                .sessions
                .iter()
                .filter(|s| s.blocked || s.state == "blocked")
                .count();
            if blocked > 0 {
                out.push(seg(format!("⚠ {blocked} blocked"), Tone::Alert));
            }
            let errors = state
                .sessions
                .iter()
                .filter(|s| s.state == "error" && !s.blocked)
                .count();
            if errors > 0 {
                out.push(seg(format!("✗ {errors} error"), Tone::Alert));
            }
            if let Some((run, count)) = active_run(state, cwd) {
                let mut text = format!("{} {}/{}", short(&run.title, 32), run.done, run.total);
                if count > 1 {
                    text.push_str(&format!(" +{}", count - 1));
                }
                out.push(seg(
                    text,
                    if run.failed > 0 {
                        Tone::Warn
                    } else {
                        Tone::Accent
                    },
                ));
            }
            if let Some(memory) = &state.memory {
                let percent = memory
                    .used_percent
                    .map(|p| format!(" {p:.0}%"))
                    .unwrap_or_default();
                out.push(match memory.level.as_str() {
                    "warn" => seg(format!("mem warn{percent}"), Tone::Warn),
                    "critical" => seg(format!("mem critical{percent}"), Tone::Alert),
                    _ => seg(format!("mem{percent}"), Tone::Muted),
                });
            }
        }
    }
    if let Some(branch) = branch {
        out.push(seg(format!("⎇ {}", short(branch, 40)), Tone::Plain));
    }
    out
}

fn session_glyph(session: &AgentSession) -> Segment {
    match (session.blocked, session.state.as_str()) {
        (true, _) | (_, "blocked") => seg("⚠", Tone::Alert),
        (_, "error") => seg("✗", Tone::Alert),
        (_, "working") => seg("●", Tone::On),
        (_, "ended") => seg("·", Tone::Muted),
        _ => seg("○", Tone::Muted),
    }
}

/// Claude-style agent list: per running run `● main  <title>  x/y`, `○ <owner>
/// <activity>` per running unit and a count line; then one entry per session.
pub fn panel_lines(source: &Source) -> Vec<Line> {
    let state = match source {
        Source::Ready(state) => state,
        Source::Pending => return vec![vec![seg("crow  loading…", Tone::Muted)]],
        Source::Missing => {
            return vec![
                vec![seg("crow-skills not found", Tone::Muted)],
                vec![seg("set crow.command in config.toml", Tone::Muted)],
            ];
        }
        Source::Failed(error) => {
            return vec![
                vec![seg("crow unavailable", Tone::Warn)],
                vec![seg(short(error, 60), Tone::Muted)],
            ];
        }
    };
    let mut lines: Vec<Line> = Vec::new();
    let live: Vec<&AgentSession> = state.sessions.iter().filter(|s| s.live()).collect();
    lines.push(vec![
        match state.mode_on {
            Some(false) => seg("▸▸ crow off", Tone::Off),
            _ => seg("▸▸ crow on", Tone::On),
        },
        seg(format!("{} live", live.len()), Tone::Muted),
    ]);
    let running: Vec<&Run> = state
        .runs
        .iter()
        .filter(|r| r.status == "running")
        .collect();
    for run in running.iter().take(PANEL_RUNS) {
        lines.push(Vec::new());
        lines.push(vec![
            seg("●", Tone::On),
            seg("main", Tone::Plain),
            seg(short(&run.title, 40), Tone::Plain),
            seg(format!("{}/{}", run.done, run.total), Tone::Accent),
        ]);
        for agent in run.agents.iter().take(PANEL_AGENTS) {
            lines.push(vec![
                seg("○", Tone::Muted),
                seg(short(&agent.owner, 20), Tone::Plain),
                seg(short(&agent.activity, 44), Tone::Muted),
            ]);
        }
        if run.agents.len() > PANEL_AGENTS {
            lines.push(vec![seg(
                format!("  +{} more", run.agents.len() - PANEL_AGENTS),
                Tone::Muted,
            )]);
        }
        let pending = run
            .total
            .saturating_sub(run.done + run.failed + run.running);
        lines.push(vec![
            seg(format!("✓ {} done", run.done), Tone::Muted),
            seg(
                format!("✗ {} failed", run.failed),
                if run.failed > 0 {
                    Tone::Alert
                } else {
                    Tone::Muted
                },
            ),
            seg(format!("{pending} pending"), Tone::Muted),
        ]);
    }
    if running.len() > PANEL_RUNS {
        lines.push(vec![seg(
            format!("+{} more runs", running.len() - PANEL_RUNS),
            Tone::Muted,
        )]);
    }
    let mut sessions: Vec<&AgentSession> = state.sessions.iter().collect();
    sessions.sort_by(|a, b| {
        a.rank()
            .cmp(&b.rank())
            .then(a.age_seconds.cmp(&b.age_seconds))
    });
    if !sessions.is_empty() {
        lines.push(Vec::new());
        lines.push(vec![seg("SESSIONS", Tone::Muted)]);
    }
    for session in sessions.iter().take(PANEL_SESSIONS) {
        let state_tone = match session.rank() {
            0 | 1 => Tone::Alert,
            2 => Tone::On,
            _ => Tone::Muted,
        };
        let mut first = vec![
            session_glyph(session),
            seg(short(&session.host, 12), Tone::Plain),
            seg(
                if session.blocked && session.state != "blocked" {
                    "blocked".to_owned()
                } else {
                    short(&session.state, 10)
                },
                state_tone,
            ),
        ];
        if let Some(tool) = session.tool.as_deref() {
            first.push(seg(short(tool, 24), Tone::Plain));
        } else if let Some(action) = session.last_action.as_deref() {
            first.push(seg(short(action, 24), Tone::Muted));
        }
        first.push(seg(age(session.age_seconds), Tone::Muted));
        lines.push(first);
        let mut detail = Vec::new();
        let project = basename(&session.cwd);
        if !project.is_empty() {
            detail.push(match &session.branch {
                Some(branch) => format!("{}@{}", short(project, 24), short(branch, 24)),
                None => short(project, 24),
            });
        }
        if let Some(model) = &session.model {
            detail.push(short(model, 24));
        }
        if let Some(percent) = session.context_percent {
            detail.push(format!("{percent:.0}% ctx"));
        } else if let Some(count) = session.tokens {
            detail.push(tokens(count));
        }
        if let Some(cost) = session.cost_usd {
            detail.push(format!("${cost:.2}"));
        }
        if session.subagents > 0 {
            detail.push(format!("{} sub", session.subagents));
        }
        if !detail.is_empty() {
            lines.push(vec![
                seg(" ", Tone::Muted),
                seg(detail.join(" · "), Tone::Muted),
            ]);
        }
    }
    if sessions.len() > PANEL_SESSIONS {
        lines.push(vec![seg(
            format!("+{} more sessions", sessions.len() - PANEL_SESSIONS),
            Tone::Muted,
        )]);
    }
    let working = live.iter().filter(|s| s.state == "working").count();
    let blocked = live
        .iter()
        .filter(|s| s.blocked || s.state == "blocked")
        .count();
    lines.push(Vec::new());
    lines.push(vec![
        seg(format!("{} live", live.len()), Tone::Muted),
        seg(format!("{working} working"), Tone::Muted),
        seg(
            format!("{blocked} blocked"),
            if blocked > 0 {
                Tone::Alert
            } else {
                Tone::Muted
            },
        ),
    ]);
    lines
}

/// Fit panel lines into `capacity` rows. When they do not fit, the final count
/// line stays pinned at the bottom and a `+N more` row replaces the overflow.
pub fn fit_lines(lines: &[Line], capacity: usize) -> Vec<Line> {
    if lines.len() <= capacity {
        return lines.to_vec();
    }
    if capacity < 3 {
        return lines.iter().rev().take(capacity).rev().cloned().collect();
    }
    let shown = capacity - 2;
    let mut out = lines[..shown].to_vec();
    out.push(vec![seg(
        format!(
            "… +{} more rows (panel_position = \"right\" shows more)",
            lines.len() - shown - 1
        ),
        Tone::Muted,
    )]);
    out.push(lines[lines.len() - 1].clone());
    out
}

/// Age fields are snapshot-relative; advance them by the time since the fetch.
pub fn aged(state: &CrowState, elapsed: Duration) -> CrowState {
    let mut state = state.clone();
    for session in &mut state.sessions {
        session.age_seconds = session.age_seconds.saturating_add(elapsed.as_secs());
    }
    state
}

// ---------------------------------------------------------------------------
// Git branch without a subprocess

/// Branch (or short detached SHA) of the repository containing `cwd`, reading
/// `.git/HEAD` (directory or `gitdir:` file, as in worktrees) walking upwards.
pub fn git_branch(cwd: &Path) -> Option<String> {
    fn read_small(path: &Path) -> Option<String> {
        // Regular files only: opening a FIFO named HEAD would block the worker.
        if !std::fs::metadata(path).ok()?.is_file() {
            return None;
        }
        let mut text = String::new();
        std::fs::File::open(path)
            .ok()?
            .take(4096)
            .read_to_string(&mut text)
            .ok()?;
        Some(text)
    }
    for dir in cwd.ancestors().take(64) {
        let dot_git = dir.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let pointer = read_small(&dot_git)?;
            let target = pointer.trim().strip_prefix("gitdir:")?.trim();
            let target = Path::new(target);
            if target.is_absolute() {
                target.to_path_buf()
            } else {
                dir.join(target)
            }
        } else {
            continue;
        };
        let head = read_small(&git_dir.join("HEAD"))?;
        let head = head.trim();
        if let Some(reference) = head.strip_prefix("ref:") {
            let reference = reference.trim();
            let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
            return Some(clean(name)).filter(|s| !s.is_empty());
        }
        if head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(head[..7].to_owned());
        }
        return None;
    }
    None
}

// ---------------------------------------------------------------------------
// Throttling (pure, unit-tested)

/// File state that changes whenever crow-skills writes an event, a session
/// snapshot, the mode or a home ledger run. `stat` only: nothing is read.
pub type Signature = Vec<Option<(SystemTime, u64)>>;

pub fn signature(paths: &[PathBuf]) -> Signature {
    paths
        .iter()
        .map(|path| {
            std::fs::metadata(path)
                .ok()
                .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
        })
        .collect()
}

pub fn watched_paths(home: &Path) -> Vec<PathBuf> {
    let events = home.join(".local/state/crow-skills/events");
    vec![
        events.join("events.jsonl"),
        events.join("sessions"),
        home.join(".config/crow-skills/mode.json"),
        home.join(".crow/runs"),
    ]
}

#[derive(Debug, Clone)]
pub struct Schedule {
    pub interval: Duration,
    last_fetch: Option<Instant>,
    last_signature: Option<Signature>,
    failures: u32,
}
impl Schedule {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_fetch: None,
            last_signature: None,
            failures: 0,
        }
    }
    /// Sleep between worker ticks: slower when the window is unfocused.
    pub fn tick(&self, focused: bool) -> Duration {
        if focused {
            self.interval
        } else {
            self.interval * 4
        }
    }
    /// Run the snapshot command only when the files changed, or the data is stale
    /// (ages, memory): 15 s with activity, 60 s idle; both x4 unfocused. Failures
    /// back off exponentially up to 32 intervals. Never more often than `tick`.
    pub fn due(&self, now: Instant, signature: &Signature, focused: bool, busy: bool) -> bool {
        let Some(last) = self.last_fetch else {
            return true;
        };
        let elapsed = now.saturating_duration_since(last);
        let backoff = self.tick(focused) * (1u32 << self.failures.min(5));
        // Busy agents append events continuously: each call costs ~0.13 s CPU, so
        // change-triggered refreshes are then spaced by at least BUSY_GAP.
        let gap = if busy { backoff.max(BUSY_GAP) } else { backoff };
        if elapsed < gap {
            return false;
        }
        if self.failures > 0 || self.last_signature.as_ref() != Some(signature) {
            return true;
        }
        let stale = Duration::from_secs(if busy { 15 } else { 60 }) * if focused { 1 } else { 4 };
        elapsed >= stale
    }
    pub fn fetched(&mut self, now: Instant, signature: Signature, ok: bool) {
        self.last_fetch = Some(now);
        self.last_signature = Some(signature);
        self.failures = if ok { 0 } else { self.failures + 1 };
    }
}

// ---------------------------------------------------------------------------
// Command execution

fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

/// Resolve the configured command. GUI launches (Finder, Dock) may not inherit a
/// shell PATH, so common package-manager bin directories are also searched.
pub fn resolve_command(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 || path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    if let Ok(found) = which::which(command) {
        return Some(found);
    }
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/home/linuxbrew/.linuxbrew/bin"),
    ];
    if let Some(home) = home_dir() {
        dirs.push(home.join(".local/bin"));
    }
    dirs.into_iter()
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

/// Run `<command> events snapshot --json` with a timeout and an output cap.
pub fn run_snapshot(program: &Path) -> Result<Vec<u8>, String> {
    let mut command = Command::new(program);
    command
        .args(["events", "snapshot", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // Own process group: crow-skills is a launcher (node -> python); a timeout
    // must terminate the whole tree, not only the direct child.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| format!("spawn: {e}"))?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (done, output) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("crow-snapshot-read".into())
        .spawn(move || {
            let mut out = Vec::new();
            let mut limited = stdout.take(MAX_STDOUT + 1);
            let _ = limited.read_to_end(&mut out);
            // Drain anything beyond the cap so the child never blocks on a full pipe.
            let _ = std::io::copy(&mut limited.into_inner(), &mut std::io::sink());
            let _ = done.send(out);
        })
        .map_err(|e| format!("reader: {e}"))?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    // The child is reaped only after the group is no longer needed, so its PID
    // (= process group id) cannot be reused while it may still be signalled.
    let abort = |child: &mut std::process::Child| {
        #[cfg(unix)]
        if let Some(group) = i32::try_from(child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    // 1) stdout EOF (all writers, including grandchildren, closed it) ...
    let out = match output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(out) => out,
        Err(_) => {
            // The reader thread ends once the killed group closes the pipe.
            abort(&mut child);
            return Err("crow-skills timed out".into());
        }
    };
    // 2) ... then the exit status, within the same deadline.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                abort(&mut child);
                return Err("crow-skills timed out".into());
            }
            Err(e) => {
                abort(&mut child);
                return Err(format!("wait: {e}"));
            }
        }
    };
    if !status.success() {
        return Err(format!(
            "crow-skills exited {}",
            status.code().unwrap_or(-1)
        ));
    }
    if out.len() as u64 > MAX_STDOUT {
        return Err("snapshot too large".into());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Worker

/// State shared between the UI thread and the crow worker.
pub struct Shared {
    pub running: Arc<AtomicBool>,
    pub focused: AtomicBool,
    pub panel: AtomicBool,
    pub pane: AtomicU64,
    wake: Mutex<Option<mpsc::SyncSender<()>>>,
}
impl Shared {
    pub fn wake(&self) {
        if let Ok(wake) = self.wake.lock()
            && let Some(sender) = wake.as_ref()
        {
            // One pending wake is enough; never queue more.
            let _ = sender.try_send(());
        }
    }
}

/// Start the worker. `publish` returns false once the UI is gone. `cwd_of` asks
/// the daemon for a pane's working directory; `Err` means the request itself
/// failed (e.g. an older daemon that does not know it), and after a few in a row
/// the worker stops asking for the rest of the session instead of flooding it.
pub fn spawn(
    config: &CrowConfig,
    running: Arc<AtomicBool>,
    pane: u64,
    cwd_of: impl Fn(u64) -> Result<Option<String>, ()> + Send + 'static,
    publish: impl Fn(CrowView) -> bool + Send + 'static,
) -> std::io::Result<(Arc<Shared>, thread::JoinHandle<()>)> {
    let (sender, wakes) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared {
        running,
        focused: AtomicBool::new(true),
        panel: AtomicBool::new(config.panel),
        pane: AtomicU64::new(pane),
        wake: Mutex::new(Some(sender)),
    });
    let worker_shared = shared.clone();
    let config = config.clone();
    let handle = thread::Builder::new()
        .name("crow-status".into())
        .spawn(move || {
            let shared = worker_shared;
            let watched = home_dir().map(|h| watched_paths(&h)).unwrap_or_default();
            let mut schedule = Schedule::new(Duration::from_millis(config.interval_ms));
            let mut program: Option<PathBuf> = None;
            let mut resolved_at: Option<Instant> = None;
            let mut source = Source::Pending;
            let mut fetched_at = Instant::now();
            let mut last_sent: Option<CrowView> = None;
            let mut cwd_cache: Option<(u64, Option<String>, Option<String>)> = None;
            let mut first = true;
            let mut cwd_failures = 0u32;
            let mut cwd_failed_at: Option<Instant> = None;
            while shared.running.load(Ordering::Relaxed) {
                if !first {
                    let focused = shared.focused.load(Ordering::Relaxed);
                    match wakes.recv_timeout(schedule.tick(focused)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    while wakes.try_recv().is_ok() {}
                    if !shared.running.load(Ordering::Relaxed) {
                        break;
                    }
                }
                first = false;
                let now = Instant::now();
                let focused = shared.focused.load(Ordering::Relaxed);
                // Pane cwd + branch: one IPC + one targeted process query per tick.
                let pane = shared.pane.load(Ordering::Relaxed);
                // Another pane (focus change, reattach): earlier failures were
                // about the old one.
                if cwd_cache.as_ref().is_some_and(|(p, _, _)| *p != pane) {
                    cwd_failures = 0;
                    cwd_failed_at = None;
                }
                let (cwd, branch) = if config.status_bar {
                    // After 3 failures in a row (closed pane, daemon restart, or an older
                    // daemon that rejects the request and logs it) retry once a minute.
                    let retry = cwd_failed_at.is_none_or(|t| now.duration_since(t) >= CWD_RETRY);
                    let cwd = if cwd_failures < 3 || retry {
                        match cwd_of(pane) {
                            Ok(cwd) => {
                                cwd_failures = 0;
                                cwd_failed_at = None;
                                cwd
                            }
                            Err(()) => {
                                cwd_failures += 1;
                                if cwd_failures >= 3 {
                                    cwd_failed_at = Some(now);
                                }
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let branch = match &cwd_cache {
                        Some((p, c, b)) if *p == pane && *c == cwd && !focused => b.clone(),
                        _ => cwd.as_deref().and_then(|c| git_branch(Path::new(c))),
                    };
                    cwd_cache = Some((pane, cwd.clone(), branch.clone()));
                    (cwd, branch)
                } else {
                    (None, None)
                };
                // (Re)resolve the command at start and every minute while missing.
                if program.is_none()
                    && resolved_at.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(60))
                {
                    program = resolve_command(&config.command);
                    resolved_at = Some(now);
                    if program.is_none() {
                        source = Source::Missing;
                    }
                }
                if let Some(path) = &program {
                    let sig = signature(&watched);
                    let busy = matches!(&source, Source::Ready(s)
                        if s.sessions.iter().any(|x| x.state == "working" || x.blocked)
                            || s.runs.iter().any(|r| r.status == "running"));
                    if schedule.due(now, &sig, focused, busy) {
                        let result = run_snapshot(path)
                            .and_then(|bytes| parse_snapshot(&bytes).map_err(|e| e.to_string()));
                        let ok = result.is_ok();
                        match result {
                            Ok(state) => source = Source::Ready(state),
                            Err(error) => {
                                if !path.is_file() {
                                    program = None;
                                    resolved_at = None;
                                    source = Source::Missing;
                                } else {
                                    source = Source::Failed(error);
                                }
                            }
                        }
                        fetched_at = Instant::now();
                        schedule.fetched(fetched_at, sig, ok);
                    }
                }
                let current = match &source {
                    Source::Ready(state) => Source::Ready(aged(state, fetched_at.elapsed())),
                    other => other.clone(),
                };
                let view = CrowView {
                    status: if config.status_bar {
                        status_segments(&current, branch.as_deref(), cwd.as_deref())
                    } else {
                        Vec::new()
                    },
                    panel: if shared.panel.load(Ordering::Relaxed) {
                        panel_lines(&current)
                    } else {
                        Vec::new()
                    },
                };
                if last_sent.as_ref() != Some(&view) {
                    if !publish(view.clone()) {
                        break;
                    }
                    last_sent = Some(view);
                }
            }
        })?;
    Ok((shared, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: &str = r#"{"schema_version":1,"ok":true,"command":"events","data":{
      "schema":"crow-snapshot/1","generatedAt":"2026-10-28T11:26:58+00:00",
      "mode":{"mode":"on","updatedAt":null,"source":"default"},"indicator":"▸▸ crow on",
      "sessions":[
        {"schema":"crow-session/1","host":"claude","state":"working","blocked":false,
         "cwd":"/w/crow-term","branch":"main","model":"claude-opus-5-5","currentTool":"Bash",
         "lastAction":"tool_start:Bash","subagents":2,"costUsd":4.5,
         "usage":{"used_percentage":57,"total_input_tokens":572499,"total_output_tokens":308},
         "ageSeconds":3,"ledgerRun":"r1"},
        {"schema":"crow-session/1","host":"codex","state":"idle","blocked":true,"cwd":"/w/api",
         "branch":null,"currentTool":null,"lastAction":"permission_request","ageSeconds":75},
        {"schema":"crow-session/1","host":"agy","state":"ended","cwd":"/tmp","ageSeconds":120},
        {"schema":"crow-session/1","host":"claude","state":"error","cwd":"/w","ageSeconds":7200}
      ],
      "runs":[
        {"id":"r1","title":"Crow panel","host":"claude","cwd":"/w/crow-term","status":"running",
         "done":3,"failed":0,"total":7,"running":2,"progress":0.43,
         "agents":[{"owner":"general-purpose","unit":"u3","activity":"Draft release"},
                   {"owner":"reviewer","unit":"u4","activity":"Break it"}]},
        {"id":"r0","title":"Old","cwd":"/w","status":"done","done":8,"total":8,"agents":[]}
      ],
      "autoupdate":{"enabled":true}}}"#;

    fn state() -> CrowState {
        parse_snapshot(CURRENT.as_bytes()).unwrap()
    }
    fn plain(segments: &[Segment]) -> String {
        segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn parses_current_schema() {
        let s = state();
        assert_eq!(s.mode_on, Some(true));
        assert_eq!(s.sessions.len(), 4);
        let claude = &s.sessions[0];
        assert_eq!(claude.tool.as_deref(), Some("Bash"));
        assert_eq!(claude.context_percent, Some(57.0));
        assert_eq!(claude.tokens, Some(572_807));
        assert_eq!(claude.cost_usd, Some(4.5));
        assert_eq!(claude.subagents, 2);
        assert!(s.sessions[1].blocked);
        assert_eq!(s.runs[0].agents.len(), 2);
        assert_eq!(s.runs[0].agents[0].owner, "general-purpose");
        assert_eq!(s.memory, None);
    }

    #[test]
    fn tolerates_future_fields_minor_versions_and_resources() {
        let future = r#"{"ok":true,"data":{"schema":"crow-snapshot/1.3","mode":{"mode":"off"},
          "brandNew":{"x":[1,2]},
          "sessions":[{"schema":"crow-session/1.1","host":"gemini","state":"working","futureField":true,
                       "ageSeconds":1.9,"costUsd":"not-a-number","usage":"weird","subagents":-3},
                      {"schema":"crow-session/2","host":"next","state":"working"},
                      "not-an-object", 42],
          "runs":[{"title":"T","status":"running","done":"x","total":5,"agents":[{"owner":"o"},7]}],
          "resources":{"schema":"crow-resources/1","cached":true,
             "memory":{"totalMb":18432,"usedPercent":82.4},
             "pressure":{"level":"warn","reasons":["swap"]},"alert":true,"segment":"mem 82%"}}}"#;
        let s = parse_snapshot(future.as_bytes()).unwrap();
        assert_eq!(s.mode_on, Some(false));
        assert_eq!(
            s.sessions.len(),
            1,
            "crow-session/2 and non-objects skipped"
        );
        assert_eq!(s.sessions[0].age_seconds, 1);
        assert_eq!(s.sessions[0].cost_usd, None);
        assert_eq!(s.sessions[0].subagents, 0);
        assert_eq!(s.runs[0].done, 0);
        assert_eq!(s.runs[0].agents.len(), 1);
        assert_eq!(
            s.memory,
            Some(Memory {
                level: "warn".into(),
                used_percent: Some(82.4)
            })
        );
        let status = plain(&status_segments(&Source::Ready(s), None, None));
        assert!(status.starts_with("▸▸ crow off | gemini 1"), "{status}");
        assert!(status.ends_with("mem warn 82%"), "{status}");
        // Unknown resources major and null resources are ignored, not errors.
        for resources in [
            r#"{"schema":"crow-resources/2","memory":{"usedPercent":1}}"#,
            "null",
        ] {
            let doc = format!(r#"{{"schema":"crow-snapshot/1","resources":{resources}}}"#);
            assert_eq!(parse_snapshot(doc.as_bytes()).unwrap().memory, None);
        }
    }

    #[test]
    fn rejects_garbage_and_unknown_major() {
        for bad in [
            &b""[..],
            b"not json",
            b"[]",
            b"{\"ok\":true,\"data\":[1]}",
            b"{\"ok\":true}",
            &[0xff, 0xfe, 0x00],
        ] {
            assert_eq!(parse_snapshot(bad), Err(ParseError::Json), "{bad:?}");
        }
        assert_eq!(
            parse_snapshot(br#"{"ok":false,"error":"x"}"#),
            Err(ParseError::NotOk)
        );
        assert!(matches!(
            parse_snapshot(br#"{"ok":true,"data":{"schema":"crow-snapshot/2"}}"#),
            Err(ParseError::Unsupported(s)) if s == "crow-snapshot/2"
        ));
        assert!(matches!(
            parse_snapshot(br#"{"schema":"crow-snapshot/10"}"#),
            Err(ParseError::Unsupported(_))
        ));
        assert!(matches!(
            parse_snapshot(br#"{"sessions":[]}"#),
            Err(ParseError::Unsupported(_))
        ));
    }

    #[test]
    fn bounds_and_sanitizes_untrusted_strings() {
        let many = (0..500)
            .map(|i| format!(r#"{{"host":"h{i}","state":"idle"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let long = "x".repeat(10_000);
        let doc = format!(
            r#"{{"schema":"crow-snapshot/1","sessions":[{many}],
               "runs":[{{"title":"{long}\u001b[31m","status":"running"}}]}}"#
        );
        let s = parse_snapshot(doc.as_bytes()).unwrap();
        assert_eq!(s.sessions.len(), MAX_SESSIONS);
        let title = &s.runs[0].title;
        assert!(title.chars().count() <= MAX_TEXT, "{}", title.len());
        assert!(!title.chars().any(char::is_control));
        let panel = panel_lines(&Source::Ready(s));
        assert!(panel.len() < 2 * PANEL_SESSIONS + 16, "{}", panel.len());
        assert!(
            panel
                .iter()
                .flatten()
                .any(|seg| seg.text == format!("+{} more sessions", MAX_SESSIONS - PANEL_SESSIONS))
        );
    }

    #[test]
    fn status_bar_format() {
        let s = Source::Ready(state());
        let status = status_segments(&s, Some("feature/x"), Some("/w/crow-term/src"));
        assert_eq!(
            plain(&status),
            "▸▸ crow on | claude 2  codex 1 | ⚠ 1 blocked | ✗ 1 error | Crow panel 3/7 | ⎇ feature/x"
        );
        assert_eq!(status[0].tone, Tone::On);
        assert_eq!(status[2].tone, Tone::Alert);
        assert_eq!(status[4].tone, Tone::Accent);
        // Absent CLI: crow segments hidden, branch still shown.
        assert_eq!(
            plain(&status_segments(&Source::Missing, Some("main"), None)),
            "⎇ main"
        );
        assert!(status_segments(&Source::Missing, None, None).is_empty());
        assert_eq!(
            plain(&status_segments(
                &Source::Failed("crow-skills timed out".into()),
                None,
                None
            )),
            "▸▸ crow ? | crow-skills timed out"
        );
    }

    #[test]
    fn agent_list_rows_follow_claude_format() {
        let lines = panel_lines(&Source::Ready(state()));
        let rows: Vec<String> = lines
            .iter()
            .map(|line| {
                line.iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join("  ")
            })
            .collect();
        let expected = [
            "▸▸ crow on  3 live",
            "",
            "●  main  Crow panel  3/7",
            "○  general-purpose  Draft release",
            "○  reviewer  Break it",
            "✓ 3 done  ✗ 0 failed  2 pending",
            "",
            "SESSIONS",
            "⚠  codex  blocked  permission_request  1m",
            "   api",
            "✗  claude  error  2h",
            "   w",
            "●  claude  working  Bash  3s",
            "   crow-term@main · claude-opus-5-5 · 57% ctx · $4.50 · 2 sub",
            "·  agy  ended  2m",
            "   tmp",
            "",
            "3 live  1 working  1 blocked",
        ];
        assert_eq!(rows, expected);
        assert_eq!(
            panel_lines(&Source::Missing)[0][0].text,
            "crow-skills not found"
        );
    }

    #[test]
    fn overflowing_panel_keeps_the_count_line() {
        let lines = panel_lines(&Source::Ready(state()));
        assert_eq!(fit_lines(&lines, 100), lines);
        let fitted = fit_lines(&lines, 11);
        assert_eq!(fitted.len(), 11);
        assert_eq!(fitted.last(), lines.last());
        assert_eq!(
            fitted[9][0].text,
            format!(
                "… +{} more rows (panel_position = \"right\" shows more)",
                lines.len() - 10
            )
        );
        assert_eq!(fit_lines(&lines, 1), vec![lines.last().unwrap().clone()]);
        assert!(fit_lines(&lines, 0).is_empty());
    }

    #[test]
    fn ages_and_aging() {
        assert_eq!(
            [0, 59, 60, 3599, 3600, 86_399, 86_400].map(age),
            ["0s", "59s", "1m", "59m", "1h", "23h", "1d"]
        );
        let older = aged(&state(), Duration::from_secs(10));
        assert_eq!(older.sessions[0].age_seconds, 13);
    }

    #[test]
    fn active_run_prefers_focused_directory() {
        let mut s = state();
        s.runs.push(Run {
            title: "Other".into(),
            cwd: "/elsewhere".into(),
            status: "running".into(),
            ..Run::default()
        });
        assert_eq!(
            active_run(&s, Some("/elsewhere/x")).unwrap().0.title,
            "Other"
        );
        assert_eq!(
            active_run(&s, Some("/w/crow-term")).unwrap().0.title,
            "Crow panel"
        );
        // A sibling with a common prefix is not an ancestor.
        assert_eq!(
            active_run(&s, Some("/elsewhere2")).unwrap().0.title,
            "Crow panel"
        );
        assert_eq!(active_run(&s, None).unwrap().1, 2);
    }

    #[test]
    fn schedule_only_fetches_on_change_staleness_or_after_backoff() {
        let t0 = Instant::now();
        let s = |n: u64| -> Signature { vec![Some((SystemTime::UNIX_EPOCH, n))] };
        let mut schedule = Schedule::new(Duration::from_secs(2));
        assert!(schedule.due(t0, &s(1), true, false), "first fetch");
        schedule.fetched(t0, s(1), true);
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        assert!(!schedule.due(at(1), &s(2), true, false), "min interval");
        assert!(schedule.due(at(2), &s(2), true, false), "changed files");
        assert!(!schedule.due(at(2), &s(1), true, false), "unchanged");
        assert!(!schedule.due(at(4), &s(9), true, true), "busy gap");
        assert!(schedule.due(at(5), &s(9), true, true), "busy gap elapsed");
        assert!(!schedule.due(at(14), &s(1), true, true));
        assert!(schedule.due(at(15), &s(1), true, true), "stale while busy");
        assert!(!schedule.due(at(59), &s(1), true, false));
        assert!(schedule.due(at(60), &s(1), true, false), "stale idle");
        // Unfocused: 4x slower for both change and staleness.
        assert!(!schedule.due(at(7), &s(2), false, false));
        assert!(schedule.due(at(8), &s(2), false, false));
        assert!(!schedule.due(at(239), &s(1), false, false));
        assert!(schedule.due(at(240), &s(1), false, false));
        // Failures back off exponentially (2, 4, 8 … capped at 64 s).
        let mut failing = Schedule::new(Duration::from_secs(2));
        let mut t = t0;
        let mut gaps = Vec::new();
        for _ in 0..8 {
            failing.fetched(t, s(1), false);
            let mut next = t;
            while !failing.due(next, &s(1), true, false) {
                next += Duration::from_secs(1);
            }
            gaps.push(next.duration_since(t).as_secs());
            t = next;
        }
        assert_eq!(gaps, [4, 8, 16, 32, 64, 64, 64, 64]);
        failing.fetched(t, s(1), true);
        assert!(failing.due(t + Duration::from_secs(2), &s(2), true, false));
    }

    #[test]
    fn git_branch_reads_head_dirs_worktrees_and_detached() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(
            repo.join(".git/HEAD"),
            "ref: refs/heads/feature/crow-panel\n",
        )
        .unwrap();
        assert_eq!(
            git_branch(&repo.join("src/deep")).as_deref(),
            Some("feature/crow-panel")
        );
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        std::fs::write(
            repo.join(".git/worktrees/wt/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
        assert_eq!(git_branch(&worktree).as_deref(), Some("0123456"));
        assert_eq!(git_branch(&root.path().join("nowhere")), None);
        std::fs::write(repo.join(".git/HEAD"), "garbage").unwrap();
        assert_eq!(git_branch(&repo), None);
    }

    #[test]
    fn missing_command_and_failing_command_are_reported() {
        assert_eq!(resolve_command("/definitely/not/here/crow-skills"), None);
        assert_eq!(resolve_command("crow-skills-definitely-absent-xyz"), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("fake");
            std::fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(run_snapshot(&script), Err("crow-skills exited 3".into()));
            let ok = dir.path().join("ok");
            std::fs::write(
                &ok,
                "#!/bin/sh\n[ \"$*\" = \"events snapshot --json\" ] || exit 9\nprintf '{\"schema\":\"crow-snapshot/1\"}'\n",
            )
            .unwrap();
            std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();
            let bytes = run_snapshot(&ok).unwrap();
            assert!(parse_snapshot(&bytes).is_ok());
            let huge = dir.path().join("huge");
            std::fs::write(&huge, "#!/bin/sh\nhead -c 3000000 /dev/zero\n").unwrap();
            std::fs::set_permissions(&huge, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(run_snapshot(&huge), Err("snapshot too large".into()));
            // A launcher whose grandchild keeps stdout open: the deadline still holds
            // and the whole process group is terminated.
            let marker = dir.path().join("grandchild.pid");
            let stuck = dir.path().join("stuck");
            std::fs::write(
                &stuck,
                format!(
                    "#!/bin/sh\nsh -c 'echo $$ > {}; exec sleep 30' &\nwait\n",
                    marker.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
            let started = Instant::now();
            assert_eq!(run_snapshot(&stuck), Err("crow-skills timed out".into()));
            assert!(started.elapsed() < COMMAND_TIMEOUT + Duration::from_secs(2));
            let pid: i32 = std::fs::read_to_string(&marker)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let pid = rustix::process::Pid::from_raw(pid).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while rustix::process::test_kill_process(pid).is_ok() {
                assert!(Instant::now() < deadline, "grandchild survived the timeout");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn git_head_fifo_does_not_block() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        let fifo = root.path().join(".git/HEAD");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let path = root.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(git_branch(&path)).unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(None));
    }

    #[test]
    fn worker_stops_asking_for_cwd_after_repeated_protocol_failures() {
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        let (views, published) = mpsc::channel();
        let config = CrowConfig {
            command: "/definitely/not/here/crow-skills".into(),
            interval_ms: 1000,
            ..CrowConfig::default()
        };
        let running = Arc::new(AtomicBool::new(true));
        let (shared, handle) = spawn(
            &config,
            running.clone(),
            1,
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(())
            },
            move |view| views.send(view).is_ok(),
        )
        .unwrap();
        for _ in 0..8 {
            shared.wake();
            thread::sleep(Duration::from_millis(30));
        }
        running.store(false, Ordering::Relaxed);
        shared.wake();
        handle.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3, "then once per CWD_RETRY");
        // Missing crow-skills: nothing but an empty status line is published.
        assert!(published.try_iter().all(|view| view.status.is_empty()));
    }
}
