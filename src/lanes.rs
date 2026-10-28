//! How the GUI sends commands to the daemon without one command ever holding
//! up another.
//!
//! * **Input lanes**, one per pane: keys, pastes and mouse reports for that
//!   pane, in order. A program that stops reading its input (a paste into
//!   `sleep`, a stalled ssh) only holds its own pane's lane; other panes,
//!   splits and closes keep going. Keys and paste bytes are never dropped;
//!   consecutive mouse-motion reports are coalesced (only the latest matters).
//! * **Structural lane**: split, close, new window, rename, clipboard fetch.
//! * **Coalesced lane**: resize (latest size per pane) and scrollback scroll
//!   (summed per pane), so window drags and wheels cannot pile up.
//!
//! Every request waits for a live daemon however long it takes (no timeout
//! while its process exists), but gives up within about a second once the GUI
//! is closing. Requests are guarded with the daemon instance they were made
//! for and are held until that instance is known, so nothing reaches the panes
//! of a restarted daemon (whose pane ids start over).
use crate::model::*;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Bytes per Input request (bracketed pastes are split, in order).
pub const CHUNK: usize = 16 * 1024;
/// A pane whose input has waited this long is reported as not reading.
pub const STUCK_AFTER: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Bytes(Vec<u8>),
    /// A mouse-motion report: superseded by a later one.
    Motion(Vec<u8>),
}

/// Pending input of one pane (pure; unit-tested).
#[derive(Debug, Default)]
pub struct Lane {
    items: VecDeque<Item>,
    bytes: usize,
}
impl Lane {
    pub fn push_bytes(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.bytes += bytes.len();
        self.items.push_back(Item::Bytes(bytes));
    }
    /// A motion report replaces a motion report still waiting at the tail.
    pub fn push_motion(&mut self, bytes: Vec<u8>) {
        if let Some(Item::Motion(last)) = self.items.back_mut() {
            self.bytes = self.bytes - last.len() + bytes.len();
            *last = bytes;
            return;
        }
        self.bytes += bytes.len();
        self.items.push_back(Item::Motion(bytes));
    }
    pub fn pending(&self) -> usize {
        self.bytes
    }
    /// The next request: queued items concatenated in order, up to `CHUNK`.
    pub fn take(&mut self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(item) = self.items.front_mut() {
            let data = match item {
                Item::Bytes(data) | Item::Motion(data) => data,
            };
            let room = CHUNK - out.len();
            if data.len() <= room {
                out.extend_from_slice(data);
                self.items.pop_front();
            } else {
                if !out.is_empty() {
                    break;
                }
                out.extend(data.drain(..room));
            }
            if out.len() == CHUNK {
                break;
            }
        }
        self.bytes -= out.len();
        (!out.is_empty()).then_some(out)
    }
    pub fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
    }
}

/// Wrap a pane/window-addressed request with the daemon instance it was made
/// for. `None` while the instance is unknown: the caller waits instead of
/// sending something a restarted daemon could misroute.
pub fn guard(request: Request, instance: u64) -> Option<Request> {
    if instance == 0 {
        return None;
    }
    Some(Request::Guarded {
        instance,
        request: Box::new(request),
    })
}

/// Results the lanes report to the window.
pub enum Event {
    Reply {
        response: Response,
        activate_created: bool,
    },
    Error {
        epoch: u64,
        message: String,
    },
    /// `Some((pane, waiting bytes))` while a pane does not take its input.
    Stuck(Option<(u64, usize)>),
}

struct InputLane {
    state: Mutex<(Lane, Option<Instant>)>,
    ready: Condvar,
}

#[derive(Default)]
struct Coalesced {
    resize: HashMap<u64, (Size, Option<CellPixels>)>,
    scroll: HashMap<u64, i32>,
}

/// Performs one request (patient; cancelled when the flag turns false).
type SendFn = Box<dyn Fn(&Request, &AtomicBool) -> anyhow::Result<Response> + Send + Sync>;

struct Shared {
    running: Arc<AtomicBool>,
    instance: AtomicU64,
    epoch: AtomicU64,
    lanes: Mutex<HashMap<u64, Arc<InputLane>>>,
    structural: Mutex<VecDeque<(Request, bool, u64)>>,
    structural_ready: Condvar,
    coalesced: Mutex<Coalesced>,
    coalesced_ready: Condvar,
    publish: Box<dyn Fn(Event) -> bool + Send + Sync>,
    send: SendFn,
}

