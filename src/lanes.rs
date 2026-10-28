//! How the GUI sends commands to the daemon without one command ever holding
//! up another.
//!
//! * **Input lanes**, one per pane: keys, pastes and mouse reports for that
//!   pane, in order. A program that stops reading its input (a paste into
//!   `sleep`, a stalled ssh) only holds its own pane's lane; other panes,
//!   splits and closes keep going. Typed keys are never dropped; a paste is
//!   refused (with a status) only when its pane already has `LANE_LIMIT`
//!   waiting; consecutive mouse-motion reports are coalesced.
//! * **Structural lane**: split, close, new window, rename, clipboard fetch.
//! * **Coalesced lane**: resize (latest size per pane) and scrollback scroll
//!   (summed per pane), so window drags and wheels cannot pile up.
//!
//! Every request waits for a live daemon however long it takes; a request
//! that did not reach a live daemon (refused or timed-out connect: busy) is
//! retried, the same bytes, until it does. Requests are guarded with the
//! daemon instance they were made for and held until that instance is known;
//! a lane belongs to one daemon instance and is discarded with it, so nothing
//! reaches the panes of a restarted daemon (whose pane ids start over).
use crate::model::*;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Bytes per Input request (pastes are split, in order).
pub const CHUNK: usize = 16 * 1024;
/// Pending input per pane beyond which a new paste is refused.
pub const LANE_LIMIT: usize = 64 * 1024 * 1024;
/// A pane whose input has waited this long is reported as not reading.
pub const STUCK_AFTER: Duration = Duration::from_secs(2);
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Bytes(Vec<u8>),
    /// A mouse-motion report: superseded by a later one.
    Motion(Vec<u8>),
    /// A whole paste (with its bracketed-paste markers when the program asked
    /// for them): may be split into chunks, never abandoned half-open.
    Paste(Vec<u8>),
}

