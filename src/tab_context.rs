//! What a tab and the status bar say about a pane: which program runs in it,
//! which agent (and task) it belongs to, which repository and branch. Pure
//! and unit-tested; the UI only measures and paints the result, and never
//! touches the filesystem for it (the daemon's summary thread reads the
//! repository from `.git` files; nothing ever runs `git`).
//!
//! * Tab label, first that applies: the user's name for the window (rename),
//!   the agent CLI in the foreground and its task, a title the foreground
//!   program set (not a shell's prompt title, a path or the program's own
//!   name), the foreground program, `repo@branch`, the shell.
//! * Footer, for the focused pane, in priority order (lower-priority parts
//!   are dropped first when space runs out): agent and state, task or tool,
//!   run progress, model and context use, `repo@branch`; without an agent,
//!   the foreground program and directory. One compact global part sits on
//!   the right: crow on/off, active agent sessions, memory only under
//!   pressure.
use crate::model::PaneSummary;
use unicode_segmentation::UnicodeSegmentation;

/// Shells: when one is in the foreground the pane is "idle at a prompt".
pub const SHELLS: [&str; 15] = [
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
    "cmd",
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

/// Text from programs and agents as it may be painted: no control, bidi or
/// zero-width characters (they could reorder or hide what is shown).
pub fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|&c| !c.is_control() && !crate::hyperlinks::invisible(c))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// The arguments worth showing for `program` (daemon side, before anything
/// leaves the process): only for allowlisted programs, only the first plain
/// argument that is not the value of a flag (`nvim -u NONE x` → `x`, `npm
/// --otp 123456 publish` → `publish`), and only if it cannot be a secret (no
/// `=`, `@`, `:`, short, plain characters). Paths are reduced to their name.
pub fn safe_args(program: &str, args: &[String]) -> Vec<String> {
    if !SHOW_ARGUMENT.contains(&program_name(program).as_str()) {
        return Vec::new();
    }
    let plain = |arg: &str| {
        !arg.is_empty()
            && arg.chars().count() <= 64
            && arg
                .chars()
                .all(|c| c.is_alphanumeric() || "._/-+".contains(c))
    };
    let mut after_flag = false;
    for arg in args {
        if arg.starts_with('-') {
            // `--flag=value` carries its value; `-f value` takes the next word.
            after_flag = !arg.contains('=') && arg != "-" && arg != "--";
            continue;
        }
        if std::mem::take(&mut after_flag) {
            continue;
        }
        if plain(arg) {
            let name = arg.rsplit('/').next().unwrap_or(arg);
            return vec![if name.is_empty() {
                arg.clone()
            } else {
                name.to_owned()
            }];
        }
        return Vec::new();
    }
    Vec::new()
}

/// Shorten `text` until `fits` accepts it: `…` in the middle (keeping the
/// distinctive end) or, for `Words`, at the end of a word when that leaves
/// at least half of it. Cuts only between grapheme clusters, so a ZWJ
/// sequence, a flag or a letter with its accents stays whole.
pub fn shorten(text: &str, style: Shorten, mut fits: impl FnMut(&str) -> bool) -> Option<String> {
    if fits(text) {
        return Some(text.to_owned());
    }
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    for keep in (1..graphemes.len()).rev() {
        let candidate = match style {
            Shorten::Middle => {
                let tail = keep / 2 + keep % 2;
                let head = keep - tail;
                format!(
                    "{}…{}",
                    graphemes[..head].concat(),
                    graphemes[graphemes.len() - tail..].concat()
                )
            }
            Shorten::Words => {
                let cut = graphemes[..keep].concat();
                match cut.rfind(' ') {
                    Some(at) if cut[..at].graphemes(true).count() >= keep / 2 => {
                        format!("{}…", cut[..at].trim_end())
                    }
                    _ => format!("{cut}…"),
                }
            }
        };
        if fits(&candidate) {
            return Some(candidate);
        }
    }
    fits("…").then(|| "…".to_owned())
}

