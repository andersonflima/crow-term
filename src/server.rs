//! A session daemon owns PTYs, not GUI windows. Detaching never sends SIGHUP.
use crate::{config::Config, engine::Engine, ipc, model::*};
// Unix: the generic listener's accept(); Windows uses ipc::IpcListener directly.
use anyhow::{Context, Result, bail, ensure};
#[cfg(unix)]
use interprocess::local_socket::prelude::*;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

/// Test-only (cargo feature `test-hooks`, never in release builds): when
/// `<runtime>/test-stall-watch-ms` exists, the next Watch removes it and sleeps
/// that long, simulating a daemon that is alive but slow to answer.
#[cfg(feature = "test-hooks")]
fn stall_for_tests(session: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let Ok(path) = ipc::runtime_dir().map(|dir| dir.join("test-stall-watch")) else {
        return;
    };
    // "<ms> <count> <session>": the next <count> Watches of <session> stall.
    let ms = {
        let _guard = LOCK.lock();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut parts = text.split_whitespace();
        let (Some(Ok(ms)), Some(Ok(count)), Some(target)) = (
            parts.next().map(str::parse::<u64>),
            parts.next().map(str::parse::<u32>),
            parts.next(),
        ) else {
            return;
        };
        if target != session || count == 0 {
            return;
        }
        if count == 1 {
            let _ = std::fs::remove_file(&path);
        } else {
            let _ = std::fs::write(&path, format!("{ms} {} {target}", count - 1));
        }
        ms
    };
    eprintln!("crow-term daemon: test hook stalls a Watch of {session} for {ms} ms");
    std::thread::sleep(Duration::from_millis(ms.min(60_000)));
}

/// Requests slower than this are logged to daemon.log with their cause
/// (`CROW_TERM_SLOW_MS` overrides it; 0 logs every Watch, for measurements).
fn slow() -> Duration {
    static SLOW: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *SLOW.get_or_init(|| {
        Duration::from_millis(
            std::env::var("CROW_TERM_SLOW_MS")
                .ok()
                .and_then(|ms| ms.parse().ok())
                .unwrap_or(500),
        )
    })
}

#[derive(Default)]
struct Signals {
    revision: Mutex<u64>,
    changed: Condvar,
}
impl Signals {
    fn bump(&self) {
        if let Ok(mut revision) = self.revision.lock() {
            *revision = revision.wrapping_add(1);
            self.changed.notify_all();
        }
    }
    fn revision(&self) -> u64 {
        self.revision.lock().map(|r| *r).unwrap_or(0)
    }
    fn wait(&self, since: u64) {
        if let Ok(revision) = self.revision.lock() {
            let _guard = self
                .changed
                .wait_timeout_while(revision, Duration::from_secs(1), |r| *r == since);
        }
    }
}

#[derive(Default)]
struct Registry {
    sessions: BTreeMap<String, Vec<WindowInfo>>,
    panes: HashMap<u64, Arc<Engine>>,
    profiles: HashMap<String, ShellSpec>,
    next_id: u64,
}
impl Registry {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
    fn window_mut(&mut self, session: &str, id: u64) -> Result<&mut WindowInfo> {
        self.sessions
            .get_mut(session)
            .and_then(|windows| windows.iter_mut().find(|w| w.id == id))
            .context("Window not found in session")
    }
    fn infos(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .map(|(name, windows)| SessionInfo {
                name: name.clone(),
                windows: windows.clone(),
            })
            .collect()
    }
    fn authorized_pane(&self, session: &str, window: u64, pane: u64) -> bool {
        self.sessions
            .get(session)
            .and_then(|s| s.iter().find(|w| w.id == window))
            .is_some_and(|w| w.layout.panes().contains(&pane))
    }
}

