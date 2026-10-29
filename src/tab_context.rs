//! What a tab and the status bar say about a pane: which program runs in it,
//! which agent (and task) it belongs to, which repository and branch. Pure
//! and unit-tested; the UI only measures and paints the result.
//!
//! * Tab label, first that applies: the user's name for the window (`rename`),
//!   the program's own title (OSC 0/2), an agent CLI and its task, the
//!   foreground program, `repo@branch`, the shell.
//! * Footer, for the focused pane, in priority order (lower-priority segments
//!   are dropped first when space runs out): agent and state, task or tool,
//!   run progress, model and context use, `repo@branch` with dirty/ahead/
//!   behind marks; without an agent, the foreground program and directory.
//!   One compact global segment sits on the right: crow on/off, active agent
//!   sessions, memory only under pressure.
use crate::model::PaneSummary;

/// Shells: when one is in the foreground the pane is "idle at a prompt".
pub const SHELLS: [&str; 14] = [
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "mksh",
    "tcsh",
    "csh",
    "nu",
    "xonsh",
    "elvish",
    "pwsh",
    "powershell",
];
/// Agent CLIs matched to crow sessions.
pub const AGENTS: [&str; 5] = ["claude", "codex", "gemini", "agy", "devin"];
/// Programs whose first plain argument may be shown (a file, a subcommand).
const SHOW_ARGUMENT: [&str; 20] = [
    "nvim", "vim", "vi", "nano", "hx", "helix", "emacs", "less", "more", "man", "cargo", "npm",
    "pnpm", "yarn", "make", "git", "go", "python", "python3", "node",
];

/// Executable name without directory, `.exe` or a login shell's `-`.
pub fn program_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let base = base.trim_start_matches('-');
    let lower = base.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .map(str::to_owned)
        .unwrap_or(lower)
}

pub fn is_shell(name: &str) -> bool {
    SHELLS.contains(&program_name(name).as_str())
}

/// The arguments worth showing for `program` (daemon side, before anything
/// leaves the process): only for allowlisted programs, only the first plain
/// argument, and only if it cannot be a secret (no `=`, `@`, `:`, no flags,
/// short, plain characters). Paths are reduced to their file name.
pub fn safe_args(program: &str, args: &[String]) -> Vec<String> {
    if !SHOW_ARGUMENT.contains(&program_name(program).as_str()) {
        return Vec::new();
    }
    let plain = |arg: &&String| {
        !arg.starts_with('-')
            && !arg.is_empty()
            && arg.chars().count() <= 64
            && arg
                .chars()
                .all(|c| c.is_alphanumeric() || "._/-+".contains(c))
    };
    args.iter()
        .find(plain)
        .map(|arg| {
            let name = arg.rsplit('/').next().unwrap_or(arg);
            if name.is_empty() {
                arg.clone()
            } else {
                name.to_owned()
            }
        })
        .into_iter()
        .collect()
}

/// `…` in the middle, keeping the distinctive end (`crow-ter…/feature/x`).
pub fn middle_ellipsis(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    if max <= 1 {
        return "…".chars().take(max).collect();
    }
    let tail = (max - 1) / 2 + (max - 1) % 2;
    let head = max - 1 - tail;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// Shorten at a word boundary when one leaves enough text; else cut with `…`.
pub fn word_ellipsis(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    if max <= 1 {
        return "…".chars().take(max).collect();
    }
    let cut: String = text.chars().take(max - 1).collect();
    match cut.rfind(' ') {
        Some(at) if cut[..at].chars().count() >= max / 2 => format!("{}…", cut[..at].trim_end()),
        _ => format!("{cut}…"),
    }
}

/// An agent CLI session as published by crow-skills (only what matching and
/// display need).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Agent {
    pub host: String,
    pub state: String,
    pub blocked: bool,
    pub cwd: String,
    pub pid: Option<u32>,
    pub tty: Option<String>,
    pub model: Option<String>,
    pub tool: Option<String>,
    pub task: Option<String>,
    pub context_percent: Option<u8>,
    pub age_seconds: u64,
}