/// Display width in terminal cells (wide characters count two).
pub fn cells(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// `…` in the middle so that `text` fits `max` cells.
pub fn middle_ellipsis(text: &str, max: usize) -> String {
    shorten(text, Shorten::Middle, |t| cells(t) <= max).unwrap_or_default()
}

/// An agent CLI session as published by crow-skills (only what matching and
/// display need; text already sanitized).
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

/// `name@branch` of the pane's repository (from `PaneSummary::repo`).
fn repo_label(pane: &PaneSummary) -> Option<String> {
    pane.repo
        .as_ref()
        .map(|(name, branch)| format!("{}@{}", sanitize(name), sanitize(branch)))
}

/// The agent session running in `pane`. The pane's foreground program must
/// be that agent CLI; then the session with the pane's process id, else with
/// its terminal, else the only session of that CLI in the pane's directory
/// that reports neither (two of them there: no guess).
pub fn agent_for<'a>(pane: &PaneSummary, agents: &'a [Agent]) -> Option<&'a Agent> {
    let process = program_name(&pane.process);
    if !AGENTS.contains(&process.as_str()) {
        return None;
    }
    let same_cli: Vec<&Agent> = agents.iter().filter(|a| a.host == process).collect();
    if let Some(agent) = same_cli
        .iter()
        .find(|a| a.pid.is_some() && a.pid == pane.pid)
    {
        return Some(agent);
    }
    if !pane.tty.is_empty()
        && let Some(agent) = same_cli
            .iter()
            .find(|a| a.tty.as_deref() == Some(pane.tty.as_str()))
    {
        return Some(agent);
    }
    let cwd = pane.cwd.as_deref()?;
    let mut here = same_cli
        .into_iter()
        .filter(|a| a.pid.is_none() && a.tty.is_none() && same_dir(&a.cwd, cwd));
    let first = here.next()?;
    here.next().is_none().then_some(first)
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

fn looks_like_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.starts_with('/')
        || text.starts_with('~')
        || text.contains('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || text.to_ascii_lowercase().ends_with(".exe")
}

/// A title worth showing: set by a program in the foreground (not by the
/// shell's prompt), not a path (ConPTY's executable path, `~/dir`), not
/// `user@host: dir`, not the program's own name (with or without a path).
pub fn useful_title(pane: &PaneSummary) -> Option<String> {
    if pane.process.is_empty() || is_shell(&pane.process) {
        return None;
    }
    let title = sanitize(&pane.title);
    let program = program_name(&pane.process);
    let lower = title.to_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    let rest = lower[first.len()..].trim();
    let user_host = title
        .split_once('@')
        .is_some_and(|(_, after)| after.contains(':'));
    let useless = title.is_empty()
        || looks_like_path(&title)
        || user_host
        || lower == program
        || (program_name(first) == program && (rest.is_empty() || looks_like_path(rest)));
    (!useless).then_some(title)
}

/// Everything a tab label can be made from.
pub struct LabelInput<'a> {
    /// The window's name when the user renamed it (`None` for the default).
    pub rename: Option<&'a str>,
    pub pane: Option<&'a PaneSummary>,
    pub agent: Option<&'a Agent>,
    pub shell: &'a str,
}