pub struct Server {
    /// Random per process; see `View::instance`.
    instance: u64,
    registry: Mutex<Registry>,
    signals: Arc<Signals>,
    stopped: AtomicBool,
    /// Pane summaries (tab labels, status bar), refreshed by their own thread
    /// once a second; a view only copies them, never computes them.
    summaries: Mutex<HashMap<u64, crate::model::PaneSummary>>,
    /// When a GUI last watched (ms since start): summaries are refreshed only
    /// while one is attached.
    last_watch: std::sync::atomic::AtomicU64,
    started: std::time::Instant,
    config: Config,
}
impl Server {
    pub fn new(config: &Config) -> Self {
        use std::hash::{BuildHasher, Hasher};
        // RandomState is seeded randomly per process; mix in time and pid.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.as_nanos()),
        );
        hasher.write_u32(std::process::id());
        Self {
            instance: hasher.finish().max(1),
            registry: Mutex::new(Registry::default()),
            signals: Arc::default(),
            stopped: AtomicBool::new(false),
            summaries: Mutex::default(),
            last_watch: std::sync::atomic::AtomicU64::new(0),
            started: std::time::Instant::now(),
            config: config.clone(),
        }
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Registry>> {
        self.registry
            .lock()
            .map_err(|_| anyhow::anyhow!("Session registry poisoned"))
    }
    fn spawn(&self, shell: &ShellSpec, size: Size) -> Result<Arc<Engine>> {
        validate_size(size)?;
        ensure!(
            !shell.program.is_empty() && shell.program.len() < 4096,
            "Invalid shell program"
        );
        let signals = self.signals.clone();
        Ok(Arc::new(Engine::new_with_theme(
            shell,
            size,
            self.config.scrollback,
            Arc::new(move || signals.bump()),
            Some(&self.config),
        )?))
    }
    fn pane(&self, id: u64) -> Result<Arc<Engine>> {
        self.lock()?
            .panes
            .get(&id)
            .cloned()
            .context("Pane not found")
    }
    fn view(&self, session: Option<&str>, window: u64, since: Option<u64>) -> Result<View> {
        // Capture before *any* metadata/screen copy so concurrent mutations cannot be skipped.
        let revision = self.signals.revision();
        let screens = since.is_some_and(|since| revision != since);
        let registry = self.lock()?;
        let panes = if screens {
            session
                .and_then(|s| registry.sessions.get(s))
                .and_then(|w| w.iter().find(|w| w.id == window))
                .map(|w| w.layout.panes())
                .unwrap_or_default()
        } else {
            vec![]
        };
        let sessions = registry.infos();
        // Summaries of every pane of the watched session (tab labels), sent
        // along with screens: copied from the cache the summary thread keeps.
        let summarized: Vec<u64> = if screens {
            session
                .and_then(|s| registry.sessions.get(s))
                .map(|windows| windows.iter().flat_map(|w| w.layout.panes()).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let engines: Vec<_> = panes
            .into_iter()
            .filter_map(|id| registry.panes.get(&id).cloned().map(|e| (id, e)))
            .collect();
        drop(registry);
        // Read the revision BEFORE snapshots: an output racing with snapshot is never lost.
        // Panes are snapshotted concurrently: the cost is the slowest pane,
        // not the sum, and one busy pane cannot delay the others.
        let screens = if engines.len() > 1 {
            std::thread::scope(|scope| {
                let jobs: Vec<_> = engines
                    .iter()
                    .map(|(id, engine)| (*id, scope.spawn(|| engine.snapshot())))
                    .collect();
                jobs.into_iter()
                    .map(|(id, job)| {
                        let screen = job
                            .join()
                            .map_err(|_| anyhow::anyhow!("pane snapshot panicked"))??;
                        Ok((id, screen))
                    })
                    .collect::<Result<_>>()
            })?
        } else {
            engines
                .into_iter()
                .map(|(id, engine)| Ok((id, engine.snapshot()?)))
                .collect::<Result<_>>()?
        };
        let panes = {
            let cache = self
                .summaries
                .lock()
                .map_err(|_| anyhow::anyhow!("summaries poisoned"))?;
            summarized
                .into_iter()
                .filter_map(|id| cache.get(&id).map(|summary| (id, summary.clone())))
                .collect()
        };
        Ok(View {
            instance: self.instance,
            revision,
            sessions,
            screens,
            panes,
        })
    }
    pub fn handle(&self, request: Request) -> Result<Response> {
        match request {
            Request::Guarded { instance, request } => {
                ensure!(
                    instance == self.instance,
                    "Daemon instance changed: command for a previous daemon ignored"
                );
                ensure!(
                    !matches!(*request, Request::Guarded { .. } | Request::Watch { .. }),
                    "Invalid guarded request"
                );
                return self.handle(*request);
            }
            Request::List => return Ok(Response::View(self.view(None, 0, None)?)),
            Request::Watch {
                session,
                window,
                since,
            } => {
                self.last_watch.store(
                    self.started.elapsed().as_millis() as u64 + 1,
                    Ordering::Relaxed,
                );
                self.signals.wait(since);
                #[cfg(feature = "test-hooks")]
                stall_for_tests(&session);
                let started = std::time::Instant::now();
                let view = self.view(Some(&session), window, Some(since))?;
                let elapsed = started.elapsed();
                if elapsed >= slow() {
                    // The GUI keeps waiting (a slow reply is not a disconnect);
                    // the log says why it was slow.
                    eprintln!(
                        "crow-term daemon: Watch snapshot of {} pane(s) took {} ms",
                        view.screens.len(),
                        elapsed.as_millis()
                    );
                }
                return Ok(Response::View(view));
            }
            Request::Ensure {
                session,
                shell,
                size,
            } => {
                validate_name(&session)?;
                let mut registry = self.lock()?;
                if let Some(window) = registry.sessions.get(&session).and_then(|w| w.first()) {
                    return Ok(Response::Created {
                        window: window.id,
                        pane: window.layout.panes()[0],
                    });
                }
                ensure!(registry.sessions.len() < 32, "Session limit (32) reached");
                ensure!(registry.panes.len() < 128, "Pane limit (128) reached");
                let engine = self.spawn(&shell, size)?;
                let pane = registry.id();
                let window = registry.id();
                registry.panes.insert(pane, engine);
                registry.profiles.insert(session.clone(), shell);
                registry.sessions.insert(
                    session,
                    vec![WindowInfo {
                        id: window,
                        name: "shell".into(),
                        layout: Layout::Pane(pane),
                    }],
                );
                self.signals.bump();
                return Ok(Response::Created { window, pane });
            }
            Request::NewWindow {
                session,
                shell: _,
                size,
            } => {
                let mut registry = self.lock()?;
                ensure!(
                    registry
                        .sessions
                        .get(&session)
                        .context("Session not found")?
                        .len()
                        < 32,
                    "Window limit (32) reached"
                );
                ensure!(registry.panes.len() < 128, "Pane limit (128) reached");
                let engine = self.spawn(
                    registry
                        .profiles
                        .get(&session)
                        .context("Session shell profile missing")?,
                    size,
                )?;
                let pane = registry.id();
                let window = registry.id();
                registry.panes.insert(pane, engine);
                registry
                    .sessions
                    .get_mut(&session)
                    .context("Session not found")?
                    .push(WindowInfo {
                        id: window,
                        name: "shell".into(),
                        layout: Layout::Pane(pane),
                    });
                self.signals.bump();
                return Ok(Response::Created { window, pane });
            }
            Request::Split {
                session,
                window,
                pane,
                axis,
                shell: _,
                size,
            } => {
                let mut registry = self.lock()?;
                ensure!(
                    registry.authorized_pane(&session, window, pane),
                    "Pane is not in this window"
                );
                ensure!(
                    registry.window_mut(&session, window)?.layout.panes().len() < 16,
                    "Pane/window limit (16) reached"
                );
                ensure!(registry.panes.len() < 128, "Pane limit (128) reached");
                let engine = self.spawn(
                    registry
                        .profiles
                        .get(&session)
                        .context("Session shell profile missing")?,
                    size,
                )?;
                let new = registry.id();
                ensure!(
                    registry
                        .window_mut(&session, window)?
                        .layout
                        .split(pane, new, axis),
                    "Split target not found"
                );
                registry.panes.insert(new, engine);
                self.signals.bump();
                return Ok(Response::Created { window, pane: new });
            }
            Request::Input { pane, bytes } => {
                ensure!(bytes.len() <= 1024 * 1024, "Input exceeds 1 MiB");
                self.pane(pane)?.input(&bytes)?;
            }
            Request::Resize { pane, size, cell } => {
                validate_size(size)?;
                self.pane(pane)?.resize_with_cell(size, cell)?;
            }
            Request::PaneCwd { pane } => return Ok(Response::Cwd(self.pane(pane)?.cwd())),
            Request::Clipboard { pane } => {
                return Ok(Response::Clipboard(self.pane(pane)?.clipboard()?));
            }
            Request::Scroll { pane, lines } => {
                self.pane(pane)?.scroll(lines.clamp(-10000, 10000))?;
            }
            Request::ClosePane {
                session,
                window,
                pane,
            } => {
                let mut registry = self.lock()?;
                ensure!(
                    registry.authorized_pane(&session, window, pane),
                    "Pane is not in this window"
                );
                // Terminate first: a failure must not orphan a process by removing its owner.
                registry
                    .panes
                    .get(&pane)
                    .context("Pane not found")?
                    .kill()?;
                registry.panes.remove(&pane);
                let windows = registry
                    .sessions
                    .get_mut(&session)
                    .context("Session not found")?;
                let index = windows
                    .iter()
                    .position(|w| w.id == window)
                    .context("Window not found")?;
                if let Some(layout) = windows[index].layout.clone().remove(pane) {
                    windows[index].layout = layout;
                } else {
                    windows.remove(index);
                }
                if windows.is_empty() {
                    registry.sessions.remove(&session);
                    registry.profiles.remove(&session);
                }
            }
            Request::RenameWindow {
                session,
                window,
                name,
            } => {
                ensure!(
                    !name.is_empty()
                        && name.chars().count() <= 80
                        && !name.chars().any(char::is_control),
                    "Invalid window name"
                );
                self.lock()?.window_mut(&session, window)?.name = name;
            }
            Request::KillSession { session } => {
                let mut registry = self.lock()?;
                let windows = registry
                    .sessions
                    .get(&session)
                    .context("Session not found")?;
                let ids: Vec<_> = windows.iter().flat_map(|w| w.layout.panes()).collect();
                for id in &ids {
                    if let Some(engine) = registry.panes.get(id) {
                        engine.kill()?;
                    }
                }
                for id in ids {
                    registry.panes.remove(&id);
                }
                registry.sessions.remove(&session);
                registry.profiles.remove(&session);
            }
            Request::Shutdown { force } => {
                let mut registry = self.lock()?;
                ensure!(
                    force || registry.sessions.is_empty(),
                    "Sessions exist: detach preserves them; stop --force terminates them explicitly"
                );
                for engine in registry.panes.values() {
                    engine.kill()?;
                }
                registry.panes.clear();
                registry.sessions.clear();
                registry.profiles.clear();
                // GUIs must not restart a daemon the user stopped on purpose.
                if let Ok(dir) = ipc::runtime_dir() {
                    let _ = std::fs::write(dir.join(ipc::STOPPED_MARKER), b"stopped\n");
                }
                self.stopped.store(true, Ordering::Release);
            }
        }
        self.signals.bump();
        Ok(Response::Ok)
    }
}

pub fn validate_size(size: Size) -> Result<()> {
    ensure!(
        (1..=200).contains(&size.rows)
            && (2..=400).contains(&size.cols)
            && u32::from(size.rows) * u32::from(size.cols) <= 40000,
        "Terminal size must be 1..200 rows, 2..400 cols, at most 40000 cells"
    );
    Ok(())
}
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || "-_.".contains(c)),
        "Session names: 1..64 bytes, letters/numbers/-/_/."
    );
    Ok(())
}