/// Progress of the crow-behaviour run for a directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Progress {
    pub title: String,
    pub done: u64,
    pub total: u64,
    pub failed: u64,
}

/// `git status --porcelain=v2 --branch` digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Repo {
    pub name: String,
    pub branch: String,
    pub dirty: bool,
    pub ahead: u64,
    pub behind: u64,
}

impl Repo {
    /// `crow-term@main *↑2↓1`.
    pub fn label(&self) -> String {
        let mut out = format!("{}@{}", self.name, self.branch);
        let marks: String = [
            self.dirty.then(|| "*".to_owned()),
            (self.ahead > 0).then(|| format!("↑{}", self.ahead)),
            (self.behind > 0).then(|| format!("↓{}", self.behind)),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !marks.is_empty() {
            out.push(' ');
            out.push_str(&marks);
        }
        out
    }
}

/// Parse `git status --porcelain=v2 --branch` (repository name from `top`).
pub fn parse_git_status(output: &str, top: &str) -> Option<Repo> {
    let mut repo = Repo {
        name: top
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_owned(),
        ..Repo::default()
    };
    let mut head = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            head = Some(rest.trim().to_owned());
        } else if let Some(rest) = line.strip_prefix("# branch.oid ") {
            if head.as_deref() == Some("(detached)") || head.is_none() {
                repo.branch = rest.chars().take(7).collect();
            }
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for part in rest.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    repo.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    repo.behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            repo.dirty = true;
        }
    }
    match head.as_deref() {
        Some("(detached)") | None => {}
        Some(branch) => repo.branch = branch.to_owned(),
    }
    (!repo.branch.is_empty() && !repo.name.is_empty()).then_some(repo)
}

/// The agent session running in `pane`: by process id, then by terminal,
/// then (when the foreground is an agent CLI) by directory, most recently
/// active first.
pub fn agent_for<'a>(pane: &PaneSummary, agents: &'a [Agent]) -> Option<&'a Agent> {
    if let Some(agent) = agents.iter().find(|a| a.pid.is_some() && a.pid == pane.pid) {
        return Some(agent);
    }
    if !pane.tty.is_empty()
        && let Some(agent) = agents
            .iter()
            .find(|a| a.tty.as_deref() == Some(pane.tty.as_str()))
    {
        return Some(agent);
    }
    let process = program_name(&pane.process);
    if !AGENTS.contains(&process.as_str()) {
        return None;
    }
    let cwd = pane.cwd.as_deref()?;
    agents
        .iter()
        .filter(|a| a.host == process && same_dir(&a.cwd, cwd))
        .min_by_key(|a| a.age_seconds)
}

fn same_dir(a: &str, b: &str) -> bool {
    a.trim_end_matches(['/', '\\']) == b.trim_end_matches(['/', '\\'])
}

/// Shortest readable form of a directory (`~/w/crow-term`).
pub fn short_dir(cwd: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() && (cwd == home || cwd.starts_with(&format!("{home}/"))) => {
            format!("~{}", &cwd[home.len()..])
        }
        _ => cwd.to_owned(),
    }
}

/// Everything a tab label can be made from.
pub struct LabelInput<'a> {
    /// The window's name when the user renamed it (`None` for the default).
    pub rename: Option<&'a str>,
    pub pane: Option<&'a PaneSummary>,
    pub agent: Option<&'a Agent>,
    pub repo: Option<&'a Repo>,
    pub shell: &'a str,
}

/// The tab label (full; the UI middle-ellipsizes it to the tab width).
pub fn tab_label(input: &LabelInput<'_>) -> String {
    if let Some(name) = input.rename.filter(|n| !n.trim().is_empty()) {
        return name.to_owned();
    }
    if let Some(pane) = input.pane {
        let title = pane.title.trim();
        if !title.is_empty() {
            return title.to_owned();
        }
        if let Some(agent) = input.agent {
            return match agent.task.as_deref().or(agent.tool.as_deref()) {
                Some(task) => format!("{} · {task}", agent.host),
                None => agent.host.clone(),
            };
        }
        if !pane.process.is_empty() && !is_shell(&pane.process) {
            let mut label = program_name(&pane.process);
            for arg in &pane.args {
                label.push(' ');
                label.push_str(arg);
            }
            return label;
        }
    }
    if let Some(repo) = input.repo {
        return format!("{}@{}", repo.name, repo.branch);
    }
    match input
        .pane
        .map(|p| program_name(&p.process))
        .filter(|p| !p.is_empty())
    {
        Some(shell) => shell,
        None => program_name(input.shell),
    }
}