/// The tab label (full; the UI shortens it to the tab width).
pub fn tab_label(input: &LabelInput<'_>) -> String {
    if let Some(name) = input.rename.map(sanitize).filter(|n| !n.is_empty()) {
        return name;
    }
    if let Some(pane) = input.pane {
        if let Some(agent) = input.agent {
            return match agent.task.as_deref().or(agent.tool.as_deref()) {
                Some(task) => format!("{} · {task}", agent.host),
                None => agent.host.clone(),
            };
        }
        if let Some(title) = useful_title(pane) {
            return title;
        }
        if !pane.process.is_empty() && !is_shell(&pane.process) {
            let mut label = program_name(&pane.process);
            for arg in &pane.args {
                label.push(' ');
                label.push_str(&sanitize(arg));
            }
            return label;
        }
        if let Some(repo) = repo_label(pane) {
            return repo;
        }
        if !pane.process.is_empty() {
            return program_name(&pane.process);
        }
    }
    program_name(input.shell)
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
    let text = sanitize(&text);
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
    if let Some(repo) = input.pane.and_then(repo_label) {
        out.push(segment(repo, Tone::Plain, 4, Shorten::Middle));
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

/// Fit `segments` into `width` (in the units `measure` returns, real glyph
/// widths in the UI), with `gap` between them: whole low-priority segments
/// go first; when only one is left and it still does not fit, it is
/// shortened (middle ellipsis for names, at a word for task text). Order is
/// kept.
pub fn fit(
    mut segments: Vec<Segment>,
    width: f32,
    gap: f32,
    mut measure: impl FnMut(&str) -> f32,
) -> Vec<Segment> {
    let total = |segments: &[Segment], measure: &mut dyn FnMut(&str) -> f32| {
        segments.iter().map(|s| measure(&s.text)).sum::<f32>()
            + gap * segments.len().saturating_sub(1) as f32
    };
    while segments.len() > 1 && total(&segments, &mut measure) > width {
        let worst = segments
            .iter()
            .enumerate()
            .max_by_key(|(index, s)| (s.priority, *index))
            .map(|(index, _)| index)
            .unwrap_or(0);
        segments.remove(worst);
    }
    if let Some(only) = segments.first_mut()
        && measure(&only.text) > width
    {
        match shorten(&only.full, only.shorten, |t| measure(t) <= width) {
            Some(text) => only.text = text,
            None => segments.clear(),
        }
    }
    segments
}

/// Shows a label only after it held for `DELAY`, so a tab does not
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

/// Per-GUI state: the latest pane summaries and the debounced tab labels.
/// Pure data: no filesystem access (repositories come in the summaries).
#[derive(Default)]
pub struct State {
    panes: std::collections::HashMap<u64, PaneSummary>,
    debounce: Debounce,
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

    /// The (debounced, full-length) label of a tab showing `pane`.
    pub fn tab_label(
        &mut self,
        window: u64,
        name: &str,
        pane: Option<u64>,
        agents: &[Agent],
        shell: &str,
        now: std::time::Instant,
    ) -> String {
        // "shell" is the name every window starts with; anything else is the user's.
        let rename = (name != "shell").then_some(name);
        let summary = pane.and_then(|p| self.panes.get(&p));
        let agent = summary.and_then(|s| agent_for(s, agents));
        let label = tab_label(&LabelInput {
            rename,
            pane: summary,
            agent,
            shell,
        });
        self.debounce.label(window, label, now)
    }

    /// When a debounced label is due (the UI schedules a redraw).
    pub fn next_change(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.debounce.next_change(now)
    }

    /// Footer segments for the focused `pane` and the global segment.
    pub fn footer(
        &self,
        pane: u64,
        context: &crate::crow::Context,
        session: Option<&str>,
        home: Option<&str>,
    ) -> (Vec<Segment>, Option<Segment>) {
        let summary = self.panes.get(&pane);
        let agent = summary.and_then(|s| agent_for(s, &context.agents));
        // The worker's run progress is about the pane it last looked at.
        let current = context.pane == pane;
        let segments = footer_segments(&FooterInput {
            pane: summary,
            agent,
            progress: context.progress.as_ref().filter(|_| current),
            home,
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
            repo: None,
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

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments_are_shown_only_when_they_cannot_be_secrets() {
        assert_eq!(
            safe_args("/opt/homebrew/bin/nvim", &args(&["src/main.rs"])),
            ["main.rs"]
        );
        assert_eq!(safe_args("cargo", &args(&["test", "--lib"])), ["test"]);
        // A flag's value is never shown.
        assert!(safe_args("nvim", &args(&["-u", "NONE"])).is_empty());
        assert_eq!(
            safe_args("nvim", &args(&["-u", "NONE", "notes.md"])),
            ["notes.md"]
        );
        assert_eq!(
            safe_args("npm", &args(&["--otp", "123456", "publish"])),
            ["publish"]
        );
        assert_eq!(
            safe_args("npm", &args(&["--otp=123456", "publish"])),
            ["publish"]
        );
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
        let mut p = pane("nvim", "", Some("/w/crow-term"));
        p.repo = Some(("crow-term".into(), "main".into()));
        let mut a = agent("claude", "/w/crow-term", 3);
        a.task = Some("fix login".into());
        let label = |rename, pane: &PaneSummary, agent| {
            tab_label(&LabelInput {
                rename,
                pane: Some(pane),
                agent,
                shell: "/bin/zsh",
            })
        };
        let mut agent_pane = pane("claude", "✳ Claude Code", Some("/w/crow-term"));
        agent_pane.repo = p.repo.clone();
        assert_eq!(
            label(Some("api"), &agent_pane, Some(&a)),
            "api",
            "rename wins"
        );
        assert_eq!(
            label(None, &agent_pane, Some(&a)),
            "claude · fix login",
            "agent beats its title"
        );
        let titled = pane("nvim", "main.rs - NVIM", None);
        assert_eq!(label(None, &titled, None), "main.rs - NVIM");
        let mut with_arg = p.clone();
        with_arg.args = vec!["main.rs".into()];
        assert_eq!(label(None, &with_arg, None), "nvim main.rs");
        let mut shell = pane("-zsh", "user@mac: ~/w/crow-term", Some("/w/crow-term"));
        shell.repo = p.repo.clone();
        assert_eq!(
            label(None, &shell, None),
            "crow-term@main",
            "a shell's title is ignored"
        );
        shell.repo = None;
        assert_eq!(label(None, &shell, None), "zsh");
        assert_eq!(
            tab_label(&LabelInput {
                rename: None,
                pane: None,
                agent: None,
                shell: "/bin/zsh"
            }),
            "zsh"
        );
    }

    #[test]
    fn titles_that_say_nothing_are_ignored() {
        let useful = |process: &str, title: &str| useful_title(&pane(process, title, None));
        assert_eq!(
            useful("nvim", "main.rs (~/w) - NVIM").as_deref(),
            Some("main.rs (~/w) - NVIM")
        );
        for (process, title) in [
            ("nvim.exe", "D:\\a\\_temp\\nvim\\bin\\nvim.exe"),
            ("nvim", "C:\\Users\\runneradmin\\"),
            ("htop", "/usr/bin/htop"),
            ("htop", "htop"),
            ("less", "less ~/notes.txt"),
            ("vim", "~/w/crow-term"),
            ("ssh", "user@prod: ~"),
            ("fish", "fish ~/w"),
            ("zsh", "My Project"),
            ("nvim", "\u{202e}"),
        ] {
            assert_eq!(useful(process, title), None, "{process}: {title:?}");
        }
    }

    #[test]
    fn agents_are_matched_only_to_a_pane_running_that_cli() {
        let mut by_pid = agent("claude", "/elsewhere", 50);
        by_pid.pid = Some(4242);
        let mut codex_pid = agent("codex", "/elsewhere", 50);
        codex_pid.pid = Some(4242);
        let mut by_tty = agent("claude", "/elsewhere", 50);
        by_tty.tty = Some("/dev/ttys004".into());
        let p = pane("claude", "", Some("/w/crow-term"));
        assert_eq!(
            agent_for(&p, &[by_tty.clone(), by_pid.clone()])
                .unwrap()
                .pid,
            Some(4242)
        );
        assert!(
            agent_for(&p, &[codex_pid.clone()]).is_none(),
            "same pid, another CLI"
        );
        assert_eq!(
            agent_for(&p, std::slice::from_ref(&by_tty))
                .unwrap()
                .tty
                .as_deref(),
            Some("/dev/ttys004")
        );
        // A shell (or anything else) in front is never an agent, whatever the pid says.
        let shell = pane("zsh", "", Some("/w/crow-term"));
        assert!(agent_for(&shell, &[by_pid.clone(), by_tty.clone()]).is_none());
        // By directory only when exactly one session there reports no pid/tty.
        let one = agent("claude", "/w/crow-term/", 2);
        assert_eq!(
            agent_for(&p, std::slice::from_ref(&one))
                .unwrap()
                .age_seconds,
            2
        );
        let two = [
            agent("claude", "/w/crow-term", 2),
            agent("claude", "/w/crow-term", 9),
        ];
        assert!(
            agent_for(&p, &two).is_none(),
            "two agents in one directory: no guess"
        );
        let mut elsewhere_pid = agent("claude", "/w/crow-term", 1);
        elsewhere_pid.pid = Some(1);
        assert!(
            agent_for(&p, &[elsewhere_pid]).is_none(),
            "its pid says it is another pane"
        );
        assert!(agent_for(&p, &[agent("claude", "/w/api", 1)]).is_none());
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
        let mut p = pane("claude", "", Some("/w/crow-term"));
        p.repo = Some(("crow-term".into(), "feature/tab-context".into()));
        footer_segments(&FooterInput {
            pane: Some(&p),
            agent: Some(&a),
            progress: Some(&progress),
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
                "crow-term@feature/tab-context",
                "session work",
            ]
        );
        // Without an agent: the program and its directory.
        let p = pane("nvim", "", Some("/Users/u/w/crow-term"));
        let plain = footer_segments(&FooterInput {
            pane: Some(&p),
            agent: None,
            progress: None,
            home: Some("/Users/u"),
            session: None,
        });
        assert_eq!(texts(&plain), ["nvim", "~/w/crow-term"]);
        // Invisible and control characters never reach the screen.
        let mut a = agent("claude", "/w", 1);
        a.task = Some("pay \u{202e}lacol\u{200b}\x1b[31m".into());
        let segs = footer_segments(&FooterInput {
            pane: None,
            agent: Some(&a),
            progress: None,
            home: None,
            session: None,
        });
        assert_eq!(segs[1].text, "pay lacol[31m");
    }

    #[test]
    fn narrow_footers_drop_whole_low_priority_segments_first() {
        let width = |s: &str| cells(s) as f32;
        let at = |w: f32| texts(&fit(sample(), w, 3.0, width));
        assert_eq!(at(240.0).len(), 6, "everything fits at 240 columns");
        assert_eq!(at(160.0).len(), 6, "and at 160");
        let w140 = at(140.0);
        assert!(
            !w140.contains(&"session work".to_owned()),
            "lowest priority goes first: {w140:?}"
        );
        let w120 = at(120.0);
        assert!(
            !w120.iter().any(|s| s.starts_with("crow-term@")),
            "then the repository: {w120:?}"
        );
        let w100 = at(100.0);
        assert!(w100.len() >= 2 && w100.len() < 6, "{w100:?}");
        assert_eq!(w100[0], "claude ● working");
        assert_eq!(at(60.0)[0], "claude ● working");
        for w in [60.0, 100.0, 160.0, 240.0] {
            let fitted = fit(sample(), w, 3.0, width);
            let used: f32 = fitted.iter().map(|s| width(&s.text)).sum::<f32>()
                + 3.0 * fitted.len().saturating_sub(1) as f32;
            assert!(used <= w, "{w}: {used}");
            if fitted.len() > 1 {
                assert!(fitted.iter().all(|s| s.text == s.full), "{w}");
            }
        }
        let task = vec![sample().remove(1)];
        assert_eq!(fit(task, 20.0, 3.0, width)[0].text, "implement the tab…");
        let name = vec![segment(
            "crow-term@feature/very-long-branch".into(),
            Tone::Plain,
            0,
            Shorten::Middle,
        )];
        assert_eq!(fit(name, 15.0, 3.0, width)[0].text, "crow-te…-branch");
    }

    #[test]
    fn shortening_counts_wide_cells_and_never_splits_a_cluster() {
        // CJK is two cells per character.
        let cjk = middle_ellipsis("日本語のタブ名前です", 9);
        assert!(cells(&cjk) <= 9, "{cjk}");
        assert!(cjk.contains('…'));
        // A family emoji (ZWJ sequence), a flag and an accented letter stay whole.
        let family = "👨\u{200d}👩\u{200d}👧\u{200d}👦";
        let text = format!("{family}{family} ok 🇧🇷 é\u{301}tude end");
        for max in 1..cells(&text) {
            let short = middle_ellipsis(&text, max);
            assert!(cells(&short) <= max, "{max}: {short}");
            for piece in short.split('…') {
                assert!(text.contains(piece), "{max}: {piece:?} is not a clean cut");
            }
            assert!(
                !short.contains('\u{200d}') || short.contains(family),
                "{max}: {short}"
            );
        }
        // The default agent label fits a default-width tab.
        assert_eq!(
            middle_ellipsis("claude · Fixture task", 24),
            "claude · Fixture task"
        );
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
    fn labels_are_debounced() {
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
}