impl Server {
    /// Summaries serve tab labels and the status bar of attached GUIs: none
    /// when both are off, or when no GUI watched in the last 5 s.
    fn summaries_wanted(&self) -> bool {
        let enabled = self.config.tab_labels || self.config.crow.status_bar;
        let last = self.last_watch.load(Ordering::Relaxed);
        enabled && last > 0 && self.started.elapsed().as_millis() as u64 <= last + 5_000
    }
    /// Recompute every pane's summary (off the request path; one pane at a
    /// time, never waiting on a busy pane) and wake watchers when any changed.
    fn refresh_summaries(&self) {
        let engines: Vec<(u64, Arc<Engine>)> = match self.lock() {
            Ok(registry) => registry
                .panes
                .iter()
                .map(|(id, engine)| (*id, engine.clone()))
                .collect(),
            Err(_) => return,
        };
        // One process table per refresh: who is whose child (agents under a
        // runtime or wrapper belong to the pane whose job started them).
        let children = {
            use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
            let mut system = System::new();
            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
            let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
            for (pid, process) in system.processes() {
                if let Some(parent) = process.parent() {
                    children
                        .entry(parent.as_u32())
                        .or_default()
                        .push(pid.as_u32());
                }
            }
            children
        };
        let fresh: HashMap<u64, crate::model::PaneSummary> = engines
            .into_iter()
            .map(|(id, engine)| (id, engine.summary(Some(&children))))
            .collect();
        let changed = match self.summaries.lock() {
            Ok(mut cache) if *cache != fresh => {
                *cache = fresh;
                true
            }
            _ => false,
        };
        if changed {
            self.signals.bump();
        }
    }
}