/// Pending input of one pane (pure; unit-tested).
#[derive(Debug, Default)]
pub struct Lane {
    items: VecDeque<Item>,
    bytes: usize,
    /// A bracketed paste was partly sent: its end marker is still owed.
    open_paste: bool,
}
impl Lane {
    /// Typed keys and clicks: always accepted (bounded by a person's typing).
    pub fn push_bytes(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        self.bytes += bytes.len();
        self.items.push_back(Item::Bytes(bytes));
    }
    /// A paste: refused when the pane already has too much waiting.
    pub fn push_paste(&mut self, bytes: Vec<u8>) -> Result<(), usize> {
        if self.bytes + bytes.len() > LANE_LIMIT {
            return Err(self.bytes);
        }
        if !bytes.is_empty() {
            self.bytes += bytes.len();
            self.items.push_back(Item::Paste(bytes));
        }
        Ok(())
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
            let (data, paste) = match item {
                Item::Bytes(data) | Item::Motion(data) => (data, false),
                Item::Paste(data) => (data, true),
            };
            let room = CHUNK - out.len();
            if data.len() <= room {
                out.extend_from_slice(data);
                self.items.pop_front();
                if paste {
                    self.open_paste = false;
                }
            } else {
                if !out.is_empty() {
                    break;
                }
                if paste && data.starts_with(PASTE_START) {
                    self.open_paste = true;
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
    /// Drop what is waiting. Returns the end marker still owed to a program
    /// left inside a bracketed paste, which must be sent if the pane lives.
    pub fn abandon(&mut self) -> Option<Vec<u8>> {
        self.items.clear();
        self.bytes = 0;
        std::mem::take(&mut self.open_paste).then(|| PASTE_END.to_vec())
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

/// What a failed request means for the lane that sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It never reached a live daemon (busy): send the same request again.
    Retry,
    /// The pane is gone (closed, program exited, other daemon): discard
    /// quietly what was waiting for it.
    PaneGone,
    /// A real failure, reported to the window.
    Failed,
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
    /// `Some((pane, waiting KiB))` while a pane does not take its input.
    Stuck(Option<(u64, usize)>),
}

struct LaneState {
    lane: Lane,
    /// Since when the in-flight request has been waiting.
    busy_since: Option<Instant>,
    closed: bool,
}

struct InputLane {
    /// The daemon generation this lane belongs to (fixed at creation).
    epoch: u64,
    state: Mutex<LaneState>,
    ready: Condvar,
}

#[derive(Default)]
struct Coalesced {
    epoch: u64,
    resize: HashMap<u64, (Size, Option<CellPixels>)>,
    scroll: HashMap<u64, i32>,
}

/// Performs one request (patient; cancelled when the flag turns false).
type SendFn = Box<dyn Fn(&Request, &AtomicBool) -> anyhow::Result<Response> + Send + Sync>;
type ClassifyFn = Box<dyn Fn(&anyhow::Error) -> Outcome + Send + Sync>;

struct Shared {
    running: Arc<AtomicBool>,
    instance: AtomicU64,
    epoch: AtomicU64,
    lanes: Mutex<HashMap<u64, Arc<InputLane>>>,
    structural: Mutex<VecDeque<(Request, bool, u64)>>,
    structural_ready: Condvar,
    coalesced: Mutex<Coalesced>,
    coalesced_ready: Condvar,
    /// Lanes with a request in flight; the stuck watcher sleeps while none.
    busy: Mutex<usize>,
    busy_ready: Condvar,
    publish: Box<dyn Fn(Event) -> bool + Send + Sync>,
    send: SendFn,
    classify: ClassifyFn,
}

pub struct Dispatcher {
    shared: Arc<Shared>,
    threads: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl Dispatcher {
    /// `send` performs one request (patient, cancelled when `running` turns
    /// false); `classify` says what a failure means; `publish` delivers
    /// events to the window (false once it is gone).
    pub fn new(
        running: Arc<AtomicBool>,
        send: impl Fn(&Request, &AtomicBool) -> anyhow::Result<Response> + Send + Sync + 'static,
        classify: impl Fn(&anyhow::Error) -> Outcome + Send + Sync + 'static,
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
            busy: Mutex::new(0),
            busy_ready: Condvar::new(),
            publish: Box::new(publish),
            send: Box::new(send),
            classify: Box::new(classify),
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

    /// The daemon instance of the latest view. A different non-zero instance
    /// means another daemon (restarted, possibly by another GUI): everything
    /// pending belongs to the old one.
    pub fn set_instance(&self, instance: u64) {
        let previous = self.shared.instance.load(Ordering::Acquire);
        if previous == instance {
            return;
        }
        if previous != 0 {
            self.reattached();
        }
        self.shared.instance.store(instance, Ordering::Release);
        self.wake_all();
    }

    pub fn instance(&self) -> u64 {
        self.shared.instance.load(Ordering::Acquire)
    }

    /// The daemon was replaced: every pending command addressed the old one.
    pub fn reattached(&self) -> u64 {
        self.shared.instance.store(0, Ordering::Release);
        let epoch = self.shared.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        for (_, lane) in lock(&self.shared.lanes).drain() {
            close_lane(&lane);
        }
        lock(&self.shared.structural).clear();
        *lock(&self.shared.coalesced) = Coalesced {
            epoch,
            ..Coalesced::default()
        };
        let _ = (self.shared.publish)(Event::Stuck(None));
        epoch
    }

    pub fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::Acquire)
    }

    pub fn input(&self, pane: u64, bytes: Vec<u8>) {
        let _ = self.lane(pane, |lane| {
            lane.push_bytes(bytes);
            Ok::<(), ()>(())
        });
    }

    /// Err(KiB already waiting) when the pane's lane is full.
    pub fn paste(&self, pane: u64, bytes: Vec<u8>) -> Result<(), usize> {
        self.lane(pane, |lane| lane.push_paste(bytes))
            .map_err(|waiting| waiting.div_ceil(1024))
    }

    pub fn motion(&self, pane: u64, bytes: Vec<u8>) {
        let _ = self.lane(pane, |lane| {
            lane.push_motion(bytes);
            Ok::<(), ()>(())
        });
    }

    fn lane<T>(&self, pane: u64, push: impl FnOnce(&mut Lane) -> Result<(), T>) -> Result<(), T> {
        let mut lanes = lock(&self.shared.lanes);
        let lane = lanes.entry(pane).or_insert_with(|| {
            let lane = Arc::new(InputLane {
                epoch: self.shared.epoch.load(Ordering::Acquire),
                state: Mutex::new(LaneState {
                    lane: Lane::default(),
                    busy_since: None,
                    closed: false,
                }),
                ready: Condvar::new(),
            });
            let shared = self.shared.clone();
            let worker = lane.clone();
            if let Ok(handle) = thread::Builder::new()
                .name(format!("crow-ui-input-{pane}"))
                .spawn(move || input_loop(shared, pane, worker))
            {
                let mut threads = lock(&self.threads);
                threads.retain(|handle| !handle.is_finished());
                threads.push(handle);
            }
            lane
        });
        let result = push(&mut lock(&lane.state).lane);
        lane.ready.notify_all();
        result
    }

    /// Drop the lanes of panes that no longer exist (closed, exited).
    pub fn retain(&self, live: &[u64]) {
        lock(&self.shared.lanes).retain(|pane, lane| {
            let keep = live.contains(pane);
            if !keep {
                close_lane(lane);
            }
            keep
        });
    }

    pub fn lane_count(&self) -> usize {
        lock(&self.shared.lanes).len()
    }

    pub fn resize(&self, pane: u64, size: Size, cell: Option<CellPixels>) {
        let mut pending = lock(&self.shared.coalesced);
        pending.epoch = self.epoch();
        pending.resize.insert(pane, (size, cell));
        self.shared.coalesced_ready.notify_all();
    }

    pub fn scroll(&self, pane: u64, lines: i32) {
        let mut pending = lock(&self.shared.coalesced);
        pending.epoch = self.epoch();
        *pending.scroll.entry(pane).or_default() += lines;
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
        self.shared.busy_ready.notify_all();
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

fn close_lane(lane: &InputLane) {
    let mut state = lock(&lane.state);
    state.closed = true;
    state.lane.abandon();
    lane.ready.notify_all();
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn running(shared: &Shared) -> bool {
    shared.running.load(Ordering::Acquire)
}

/// Send `request` guarded, holding it until the daemon instance is known and
/// resending it while it did not reach a live daemon. `None` when the GUI
/// closed or the daemon was replaced meanwhile.
fn send_guarded(
    shared: &Shared,
    request: &Request,
    epoch: u64,
) -> Option<anyhow::Result<Response>> {
    let mut attempt = 0u32;
    loop {
        if !running(shared) || shared.epoch.load(Ordering::Acquire) != epoch {
            return None;
        }
        let instance = shared.instance.load(Ordering::Acquire);
        // An instance read after a reattach began belongs to the new daemon:
        // the epoch (bumped first) says this request is not for it.
        if shared.epoch.load(Ordering::Acquire) != epoch {
            return None;
        }
        let Some(guarded) = guard(request.clone(), instance) else {
            thread::sleep(Duration::from_millis(20));
            continue;
        };
        let result = (shared.send)(&guarded, &shared.running);
        if shared.epoch.load(Ordering::Acquire) != epoch {
            return None;
        }
        match &result {
            Err(error) if (shared.classify)(error) == Outcome::Retry => {
                attempt = attempt.saturating_add(1);
                thread::sleep(Duration::from_millis((20 * u64::from(attempt)).min(500)));
            }
            _ => return Some(result),
        }
    }
}

/// A lane starts or stops waiting for the daemon to take its request.
fn set_busy(shared: &Shared, state: &mut LaneState, busy: bool) {
    if busy == state.busy_since.is_some() {
        return;
    }
    state.busy_since = busy.then(Instant::now);
    let mut count = lock(&shared.busy);
    if busy {
        *count += 1;
    } else {
        *count = count.saturating_sub(1);
    }
    shared.busy_ready.notify_all();
}

fn report_error(shared: &Shared, epoch: u64, error: anyhow::Error) {
    if running(shared) && (shared.classify)(&error) == Outcome::Failed {
        let _ = (shared.publish)(Event::Error {
            epoch,
            message: format!("Mux command: {error:#}"),
        });
    }
}

fn input_loop(shared: Arc<Shared>, pane: u64, lane: Arc<InputLane>) {
    let epoch = lane.epoch;
    loop {
        let chunk = {
            let mut state = lock(&lane.state);
            loop {
                if state.closed {
                    set_busy(&shared, &mut state, false);
                    return;
                }
                if !running(&shared) {
                    // Closing with a bracketed paste half sent: end it, so the
                    // program does not stay in paste mode (best effort).
                    set_busy(&shared, &mut state, false);
                    if let Some(end) = state.lane.abandon() {
                        drop(state);
                        if let Some(request) = guard(
                            Request::Input { pane, bytes: end },
                            shared.instance.load(Ordering::Acquire),
                        ) {
                            let still = AtomicBool::new(true);
                            let _ = (shared.send)(&request, &still);
                        }
                    }
                    return;
                }
                if let Some(chunk) = state.lane.take() {
                    set_busy(&shared, &mut state, true);
                    break chunk;
                }
                set_busy(&shared, &mut state, false);
                state = lane
                    .ready
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };
        let result = send_guarded(&shared, &Request::Input { pane, bytes: chunk }, epoch);
        if let Some(Err(error)) = result {
            // Pane gone or a real failure: what waited for this pane has
            // nowhere to go (a pane closed on purpose is discarded quietly).
            let end = lock(&lane.state).lane.abandon();
            if let Some(end) = end
                && (shared.classify)(&error) == Outcome::Failed
            {
                // The pane may live on: never leave its program inside a
                // bracketed paste.
                let _ = send_guarded(&shared, &Request::Input { pane, bytes: end }, epoch);
            }
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
                    .wait(queue)
                    .unwrap_or_else(|error| error.into_inner());
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
        let work = {
            let mut pending = lock(&shared.coalesced);
            while pending.resize.is_empty() && pending.scroll.is_empty() && running(&shared) {
                pending = shared
                    .coalesced_ready
                    .wait(pending)
                    .unwrap_or_else(|error| error.into_inner());
            }
            let epoch = pending.epoch;
            Coalesced {
                epoch,
                resize: std::mem::take(&mut pending.resize),
                scroll: std::mem::take(&mut pending.scroll),
            }
        };
        let epoch = work.epoch;
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
        {
            // Nothing in flight and nothing shown: sleep until a lane waits.
            let mut busy = lock(&shared.busy);
            while *busy == 0 && shown.is_none() && running(&shared) {
                busy = shared
                    .busy_ready
                    .wait(busy)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
        thread::sleep(Duration::from_millis(250));
        let stuck = lock(&shared.lanes).iter().find_map(|(&pane, lane)| {
            let state = lock(&lane.state);
            state
                .busy_since
                .filter(|since| since.elapsed() >= STUCK_AFTER)
                .map(|_| (pane, state.lane.pending().div_ceil(1024)))
        });
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
    use std::sync::mpsc;

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
        let mut paste = PASTE_START.to_vec();
        paste.extend((0..100_000u32).map(|i| (i % 251) as u8));
        paste.extend(PASTE_END);
        lane.push_paste(paste.clone()).unwrap();
        lane.push_bytes(b"x".to_vec());
        let mut out = Vec::new();
        while let Some(chunk) = lane.take() {
            assert!(chunk.len() <= CHUNK);
            out.extend(chunk);
        }
        paste.push(b'x');
        assert_eq!(out, paste);
    }

    #[test]
    fn a_half_sent_bracketed_paste_is_always_closed() {
        let mut lane = Lane::default();
        let mut paste = PASTE_START.to_vec();
        paste.extend(vec![b'p'; CHUNK * 3]);
        paste.extend(PASTE_END);
        lane.push_paste(paste).unwrap();
        assert!(lane.take().is_some());
        assert_eq!(lane.abandon(), Some(PASTE_END.to_vec()), "end marker owed");
        assert_eq!(lane.abandon(), None);
        // A fully sent paste owes nothing.
        let mut lane = Lane::default();
        lane.push_paste([PASTE_START, b"hi", PASTE_END].concat())
            .unwrap();
        while lane.take().is_some() {}
        assert_eq!(lane.abandon(), None);
    }

    #[test]
    fn pastes_are_bounded_per_pane_but_keys_never_dropped() {
        let mut lane = Lane::default();
        lane.push_paste(vec![b'p'; LANE_LIMIT - 10]).unwrap();
        assert!(lane.push_paste(vec![b'q'; 11]).is_err(), "over the limit");
        for _ in 0..100 {
            lane.push_bytes(b"k".to_vec());
        }
        assert_eq!(lane.pending(), LANE_LIMIT - 10 + 100);
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

    #[derive(Debug)]
    struct Busy;
    impl std::fmt::Display for Busy {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("connection refused (daemon busy)")
        }
    }
    impl std::error::Error for Busy {}

    fn classify(error: &anyhow::Error) -> Outcome {
        if error.is::<Busy>() {
            Outcome::Retry
        } else if error.to_string().contains("Pane not found") {
            Outcome::PaneGone
        } else {
            Outcome::Failed
        }
    }

    #[test]
    fn a_pane_that_does_not_read_never_holds_up_other_panes_or_splits() {
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
            classify,
            |_| true,
        );
        dispatcher.paste(1, vec![b'p'; 600 * 1024]).unwrap();
        // Held until the daemon instance is known.
        dispatcher.input(2, b"echo B".to_vec());
        assert!(seen.recv_timeout(Duration::from_millis(300)).is_err());
        dispatcher.set_instance(7);
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(5)).unwrap(),
            "input 2 echo B"
        );
        dispatcher.structural(
            Request::Split {
                session: "s".into(),
                window: 1,
                pane: 2,
                axis: Axis::Vertical,
                shell: ShellSpec {
                    program: "sh".into(),
                    args: vec![],
                    cwd: None,
                },
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
        // Lanes of panes that no longer exist are removed.
        dispatcher.retain(&[2]);
        assert_eq!(dispatcher.lane_count(), 1);
        // Closing the GUI ends every lane promptly, the stuck one included.
        let started = Instant::now();
        dispatcher.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_busy_daemon_gets_the_same_bytes_again_in_order_with_no_error() {
        let running = Arc::new(AtomicBool::new(true));
        let (sent, seen) = mpsc::channel::<Vec<u8>>();
        let refusals = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        let (counted, reported) = (refusals.clone(), errors.clone());
        let dispatcher = Dispatcher::new(
            running,
            move |request, _| {
                let Request::Guarded { request, .. } = request else {
                    anyhow::bail!("unguarded");
                };
                let Request::Input { bytes, .. } = &**request else {
                    anyhow::bail!("unexpected");
                };
                // The first five attempts find the accept backlog full.
                if counted.fetch_add(1, Ordering::AcqRel) < 5 {
                    return Err(anyhow::Error::new(Busy));
                }
                let _ = sent.send(bytes.clone());
                Ok(Response::Ok)
            },
            classify,
            move |event| {
                if matches!(event, Event::Error { .. }) {
                    reported.fetch_add(1, Ordering::AcqRel);
                }
                true
            },
        );
        dispatcher.set_instance(1);
        dispatcher.input(4, b"first".to_vec());
        dispatcher.input(4, b"second".to_vec());
        let mut got = Vec::new();
        while got.len() < b"firstsecond".len() {
            got.extend(seen.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        assert_eq!(got, b"firstsecond");
        assert_eq!(errors.load(Ordering::Acquire), 0, "busy is never an error");
        dispatcher.shutdown();
    }

    #[test]
    fn another_daemon_instance_discards_everything_pending_for_the_old_one() {
        let running = Arc::new(AtomicBool::new(true));
        let (sent, seen) = mpsc::channel::<u64>();
        let dispatcher = Dispatcher::new(
            running,
            move |request, running| {
                let Request::Guarded { instance, .. } = request else {
                    anyhow::bail!("unguarded");
                };
                if *instance == 1 {
                    // Old daemon: never answers until the GUI closes.
                    while running.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(5));
                    }
                }
                let _ = sent.send(*instance);
                Ok(Response::Ok)
            },
            classify,
            |_| true,
        );
        dispatcher.set_instance(1);
        dispatcher.input(1, b"for the old daemon".to_vec());
        dispatcher.input(1, b"queued behind it".to_vec());
        thread::sleep(Duration::from_millis(100));
        let epoch = dispatcher.epoch();
        dispatcher.set_instance(2);
        assert_eq!(
            dispatcher.epoch(),
            epoch + 1,
            "an instance change is a reattach"
        );
        assert_eq!(dispatcher.lane_count(), 0, "old lanes discarded");
        dispatcher.input(1, b"for the new daemon".to_vec());
        assert_eq!(seen.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        assert!(
            seen.recv_timeout(Duration::from_millis(300)).is_err(),
            "nothing old replayed"
        );
        dispatcher.shutdown();
    }

    #[test]
    fn a_paste_cut_by_a_failure_still_ends_its_bracketed_paste() {
        let running = Arc::new(AtomicBool::new(true));
        let (sent, seen) = mpsc::channel::<Vec<u8>>();
        let calls = Arc::new(AtomicU64::new(0));
        let counted = calls.clone();
        let dispatcher = Dispatcher::new(
            running,
            move |request, _| {
                let Request::Guarded { request, .. } = request else {
                    anyhow::bail!("unguarded");
                };
                let Request::Input { bytes, .. } = &**request else {
                    anyhow::bail!("unexpected");
                };
                // The second chunk of the paste fails for real.
                if counted.fetch_add(1, Ordering::AcqRel) == 1 {
                    anyhow::bail!("disk on fire");
                }
                let _ = sent.send(bytes.clone());
                Ok(Response::Ok)
            },
            classify,
            |_| true,
        );
        dispatcher.set_instance(1);
        let mut paste = PASTE_START.to_vec();
        paste.extend(vec![b'p'; CHUNK * 3]);
        paste.extend(PASTE_END);
        dispatcher.paste(9, paste).unwrap();
        let first = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(first.starts_with(PASTE_START));
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(5)).unwrap(),
            PASTE_END
        );
        assert!(
            seen.recv_timeout(Duration::from_millis(200)).is_err(),
            "the rest dropped"
        );
        dispatcher.shutdown();
    }
}