pub struct Dispatcher {
    shared: Arc<Shared>,
    threads: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl Dispatcher {
    /// `send` performs one request (patient, cancelled when `running` turns
    /// false); `publish` delivers events to the window (false once it is gone).
    pub fn new(
        running: Arc<AtomicBool>,
        send: impl Fn(&Request, &AtomicBool) -> anyhow::Result<Response> + Send + Sync + 'static,
        publish: impl Fn(Event) -> bool + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            running,
            instance: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            lanes: Mutex::default(),
            structural: Mutex::default(),
            structural_ready: Condvar::new(),
            coalesced: Mutex::default(),
            coalesced_ready: Condvar::new(),
            publish: Box::new(publish),
            send: Box::new(send),
        });
        let dispatcher = Self {
            shared,
            threads: Mutex::default(),
        };
        dispatcher.spawn("crow-ui-structural", structural_loop);
        dispatcher.spawn("crow-ui-coalesced", coalesced_loop);
        dispatcher.spawn("crow-ui-stuck", stuck_loop);
        dispatcher
    }

    fn spawn(&self, name: &str, body: fn(Arc<Shared>)) {
        let shared = self.shared.clone();
        if let Ok(handle) = thread::Builder::new()
            .name(name.into())
            .spawn(move || body(shared))
        {
            lock(&self.threads).push(handle);
        }
    }

    pub fn set_instance(&self, instance: u64) {
        if self.shared.instance.swap(instance, Ordering::AcqRel) != instance {
            self.wake_all();
        }
    }

    pub fn instance(&self) -> u64 {
        self.shared.instance.load(Ordering::Acquire)
    }

    /// The daemon was replaced: every pending command addressed the old one.
    pub fn reattached(&self) -> u64 {
        self.shared.instance.store(0, Ordering::Release);
        let epoch = self.shared.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        for lane in lock(&self.shared.lanes).values() {
            lock(&lane.state).0.clear();
        }
        lock(&self.shared.structural).clear();
        *lock(&self.shared.coalesced) = Coalesced::default();
        let _ = (self.shared.publish)(Event::Stuck(None));
        epoch
    }

    pub fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::Acquire)
    }

    pub fn input(&self, pane: u64, bytes: Vec<u8>) {
        self.lane(pane, |lane| lane.push_bytes(bytes));
    }

    pub fn motion(&self, pane: u64, bytes: Vec<u8>) {
        self.lane(pane, |lane| lane.push_motion(bytes));
    }

    fn lane(&self, pane: u64, push: impl FnOnce(&mut Lane)) {
        let mut lanes = lock(&self.shared.lanes);
        let lane = lanes.entry(pane).or_insert_with(|| {
            let lane = Arc::new(InputLane {
                state: Mutex::new((Lane::default(), None)),
                ready: Condvar::new(),
            });
            let shared = self.shared.clone();
            let worker = lane.clone();
            if let Ok(handle) = thread::Builder::new()
                .name(format!("crow-ui-input-{pane}"))
                .spawn(move || input_loop(shared, pane, worker))
            {
                lock(&self.threads).push(handle);
            }
            lane
        });
        push(&mut lock(&lane.state).0);
        lane.ready.notify_all();
    }

    pub fn resize(&self, pane: u64, size: Size, cell: Option<CellPixels>) {
        lock(&self.shared.coalesced)
            .resize
            .insert(pane, (size, cell));
        self.shared.coalesced_ready.notify_all();
    }

    pub fn scroll(&self, pane: u64, lines: i32) {
        *lock(&self.shared.coalesced).scroll.entry(pane).or_default() += lines;
        self.shared.coalesced_ready.notify_all();
    }

    pub fn structural(&self, request: Request, activate_created: bool) {
        let epoch = self.epoch();
        lock(&self.shared.structural).push_back((request, activate_created, epoch));
        self.shared.structural_ready.notify_all();
    }

    fn wake_all(&self) {
        self.shared.structural_ready.notify_all();
        self.shared.coalesced_ready.notify_all();
        for lane in lock(&self.shared.lanes).values() {
            lane.ready.notify_all();
        }
    }

    /// Stop every lane; returns once their threads ended (each observes the
    /// shutdown within about a second, even with a frozen daemon).
    pub fn shutdown(&self) {
        self.shared.running.store(false, Ordering::Release);
        self.wake_all();
        for handle in std::mem::take(&mut *lock(&self.threads)) {
            let _ = handle.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn running(shared: &Shared) -> bool {
    shared.running.load(Ordering::Acquire)
}

/// Send `request` guarded, holding it until the daemon instance is known.
/// `None` when the GUI closed or the daemon was replaced meanwhile.
fn send_guarded(
    shared: &Shared,
    request: &Request,
    epoch: u64,
) -> Option<anyhow::Result<Response>> {
    loop {
        if !running(shared) || shared.epoch.load(Ordering::Acquire) != epoch {
            return None;
        }
        let instance = shared.instance.load(Ordering::Acquire);
        if let Some(guarded) = guard(request.clone(), instance) {
            let result = (shared.send)(&guarded, &shared.running);
            if shared.epoch.load(Ordering::Acquire) != epoch {
                return None;
            }
            return Some(result);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn report_error(shared: &Shared, epoch: u64, error: anyhow::Error) {
    if running(shared) {
        let _ = (shared.publish)(Event::Error {
            epoch,
            message: format!("Mux command: {error:#}"),
        });
    }
}

fn input_loop(shared: Arc<Shared>, pane: u64, lane: Arc<InputLane>) {
    while running(&shared) {
        let (chunk, epoch) = {
            let mut state = lock(&lane.state);
            loop {
                if !running(&shared) {
                    return;
                }
                if let Some(chunk) = state.0.take() {
                    state.1 = Some(Instant::now());
                    break (chunk, shared.epoch.load(Ordering::Acquire));
                }
                state.1 = None;
                state = lane
                    .ready
                    .wait_timeout(state, Duration::from_millis(250))
                    .map(|(state, _)| state)
                    .unwrap_or_else(|error| error.into_inner().0);
            }
        };
        let result = send_guarded(&shared, &Request::Input { pane, bytes: chunk }, epoch);
        if let Some(Err(error)) = result {
            // The pane is gone (closed, or its program exited): its queued
            // input has nowhere to go.
            lock(&lane.state).0.clear();
            report_error(&shared, epoch, error);
        }
    }
}

fn structural_loop(shared: Arc<Shared>) {
    while running(&shared) {
        let next = {
            let mut queue = lock(&shared.structural);
            while queue.is_empty() && running(&shared) {
                queue = shared
                    .structural_ready
                    .wait_timeout(queue, Duration::from_millis(250))
                    .map(|(queue, _)| queue)
                    .unwrap_or_else(|error| error.into_inner().0);
            }
            queue.pop_front()
        };
        let Some((request, activate_created, epoch)) = next else {
            continue;
        };
        match send_guarded(&shared, &request, epoch) {
            Some(Ok(response)) => {
                let _ = (shared.publish)(Event::Reply {
                    response,
                    activate_created,
                });
            }
            Some(Err(error)) => report_error(&shared, epoch, error),
            None => {}
        }
    }
}

fn coalesced_loop(shared: Arc<Shared>) {
    while running(&shared) {
        let (work, epoch) = {
            let mut pending = lock(&shared.coalesced);
            while pending.resize.is_empty() && pending.scroll.is_empty() && running(&shared) {
                pending = shared
                    .coalesced_ready
                    .wait_timeout(pending, Duration::from_millis(250))
                    .map(|(pending, _)| pending)
                    .unwrap_or_else(|error| error.into_inner().0);
            }
            (
                std::mem::take(&mut *pending),
                shared.epoch.load(Ordering::Acquire),
            )
        };
        for (pane, (size, cell)) in work.resize {
            if let Some(Err(error)) =
                send_guarded(&shared, &Request::Resize { pane, size, cell }, epoch)
            {
                report_error(&shared, epoch, error);
            }
        }
        for (pane, lines) in work.scroll {
            if lines != 0
                && let Some(Err(error)) =
                    send_guarded(&shared, &Request::Scroll { pane, lines }, epoch)
            {
                report_error(&shared, epoch, error);
            }
        }
    }
}

/// Reports a pane whose input has waited longer than `STUCK_AFTER`.
fn stuck_loop(shared: Arc<Shared>) {
    let mut shown: Option<(u64, usize)> = None;
    while running(&shared) {
        thread::sleep(Duration::from_millis(250));
        let stuck = lock(&shared.lanes).iter().find_map(|(&pane, lane)| {
            let state = lock(&lane.state);
            state
                .1
                .filter(|since| since.elapsed() >= STUCK_AFTER)
                .map(|_| (pane, state.0.pending()))
        });
        // Round to KiB so the note does not flicker for every key.
        let stuck = stuck.map(|(pane, bytes)| (pane, bytes.div_ceil(1024)));
        if stuck != shown {
            shown = stuck;
            if !(shared.publish)(Event::Stuck(stuck)) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_pastes_keep_their_order_and_motion_is_coalesced() {
        let mut lane = Lane::default();
        lane.push_bytes(b"a".to_vec());
        for x in 0..10_000u32 {
            lane.push_motion(format!("<M{x}>").into_bytes());
        }
        lane.push_bytes(b"b".to_vec());
        lane.push_motion(b"<M1>".to_vec());
        lane.push_motion(b"<M2>".to_vec());
        lane.push_bytes(b"c".to_vec());
        let mut out = Vec::new();
        while let Some(chunk) = lane.take() {
            out.extend(chunk);
        }
        assert_eq!(
            out, b"a<M9999>b<M2>c",
            "every key kept, only the latest motion of a run"
        );
        assert_eq!(lane.pending(), 0);
    }

    #[test]
    fn a_large_paste_is_split_in_order_without_loss() {
        let mut lane = Lane::default();
        let paste: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        lane.push_bytes(b"\x1b[200~".to_vec());
        lane.push_bytes(paste.clone());
        lane.push_bytes(b"\x1b[201~x".to_vec());
        let mut out = Vec::new();
        while let Some(chunk) = lane.take() {
            assert!(chunk.len() <= CHUNK);
            out.extend(chunk);
        }
        let mut expected = b"\x1b[200~".to_vec();
        expected.extend(paste);
        expected.extend(b"\x1b[201~x");
        assert_eq!(out, expected);
    }

    #[test]
    fn nothing_is_sent_before_the_daemon_instance_is_known() {
        assert!(guard(Request::List, 0).is_none());
        let Some(Request::Guarded { instance, request }) = guard(
            Request::Input {
                pane: 3,
                bytes: b"x".to_vec(),
            },
            42,
        ) else {
            panic!("not guarded");
        };
        assert_eq!(instance, 42);
        assert!(matches!(*request, Request::Input { pane: 3, .. }));
    }

    #[test]
    fn a_pane_that_does_not_read_never_holds_up_other_panes_or_splits() {
        use std::sync::mpsc;
        let running = Arc::new(AtomicBool::new(true));
        let (sent, seen) = mpsc::channel::<String>();
        let dispatcher = Dispatcher::new(
            running.clone(),
            move |request, running| {
                let Request::Guarded { request, .. } = request else {
                    anyhow::bail!("unguarded request sent");
                };
                match &**request {
                    // Pane 1's program never reads: its request waits until
                    // the GUI closes (the real client waits for the daemon).
                    Request::Input { pane: 1, .. } => {
                        while running.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_millis(10));
                        }
                        anyhow::bail!("closing")
                    }
                    Request::Input { pane, bytes } => {
                        let _ =
                            sent.send(format!("input {pane} {}", String::from_utf8_lossy(bytes)));
                        Ok(Response::Ok)
                    }
                    Request::Split { .. } => {
                        let _ = sent.send("split".into());
                        Ok(Response::Ok)
                    }
                    Request::ClosePane { pane, .. } => {
                        let _ = sent.send(format!("close {pane}"));
                        Ok(Response::Ok)
                    }
                    other => anyhow::bail!("unexpected {other:?}"),
                }
            },
            |_| true,
        );
        dispatcher.input(1, vec![b'p'; 600 * 1024]);
        // Held until the daemon instance is known.
        dispatcher.input(2, b"echo B".to_vec());
        assert!(seen.recv_timeout(Duration::from_millis(300)).is_err());
        dispatcher.set_instance(7);
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(5)).unwrap(),
            "input 2 echo B"
        );
        let shell = ShellSpec {
            program: "sh".into(),
            args: vec![],
            cwd: None,
        };
        dispatcher.structural(
            Request::Split {
                session: "s".into(),
                window: 1,
                pane: 2,
                axis: Axis::Vertical,
                shell,
                size: Size::default(),
            },
            false,
        );
        assert_eq!(seen.recv_timeout(Duration::from_secs(5)).unwrap(), "split");
        dispatcher.structural(
            Request::ClosePane {
                session: "s".into(),
                window: 1,
                pane: 1,
            },
            false,
        );
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(5)).unwrap(),
            "close 1"
        );
        // Closing the GUI ends every lane promptly, the stuck one included.
        let started = Instant::now();
        dispatcher.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