pub fn run(config: Config) -> Result<()> {
    // Held for the daemon's whole life: starters use it to tell a live but
    // unresponsive daemon from a dead one (never replace a live daemon).
    let _alive = ipc::hold_daemon_lock()?;
    let _ = std::fs::remove_file(ipc::runtime_dir()?.join(ipc::STOPPED_MARKER));
    let listener = ipc::listen()?;
    let server = Arc::new(Server::new(&config));
    {
        let server = server.clone();
        std::thread::Builder::new()
            .name("crow-summaries".into())
            .spawn(move || {
                while !server.stopped.load(Ordering::Acquire) {
                    if server.summaries_wanted() {
                        server.refresh_summaries();
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            })?;
    }
    let active = Arc::new(AtomicUsize::new(0));
    while !server.stopped.load(Ordering::Acquire) {
        match listener.accept() {
            Ok(stream) => {
                if server.stopped.load(Ordering::Acquire) {
                    break;
                }
                // Never stop accepting: a fixed cap let a burst of slow
                // clients starve every new connect (the GUI's typing among
                // them). Each client costs one short-lived thread; if the OS
                // refuses another, this one connection is dropped and its
                // client retries (it never reached the daemon).
                let server = server.clone();
                let counted = active.clone();
                active.fetch_add(1, Ordering::AcqRel);
                let spawned = std::thread::Builder::new().spawn(move || {
                    let active = counted;
                    struct Guard(Arc<AtomicUsize>);
                    impl Drop for Guard {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    let _guard = Guard(active);
                    let result = (|| -> Result<()> {
                        ipc::check_peer(&stream).context("authenticate IPC client")?;
                        ipc::prepare(&stream).context("configure server IPC stream")?;
                        let mut stream = ipc::FramedStream::new(stream);
                        let request = ipc::read_frame(&mut stream).context("read IPC request")?;
                        let response = server
                            .handle(request)
                            .unwrap_or_else(|e| Response::Error(format!("{e:#}")));
                        let started = std::time::Instant::now();
                        let written = match ipc::write_frame(&mut stream, &response) {
                            // Too large to send (e.g. a giant multi-pane window):
                            // answer with an error the GUI can show, never a cut stream.
                            Err(error) if error.to_string().contains("exceeds limit") => {
                                eprintln!("crow-term daemon: reply too large: {error:#}");
                                ipc::write_frame(
                                    &mut stream,
                                    &Response::Error(
                                        "This window's screen is too large to send (16 MiB IPC limit)"
                                            .into(),
                                    ),
                                )
                            }
                            other => other,
                        }
                        .context("write IPC response");
                        if started.elapsed() >= slow() && slow() > Duration::ZERO {
                            eprintln!(
                                "crow-term daemon: sending a reply took {} ms (slow or stalled client)",
                                started.elapsed().as_millis()
                            );
                        }
                        written
                    })();
                    if let Err(error) = result {
                        eprintln!("crow-term IPC: {error:#}");
                    }
                    if server.stopped.load(Ordering::Acquire) {
                        ipc::wake_listener();
                    }
                });
                if let Err(error) = spawned {
                    active.fetch_sub(1, Ordering::AcqRel);
                    eprintln!("crow-term daemon: cannot serve a client now: {error}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            Err(error) => bail!("IPC accept: {error}"),
        }
    }
    // Let the shutdown RPC send its response before process teardown.
    for _ in 0..100 {
        if active.load(Ordering::Acquire) == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn views_carry_cached_pane_summaries_refreshed_off_the_request_path() {
        let server = Server::new(&Config::default());
        let Response::Created { window, pane } = server
            .handle(Request::Ensure {
                session: "s".into(),
                shell: ShellSpec {
                    program: "/bin/sh".into(),
                    // A background child of the foreground job: in its tree.
                    args: vec!["-c".into(), "sleep 29 & exec sleep 30".into()],
                    cwd: None,
                },
                size: Size::default(),
            })
            .unwrap()
        else {
            panic!("not created");
        };
        // No GUI has watched yet: the summary thread stays idle.
        assert!(!server.summaries_wanted());
        server.last_watch.store(
            server.started.elapsed().as_millis() as u64 + 1,
            Ordering::Relaxed,
        );
        assert!(server.summaries_wanted(), "a GUI is attached");
        // Before the summary thread ran, a view has screens but no summaries:
        // nothing is computed while answering.
        let view = server.view(Some("s"), window, Some(0)).unwrap();
        assert!(!view.screens.is_empty());
        assert!(view.panes.is_empty(), "{:?}", view.panes);
        let revision = server.signals.revision();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let summary = loop {
            server.refresh_summaries();
            let view = server.view(Some("s"), window, Some(0)).unwrap();
            if let Some((_, summary)) = view.panes.iter().find(|(id, _)| *id == pane)
                && summary.process == "sleep"
                && summary.pids.len() >= 2
            {
                break summary.clone();
            }
            assert!(std::time::Instant::now() < deadline, "no summary");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(summary.pid.is_some());
        assert_eq!(summary.pids[0], summary.pid.unwrap(), "the job leader first");
        assert!(
            server.signals.revision() > revision,
            "a changed summary wakes watchers"
        );
        // Unchanged summaries do not wake anyone.
        let settled = server.signals.revision();
        server.refresh_summaries();
        assert_eq!(server.signals.revision(), settled);
        server
            .handle(Request::KillSession {
                session: "s".into(),
            })
            .unwrap();
    }
    #[test]
    fn guarded_commands_run_only_on_the_daemon_instance_they_were_made_for() {
        // Two daemons in a row reuse pane ids from 1: a command made for the
        // first must never reach a pane of the second (another GUI's session).
        let first = Server::new(&Config::default());
        let second = Server::new(&Config::default());
        assert_ne!(first.instance, second.instance);
        let View { instance, .. } = first.view(None, 0, None).unwrap();
        assert_eq!(instance, first.instance);
        let stale = Request::Guarded {
            instance: first.instance,
            request: Box::new(Request::Input {
                pane: 1,
                bytes: b"rm -rf ~".to_vec(),
            }),
        };
        let error = second.handle(stale).unwrap_err().to_string();
        assert!(error.contains("instance changed"), "{error}");
        // The right instance reaches the request itself (here: no such pane).
        let current = Request::Guarded {
            instance: second.instance,
            request: Box::new(Request::Input {
                pane: 1,
                bytes: b"x".to_vec(),
            }),
        };
        assert!(
            second
                .handle(current)
                .unwrap_err()
                .to_string()
                .contains("Pane not found")
        );
        let nested = Request::Guarded {
            instance: second.instance,
            request: Box::new(Request::Guarded {
                instance: 0,
                request: Box::new(Request::List),
            }),
        };
        assert!(second.handle(nested).is_err());
    }
    #[test]
    fn validates_sizes_and_names() {
        assert!(validate_size(Size { rows: 0, cols: 80 }).is_err());
        assert!(
            validate_size(Size {
                rows: 200,
                cols: 400
            })
            .is_err()
        );
        assert!(validate_size(Size::default()).is_ok());
        assert!(validate_name("work-α").is_ok());
        assert!(validate_name("bad\nname").is_err());
    }
    #[test]
    fn layout_split_remove_preserves_sibling() {
        let mut layout = Layout::Pane(1);
        assert!(layout.split(1, 2, Axis::Vertical));
        assert!(layout.split(2, 3, Axis::Horizontal));
        assert_eq!(layout.clone().remove(2).unwrap().panes(), vec![1, 3]);
        assert_eq!(layout.remove(1).unwrap().panes(), vec![2, 3]);
    }
}