/// How a segment is painted (the UI maps it to theme colours).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    /// Theme foreground: readable text.
    Plain,
    /// Agent working, crow on.
    On,
    Accent,
    Warn,
    Alert,
    /// Only for separators and secondary marks.
    Muted,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Segment {
    /// Shown text (may be shortened by `fit`).
    pub text: String,
    /// Full text (shown on click when shortened).
    pub full: String,
    pub tone: Tone,
    /// Lower values are kept longer.
    pub priority: u8,
    /// Shortening style when this is the last segment that does not fit.
    pub shorten: Shorten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shorten {
    Middle,
    Words,
}

fn segment(text: String, tone: Tone, priority: u8, shorten: Shorten) -> Segment {
    Segment {
        full: text.clone(),
        text,
        tone,
        priority,
        shorten,
    }
}

/// Inputs of the footer for the focused pane.
pub struct FooterInput<'a> {
    pub pane: Option<&'a PaneSummary>,
    pub agent: Option<&'a Agent>,
    pub progress: Option<&'a Progress>,
    pub repo: Option<&'a Repo>,
    pub home: Option<&'a str>,
    /// The session name, shown only when more than one session exists.
    pub session: Option<&'a str>,
}

/// `▰▰▰▱▱▱▱▱` for done/total.
fn bar(done: u64, total: u64) -> String {
    let cells = 8u64;
    let filled = (done.min(total) * cells + total / 2)
        .checked_div(total)
        .unwrap_or(0);
    (0..cells)
        .map(|i| if i < filled { '▰' } else { '▱' })
        .collect()
}

/// The left part of the footer, highest priority first.
pub fn footer_segments(input: &FooterInput<'_>) -> Vec<Segment> {
    let mut out = Vec::new();
    if let Some(agent) = input.agent {
        let (state, tone) = match (agent.blocked, agent.state.as_str()) {
            (true, _) | (_, "blocked") => ("⚠ blocked", Tone::Alert),
            (_, "error") => ("✗ error", Tone::Alert),
            (_, "working") => ("● working", Tone::On),
            (_, "ended") => ("· ended", Tone::Plain),
            (_, state) => (state, Tone::Plain),
        };
        out.push(segment(
            format!("{} {state}", agent.host),
            tone,
            0,
            Shorten::Middle,
        ));
        if let Some(task) = agent.task.as_deref().or(agent.tool.as_deref()) {
            out.push(segment(task.to_owned(), Tone::Plain, 1, Shorten::Words));
        }
    } else if let Some(pane) = input.pane {
        let mut program = program_name(&pane.process);
        for arg in &pane.args {
            program.push(' ');
            program.push_str(arg);
        }
        if !program.is_empty() {
            out.push(segment(program, Tone::Plain, 0, Shorten::Middle));
        }
    }
    if let Some(progress) = input.progress.filter(|p| p.total > 0) {
        let percent = progress.done.min(progress.total) * 100 / progress.total;
        out.push(segment(
            format!(
                "{}/{} · {percent}% {}",
                progress.done,
                progress.total,
                bar(progress.done, progress.total)
            ),
            if progress.failed > 0 {
                Tone::Warn
            } else {
                Tone::Accent
            },
            2,
            Shorten::Middle,
        ));
    }
    if let Some(agent) = input.agent {
        let model = agent.model.as_deref().map(short_model);
        let context = agent.context_percent.map(|p| format!("ctx {p}%"));
        let text = [model, context]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        if !text.is_empty() {
            let tone = match agent.context_percent {
                Some(p) if p >= 90 => Tone::Alert,
                Some(p) if p >= 75 => Tone::Warn,
                _ => Tone::Plain,
            };
            out.push(segment(text, tone, 3, Shorten::Middle));
        }
    }
    if let Some(repo) = input.repo {
        out.push(segment(repo.label(), Tone::Plain, 4, Shorten::Middle));
    } else if input.agent.is_none()
        && let Some(cwd) = input.pane.and_then(|p| p.cwd.as_deref())
    {
        out.push(segment(
            short_dir(cwd, input.home),
            Tone::Plain,
            4,
            Shorten::Middle,
        ));
    }
    if let Some(session) = input.session {
        out.push(segment(
            format!("session {session}"),
            Tone::Plain,
            5,
            Shorten::Middle,
        ));
    }
    out
}

