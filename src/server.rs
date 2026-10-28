//! A session daemon owns PTYs, not GUI windows. Detaching never sends SIGHUP.
use crate::{config::Config, engine::Engine, ipc, model::*};
use anyhow::{Context, Result, bail, ensure};
use interprocess::local_socket::prelude::*;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

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
    registry: Mutex<Registry>,
    signals: Arc<Signals>,
    stopped: AtomicBool,
    config: Config,
}
impl Server {
    pub fn new(config: &Config) -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            signals: Arc::default(),
            stopped: AtomicBool::new(false),
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
        let engines: Vec<_> = panes
            .into_iter()
            .filter_map(|id| registry.panes.get(&id).cloned().map(|e| (id, e)))
            .collect();
        drop(registry);
        // Read the revision BEFORE snapshots: an output racing with snapshot is never lost.
        let screens = engines
            .into_iter()
            .map(|(id, engine)| Ok((id, engine.snapshot()?)))
            .collect::<Result<_>>()?;
        Ok(View {
            revision,
            sessions,
            screens,
        })
    }
    pub fn handle(&self, request: Request) -> Result<Response> {
        match request {
            Request::List => return Ok(Response::View(self.view(None, 0, None)?)),
            Request::Watch {
                session,
                window,
                since,
            } => {
                self.signals.wait(since);
                return Ok(Response::View(self.view(
                    Some(&session),
                    window,
                    Some(since),
                )?));
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
            Request::Resize { pane, size } => {
                validate_size(size)?;
                self.pane(pane)?.resize(size)?;
            }
            Request::PaneCwd { pane } => return Ok(Response::Cwd(self.pane(pane)?.cwd())),
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

pub fn run(config: Config) -> Result<()> {
    let listener = ipc::listen()?;
    let server = Arc::new(Server::new(&config));
    let active = Arc::new(AtomicUsize::new(0));
    while !server.stopped.load(Ordering::Acquire) {
        match listener.accept() {
            Ok(stream) => {
                if server.stopped.load(Ordering::Acquire) {
                    break;
                }
                if active.load(Ordering::Acquire) >= 64 {
                    drop(stream);
                    continue;
                }
                let server = server.clone();
                let active = active.clone();
                active.fetch_add(1, Ordering::AcqRel);
                std::thread::spawn(move || {
                    struct Guard(Arc<AtomicUsize>);
                    impl Drop for Guard {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    let _guard = Guard(active);
                    let result = (|| -> Result<()> {
                        ipc::check_peer(&stream).context("authenticate IPC client")?;
                        stream
                            .set_nonblocking(true)
                            .context("set server IPC nonblocking mode")?;
                        let mut stream = ipc::FramedStream::new(stream);
                        let request = ipc::read_frame(&mut stream).context("read IPC request")?;
                        let response = server
                            .handle(request)
                            .unwrap_or_else(|e| Response::Error(format!("{e:#}")));
                        ipc::write_frame(&mut stream, &response).context("write IPC response")
                    })();
                    if let Err(error) = result {
                        eprintln!("crow-term IPC: {error:#}");
                    }
                    if server.stopped.load(Ordering::Acquire) {
                        ipc::wake_listener();
                    }
                });
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
