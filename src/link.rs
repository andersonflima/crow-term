//! GUI ↔ daemon link policy, kept pure so it is unit-tested.
//!
//! The daemon serves every request on its own connection and thread, so a slow
//! reply (a very large snapshot, a loaded or sleeping machine) is never a
//! disconnect: while the daemon still accepts connections the GUI keeps
//! retrying without giving up and, at most, says the daemon is busy. Only a
//! daemon that stopped accepting connections is reported as stopped, and the
//! GUI then restarts it and reattaches.
use std::time::Duration;

/// Connecting failed: nothing is listening on the daemon's socket or pipe.
#[derive(Debug)]
pub struct Unreachable(pub std::io::Error);
impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Crow daemon is not running: {}", self.0)
    }
}
impl std::error::Error for Unreachable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// The request never reached the daemon (connect, authentication or writing
/// failed): retrying cannot duplicate it.
#[derive(Debug)]
pub struct NotSent(pub anyhow::Error);
impl std::fmt::Display for NotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for NotSent {}

/// Only failures before the daemon got the bytes. Unreachable is excluded on
/// purpose: a request for a daemon that died must not be replayed into the
/// next daemon, whose panes reuse the same ids.
pub fn not_sent(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<NotSent>())
}

/// The daemon answered, with an error (e.g. the session no longer exists).
#[derive(Debug)]
pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

pub fn refused(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Refused>())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The daemon accepted the connection but the exchange did not complete
    /// (deadline, truncated frame, a clock jump across sleep/wake).
    Transient,
    /// No daemon accepted the connection.
    Unreachable,
}

pub fn classify(error: &anyhow::Error) -> Failure {
    if error.chain().any(|cause| cause.is::<Unreachable>()) {
        Failure::Unreachable
    } else {
        Failure::Transient
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Try again after `after`; replace the status line when `status` is set.
    Retry {
        after: Duration,
        status: Option<String>,
    },
    /// The daemon is gone: start a new one and reattach.
    Restart { status: String },
}

/// Consecutive failures that must pass silently before the status line speaks.
pub const QUIET_TRANSIENT: u32 = 3;
/// Consecutive connection refusals before the daemon counts as stopped (a
/// single refusal can be a momentary accept backlog).
pub const UNREACHABLE_CONFIRM: u32 = 3;
pub const BUSY: &str = "Daemon busy — waiting for it (the session is intact)";
pub const STOPPED: &str = "Daemon stopped — restarting it";
pub const RECONNECTED: &str = "Reconnected to the daemon";

#[derive(Debug, Default)]
pub struct Link {
    transient: u32,
    unreachable: u32,
    /// The status line currently shows a link message that success must clear.
    noted: bool,
}

impl Link {
    /// A reply arrived. Returns the status to show when the link had been
    /// reported as busy or stopped before.
    pub fn ok(&mut self) -> Option<&'static str> {
        self.transient = 0;
        self.unreachable = 0;
        std::mem::take(&mut self.noted).then_some(RECONNECTED)
    }

    pub fn failed(&mut self, failure: Failure) -> Action {
        match failure {
            Failure::Transient => {
                self.unreachable = 0;
                self.transient = self.transient.saturating_add(1);
                let status = (self.transient > QUIET_TRANSIENT && !self.noted).then(|| {
                    self.noted = true;
                    BUSY.to_owned()
                });
                Action::Retry {
                    after: backoff(self.transient),
                    status,
                }
            }
            Failure::Unreachable => {
                self.transient = 0;
                self.unreachable = self.unreachable.saturating_add(1);
                if self.unreachable >= UNREACHABLE_CONFIRM {
                    self.unreachable = 0;
                    self.noted = true;
                    Action::Restart {
                        status: STOPPED.to_owned(),
                    }
                } else {
                    Action::Retry {
                        after: Duration::from_millis(150),
                        status: None,
                    }
                }
            }
        }
    }
}

/// 100 ms, 200 ms, 400 ms … capped at 1 s: a recovered daemon is noticed fast.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((100u64 << attempt.saturating_sub(1).min(4)).min(1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slowness_never_gives_up_and_only_says_busy_once() {
        let mut link = Link::default();
        let mut statuses = Vec::new();
        for _ in 0..1000 {
            match link.failed(Failure::Transient) {
                Action::Retry { after, status } => {
                    assert!(after <= Duration::from_secs(1));
                    statuses.extend(status);
                }
                Action::Restart { .. } => panic!("slowness must never restart the daemon"),
            }
        }
        assert_eq!(statuses, [BUSY]);
        assert!(!BUSY.to_lowercase().contains("disconnect"));
        assert_eq!(link.ok(), Some(RECONNECTED));
        assert_eq!(link.ok(), None, "reconnected is said once");
    }

    #[test]
    fn first_failures_are_silent() {
        let mut link = Link::default();
        for _ in 0..QUIET_TRANSIENT {
            assert!(matches!(
                link.failed(Failure::Transient),
                Action::Retry { status: None, .. }
            ));
        }
        assert_eq!(link.ok(), None, "nothing was shown, nothing to clear");
    }

    #[test]
    fn a_stopped_daemon_is_confirmed_then_restarted() {
        let mut link = Link::default();
        for _ in 1..UNREACHABLE_CONFIRM {
            assert!(matches!(
                link.failed(Failure::Unreachable),
                Action::Retry { status: None, .. }
            ));
        }
        assert_eq!(
            link.failed(Failure::Unreachable),
            Action::Restart {
                status: STOPPED.into()
            }
        );
        // A transient error in between resets the refusal count.
        let mut link = Link::default();
        link.failed(Failure::Unreachable);
        link.failed(Failure::Transient);
        link.failed(Failure::Unreachable);
        assert!(matches!(
            link.failed(Failure::Unreachable),
            Action::Retry { .. }
        ));
    }

    #[test]
    fn classification_looks_through_context() {
        let gone = anyhow::Error::new(Unreachable(std::io::ErrorKind::ConnectionRefused.into()))
            .context("watch");
        assert_eq!(classify(&gone), Failure::Unreachable);
        let timeout =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::TimedOut)).context("read");
        assert_eq!(classify(&timeout), Failure::Transient);
        let answered = anyhow::Error::new(Refused("Window not found in session".into()));
        assert!(refused(&answered) && !refused(&timeout));
        assert_eq!(answered.to_string(), "Window not found in session");
    }

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(backoff(1), Duration::from_millis(100));
        assert_eq!(backoff(2), Duration::from_millis(200));
        assert_eq!(backoff(50), Duration::from_secs(1));
    }
}