/// `claude-opus-5-5` → `opus-5.5`; other names unchanged.
pub fn short_model(model: &str) -> String {
    let name = model.strip_prefix("claude-").unwrap_or(model);
    let mut parts: Vec<&str> = name.split('-').collect();
    if parts.len() >= 3
        && parts[parts.len() - 2].chars().all(|c| c.is_ascii_digit())
        && parts[parts.len() - 1].chars().all(|c| c.is_ascii_digit())
    {
        let minor = parts.pop().unwrap_or_default();
        let major = parts.pop().unwrap_or_default();
        return format!("{}-{major}.{minor}", parts.join("-"));
    }
    name.to_owned()
}

/// Global crow summary for the right side of the footer.
pub struct GlobalInput {
    pub crow_on: Option<bool>,
    pub active_sessions: usize,
    /// (level, used %) when memory is under pressure ("warn"/"critical").
    pub memory: Option<(String, Option<u8>)>,
}

pub fn global_segment(input: &GlobalInput) -> Option<Segment> {
    let mut parts = Vec::new();
    let mut tone = Tone::Plain;
    match input.crow_on {
        Some(true) => {
            parts.push("crow on".to_owned());
            tone = Tone::On;
        }
        Some(false) => parts.push("crow off".to_owned()),
        None => {}
    }
    if input.active_sessions > 0 {
        parts.push(format!("{} active", input.active_sessions));
    }
    if let Some((level, percent)) = &input.memory
        && matches!(level.as_str(), "warn" | "critical")
    {
        parts.push(match percent {
            Some(p) => format!("mem {p}%"),
            None => format!("mem {level}"),
        });
        tone = if level == "critical" {
            Tone::Alert
        } else {
            Tone::Warn
        };
    }
    (!parts.is_empty()).then(|| segment(parts.join(" · "), tone, 0, Shorten::Middle))
}

/// Fit `segments` into `width` (in the units `measure` returns), with `gap`
/// between them: whole low-priority segments go first; when only one is
/// left and it still does not fit, it is shortened (middle ellipsis for
/// names, at a word for task text). Order is kept.
pub fn fit(
    mut segments: Vec<Segment>,
    width: f32,
    gap: f32,
    measure: impl Fn(&str) -> f32,
) -> Vec<Segment> {
    let total = |segments: &[Segment]| {
        segments.iter().map(|s| measure(&s.text)).sum::<f32>()
            + gap * segments.len().saturating_sub(1) as f32
    };
    while segments.len() > 1 && total(&segments) > width {
        let worst = segments
            .iter()
            .enumerate()
            .max_by_key(|(index, s)| (s.priority, *index))
            .map(|(index, _)| index)
            .unwrap_or(0);
        segments.remove(worst);
    }
    if let Some(only) = segments.first_mut()
        && total(std::slice::from_ref(only)) > width
    {
        let full = only.full.clone();
        let mut max = full.chars().count();
        while max > 1 {
            max -= 1;
            let text = match only.shorten {
                Shorten::Middle => middle_ellipsis(&full, max),
                Shorten::Words => word_ellipsis(&full, max),
            };
            if measure(&text) <= width {
                only.text = text;
                break;
            }
        }
        if measure(&only.text) > width {
            segments.clear();
        }
    }
    segments
}

/// WCAG relative luminance of `0xRRGGBB`.
pub fn luminance(rgb: u32) -> f64 {
    let channel = |shift: u32| {
        let c = f64::from((rgb >> shift) & 0xff) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
}

/// WCAG contrast ratio between two colours.
pub fn contrast(a: u32, b: u32) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Shows a label only after it held for `DEBOUNCE`, so a tab does not
/// flicker through every short-lived command.
#[derive(Debug, Default)]
pub struct Debounce {
    shown: std::collections::HashMap<u64, String>,
    pending: std::collections::HashMap<u64, (String, std::time::Instant)>,
}

impl Debounce {
    pub const DELAY: std::time::Duration = std::time::Duration::from_millis(600);

    /// The label to show for `key` now, given the latest computed one.
    pub fn label(&mut self, key: u64, latest: String, now: std::time::Instant) -> String {
        let shown = self.shown.entry(key).or_insert_with(|| latest.clone());
        if *shown == latest {
            self.pending.remove(&key);
            return latest;
        }
        let pending = self
            .pending
            .entry(key)
            .or_insert_with(|| (latest.clone(), now));
        if pending.0 != latest {
            *pending = (latest.clone(), now);
        }
        if now.duration_since(pending.1) >= Self::DELAY {
            *shown = latest.clone();
            self.pending.remove(&key);
            return latest;
        }
        shown.clone()
    }

    /// A pending label will need a redraw after this long.
    pub fn next_change(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.pending
            .values()
            .map(|(_, since)| Self::DELAY.saturating_sub(now.duration_since(*since)))
            .min()
    }
}

/// Per-GUI state: the latest pane summaries, debounced tab labels, and the
/// repository of each directory (read from `.git`, cached for 5 s).
#[derive(Default)]
pub struct State {
    panes: std::collections::HashMap<u64, PaneSummary>,
    debounce: Debounce,
    repos: std::collections::HashMap<String, (std::time::Instant, Option<Repo>)>,
}

impl State {
    /// A view's summaries replace the old ones (they cover the whole session).
    pub fn update(&mut self, panes: &[(u64, PaneSummary)]) {
        if !panes.is_empty() {
            self.panes = panes.iter().cloned().collect();
        }
    }

    pub fn pane(&self, id: u64) -> Option<&PaneSummary> {
        self.panes.get(&id)
    }

    fn repo(&mut self, cwd: Option<&str>) -> Option<Repo> {
        let cwd = cwd?;
        let now = std::time::Instant::now();
        if let Some((at, repo)) = self.repos.get(cwd)
            && now.duration_since(*at) < std::time::Duration::from_secs(5)
        {
            return repo.clone();
        }
        let repo = crate::crow::git_repo(std::path::Path::new(cwd)).map(|(top, branch)| Repo {
            name: top
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            branch,
            ..Repo::default()
        });
        if self.repos.len() > 64 {
            self.repos.clear();
        }
        self.repos.insert(cwd.to_owned(), (now, repo.clone()));
        repo
    }

    /// The (debounced, full-length) label of a tab showing `pane`.
    pub fn tab_label(
        &mut self,
        window: u64,
        name: &str,
        pane: Option<u64>,
        agents: &[Agent],
        shell: &str,
    ) -> String {
        // "shell" is the name every window starts with; anything else is the user's.
        let rename = (name != "shell").then_some(name);
        let summary = pane.and_then(|p| self.panes.get(&p)).cloned();
        let agent = summary.as_ref().and_then(|s| agent_for(s, agents)).cloned();
        let repo = self.repo(summary.as_ref().and_then(|s| s.cwd.as_deref()));
        let label = tab_label(&LabelInput {
            rename,
            pane: summary.as_ref(),
            agent: agent.as_ref(),
            repo: repo.as_ref(),
            shell,
        });
        self.debounce
            .label(window, label, std::time::Instant::now())
    }

    /// When a debounced label is due (the UI schedules a redraw).
    pub fn next_change(&self) -> Option<std::time::Duration> {
        self.debounce.next_change(std::time::Instant::now())
    }

    /// Footer segments for the focused `pane` and the global segment.
    pub fn footer(
        &mut self,
        pane: u64,
        context: &crate::crow::Context,
        session: Option<&str>,
    ) -> (Vec<Segment>, Option<Segment>) {
        let summary = self.panes.get(&pane).cloned();
        let agent = summary
            .as_ref()
            .and_then(|s| agent_for(s, &context.agents))
            .cloned();
        // The worker's facts are about the pane it last looked at.
        let current = context.pane == pane;
        let repo = if current && context.repo.is_some() {
            context.repo.clone()
        } else {
            self.repo(summary.as_ref().and_then(|s| s.cwd.as_deref()))
        };
        let home = std::env::var("HOME").ok();
        let segments = footer_segments(&FooterInput {
            pane: summary.as_ref(),
            agent: agent.as_ref(),
            progress: context.progress.as_ref().filter(|_| current),
            repo: repo.as_ref(),
            home: home.as_deref(),
            session,
        });
        (segments, context.global.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(process: &str, title: &str, cwd: Option<&str>) -> PaneSummary {
        PaneSummary {
            title: title.into(),
            process: process.into(),
            args: Vec::new(),
            pid: Some(4242),
            tty: "/dev/ttys004".into(),
            cwd: cwd.map(str::to_owned),
        }
    }

    fn agent(host: &str, cwd: &str, age: u64) -> Agent {
        Agent {
            host: host.into(),
            state: "working".into(),
            cwd: cwd.into(),
            age_seconds: age,
            ..Agent::default()
        }
    }

    #[test]
    fn arguments_are_shown_only_when_they_cannot_be_secrets() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            safe_args("/opt/homebrew/bin/nvim", &args(&["-p", "src/main.rs"])),
            ["main.rs"]
        );
        assert_eq!(safe_args("cargo", &args(&["test", "--lib"])), ["test"]);
        assert!(
            safe_args("curl", &args(&["https://x"])).is_empty(),
            "not allowlisted"
        );
        assert!(safe_args("python3", &args(&["-c", "TOKEN=abc"])).is_empty());
        assert!(safe_args("git", &args(&["https://user:pw@host/r"])).is_empty());
        assert!(safe_args("node", &args(&["api_key=sk-123"])).is_empty());
        assert!(safe_args("npm", &args(&[&"x".repeat(80)])).is_empty());
        assert!(
            safe_args("ssh", &args(&["prod-db"])).is_empty(),
            "hosts are not shown"
        );
    }

    #[test]
    fn tab_labels_follow_the_documented_order() {
        let p = pane("nvim", "", Some("/w/crow-term"));
        let repo = Repo {
            name: "crow-term".into(),
            branch: "main".into(),
            ..Repo::default()
        };
        let mut a = agent("claude", "/w/crow-term", 3);
        a.task = Some("fix login".into());
        let input = |rename, pane, agent, repo| LabelInput {
            rename,
            pane,
            agent,
            repo,
            shell: "/bin/zsh",
        };
        assert_eq!(
            tab_label(&input(Some("api"), Some(&p), Some(&a), Some(&repo))),
            "api"
        );
        let titled = pane("nvim", "main.rs - NVIM", None);
        assert_eq!(
            tab_label(&input(None, Some(&titled), Some(&a), Some(&repo))),
            "main.rs - NVIM"
        );
        assert_eq!(
            tab_label(&input(None, Some(&p), Some(&a), Some(&repo))),
            "claude · fix login"
        );
        let mut with_arg = p.clone();
        with_arg.args = vec!["main.rs".into()];
        assert_eq!(
            tab_label(&input(None, Some(&with_arg), None, Some(&repo))),
            "nvim main.rs"
        );
        let shell = pane("-zsh", "", Some("/w/crow-term"));
        assert_eq!(
            tab_label(&input(None, Some(&shell), None, Some(&repo))),
            "crow-term@main"
        );
        assert_eq!(tab_label(&input(None, Some(&shell), None, None)), "zsh");
        assert_eq!(tab_label(&input(None, None, None, None)), "zsh");
    }

    #[test]
    fn agents_are_matched_by_pid_then_tty_then_directory() {
        let mut by_pid = agent("codex", "/elsewhere", 50);
        by_pid.pid = Some(4242);
        let mut by_tty = agent("gemini", "/elsewhere", 50);
        by_tty.tty = Some("/dev/ttys004".into());
        let old = agent("claude", "/w/crow-term", 300);
        let recent = agent("claude", "/w/crow-term/", 2);
        let other_dir = agent("claude", "/w/api", 1);
        let p = pane("claude", "", Some("/w/crow-term"));
        assert_eq!(
            agent_for(&p, &[old.clone(), by_pid.clone(), by_tty.clone()])
                .unwrap()
                .host,
            "codex"
        );
        assert_eq!(
            agent_for(&p, &[old.clone(), by_tty.clone()]).unwrap().host,
            "gemini"
        );
        let all = [old.clone(), recent.clone(), other_dir.clone()];
        let found = agent_for(&p, &all).unwrap();
        assert_eq!(
            found.age_seconds, 2,
            "most recently active in the same directory"
        );
        // A shell in that directory is not the agent.
        let shell = pane("zsh", "", Some("/w/crow-term"));
        assert!(agent_for(&shell, &[recent]).is_none());
        assert!(agent_for(&p, &[other_dir]).is_none());
    }

    #[test]
    fn git_status_gives_branch_dirty_ahead_behind() {
        let out = "# branch.oid 1234567890abcdef\n# branch.head feature/x\n# branch.upstream origin/feature/x\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b src/ui.rs\n";
        let repo = parse_git_status(out, "/w/crow-term").unwrap();
        assert_eq!(repo.label(), "crow-term@feature/x *↑2↓1");
        let clean =
            parse_git_status("# branch.oid abcdef1234\n# branch.head main\n", "/w/api/").unwrap();
        assert_eq!(clean.label(), "api@main");
        let detached = parse_git_status(
            "# branch.oid abcdef1234\n# branch.head (detached)\n",
            "/w/api",
        )
        .unwrap();
        assert_eq!(detached.label(), "api@abcdef1");
        assert!(parse_git_status("", "/w/api").is_none());
    }

    fn sample() -> Vec<Segment> {
        let mut a = agent("claude", "/w/crow-term", 3);
        a.task = Some("implement the tab context footer and labels".into());
        a.model = Some("claude-opus-5-5".into());
        a.context_percent = Some(57);
        let progress = Progress {
            title: "tab context".into(),
            done: 3,
            total: 8,
            failed: 0,
        };
        let repo = Repo {
            name: "crow-term".into(),
            branch: "feature/tab-context".into(),
            dirty: true,
            ahead: 1,
            behind: 0,
        };
        let p = pane("claude", "", Some("/w/crow-term"));
        footer_segments(&FooterInput {
            pane: Some(&p),
            agent: Some(&a),
            progress: Some(&progress),
            repo: Some(&repo),
            home: None,
            session: Some("work"),
        })
    }

    fn texts(segments: &[Segment]) -> Vec<String> {
        segments.iter().map(|s| s.text.clone()).collect()
    }

    #[test]
    fn the_footer_lists_agent_task_progress_model_repo_session() {
        assert_eq!(
            texts(&sample()),
            [
                "claude ● working",
                "implement the tab context footer and labels",
                "3/8 · 37% ▰▰▰▱▱▱▱▱",
                "opus-5.5 · ctx 57%",
                "crow-term@feature/tab-context *↑1",
                "session work",
            ]
        );
        // Without an agent: the program and its directory.
        let p = pane("nvim", "", Some("/Users/u/w/crow-term"));
        let plain = footer_segments(&FooterInput {
            pane: Some(&p),
            agent: None,
            progress: None,
            repo: None,
            home: Some("/Users/u"),
            session: None,
        });
        assert_eq!(texts(&plain), ["nvim", "~/w/crow-term"]);
    }

    #[test]
    fn narrow_footers_drop_whole_low_priority_segments_first() {
        let chars = |s: &str| s.chars().count() as f32;
        let at = |width: f32| texts(&fit(sample(), width, 3.0, chars));
        assert_eq!(at(240.0).len(), 6, "everything fits at 240 columns");
        assert_eq!(at(160.0).len(), 6, "and at 160");
        let w140 = at(140.0);
        assert_eq!(w140.len(), 5, "{w140:?}");
        assert!(
            !w140.contains(&"session work".to_owned()),
            "lowest priority goes first"
        );
        let w120 = at(120.0);
        assert!(
            !w120.iter().any(|s| s.starts_with("crow-term@")),
            "then the repository: {w120:?}"
        );
        let w100 = at(100.0);
        assert!(w100.len() >= 2 && w100.len() < 6, "{w100:?}");
        assert_eq!(w100[0], "claude ● working");
        let w60 = at(60.0);
        assert_eq!(w60[0], "claude ● working");
        for width in [60.0, 100.0, 160.0, 240.0] {
            let fitted = fit(sample(), width, 3.0, chars);
            let used: f32 = fitted.iter().map(|s| chars(&s.text)).sum::<f32>()
                + 3.0 * fitted.len().saturating_sub(1) as f32;
            assert!(used <= width, "{width}: {used}");
            // Kept segments are whole (never cut) unless only one remains.
            if fitted.len() > 1 {
                assert!(fitted.iter().all(|s| s.text == s.full), "{width}");
            }
        }
        // One segment left that does not fit: shortened, never cut mid-word
        // when a word boundary leaves enough text.
        let task = vec![sample().remove(1)];
        let one = fit(task, 20.0, 3.0, chars);
        assert_eq!(one[0].text, "implement the tab…");
        let name = vec![segment(
            "crow-term@feature/very-long-branch".into(),
            Tone::Plain,
            0,
            Shorten::Middle,
        )];
        assert_eq!(fit(name, 15.0, 3.0, chars)[0].text, "crow-te…-branch");
    }

    #[test]
    fn the_global_segment_counts_only_active_sessions_and_shows_memory_under_pressure() {
        let global = |on, active, memory| {
            global_segment(&GlobalInput {
                crow_on: on,
                active_sessions: active,
                memory,
            })
            .map(|s| s.text)
        };
        assert_eq!(
            global(Some(true), 3, Some(("ok".into(), Some(41)))).unwrap(),
            "crow on · 3 active"
        );
        assert_eq!(
            global(Some(true), 0, Some(("warn".into(), Some(87)))).unwrap(),
            "crow on · mem 87%"
        );
        assert_eq!(global(Some(false), 0, None).unwrap(), "crow off");
        assert_eq!(global(None, 0, None), None);
    }

    #[test]
    fn labels_are_shortened_in_the_middle_and_debounced() {
        assert_eq!(
            middle_ellipsis("crow-term@feature/tab-context", 14),
            "crow-t…context"
        );
        assert_eq!(middle_ellipsis("short", 14), "short");
        assert_eq!(short_model("claude-opus-5-5"), "opus-5.5");
        assert_eq!(short_model("gpt-5-codex"), "gpt-5-codex");
        let start = std::time::Instant::now();
        let at = |ms| start + std::time::Duration::from_millis(ms);
        let mut debounce = Debounce::default();
        assert_eq!(debounce.label(1, "zsh".into(), at(0)), "zsh");
        assert_eq!(
            debounce.label(1, "ls".into(), at(10)),
            "zsh",
            "short-lived: not shown"
        );
        assert_eq!(debounce.label(1, "zsh".into(), at(100)), "zsh");
        assert_eq!(debounce.label(1, "nvim".into(), at(200)), "zsh");
        assert!(debounce.next_change(at(300)).is_some());
        assert_eq!(debounce.label(1, "nvim".into(), at(900)), "nvim");
        assert!(debounce.next_change(at(900)).is_none());
    }

    #[test]
    fn contrast_follows_wcag() {
        assert!((contrast(0x000000, 0xffffff) - 21.0).abs() < 1e-9);
        assert!((contrast(0x777777, 0xffffff) - 4.48).abs() < 0.01);
    }
}
