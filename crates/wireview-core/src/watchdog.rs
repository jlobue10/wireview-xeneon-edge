//! Watchdog for calls that must not hang.
//!
//! Both daemons can survive a device that is unplugged, busy or answering
//! garbage: those paths return errors and are retried. What neither could
//! survive was a call that never returns - a serial or HID operation stuck
//! in a driver after a USB re-enumeration, say. The process stays alive, so
//! the Scheduled Task's restart settings never fire; the bridge's request
//! handlers queue behind the stuck read until the worker slots are full and
//! new connections are dropped, and the Nexus just stops painting.
//!
//! A [`Watchdog`] hands out a [`Guard`] around each such call. A thread
//! checks four times a second that no guard has been alive longer than the
//! limit; when one has, the process is ended with [`EXIT_CODE`] so the task
//! scheduler starts a fresh one (`RestartCount` in the installers).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

/// Exit status when a watched call overran the limit.
pub const EXIT_CODE: u8 = 3;
/// Default limit. Device operations are bounded at a second or two by their
/// own timeouts, so anything an order of magnitude longer is stuck.
pub const DEFAULT_LIMIT: Duration = Duration::from_secs(20);
/// Longest limit the command line accepts.
pub const MAX_LIMIT: Duration = Duration::from_secs(3600);
const POLL: Duration = Duration::from_millis(250);

/// Called with the stuck call's name and how long it has been running.
type Handler = Box<dyn Fn(&'static str, Duration) + Send>;

struct Inflight {
    next_id: u64,
    calls: BTreeMap<u64, (&'static str, Instant)>,
}

struct Inner {
    limit: Duration,
    inflight: Mutex<Inflight>,
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inflight> {
        self.inflight.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The longest-running watched call, when it has outlived the limit.
    fn overdue(&self) -> Option<(&'static str, Duration)> {
        let now = Instant::now();
        self.lock()
            .calls
            .values()
            .map(|&(what, since)| (what, now.saturating_duration_since(since)))
            .max_by_key(|&(_, waited)| waited)
            .filter(|&(_, waited)| waited >= self.limit)
    }
}

/// Watches calls for one process. Cloning shares the same watchdog.
#[derive(Clone)]
pub struct Watchdog {
    inner: Arc<Inner>,
}

/// Marks a call in flight until dropped.
pub struct Guard {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.inner.lock().calls.remove(&self.id);
    }
}

impl Watchdog {
    /// Start a watchdog that ends the process (exit status [`EXIT_CODE`])
    /// when a watched call runs longer than `limit`.
    pub fn start(limit: Duration) -> Watchdog {
        Self::with_handler(
            limit,
            Box::new(|what, waited| {
                println!(
                    "watchdog: {what} has not returned in {} s; exiting so the scheduled task restarts the daemon",
                    waited.as_secs()
                );
                std::process::exit(i32::from(EXIT_CODE));
            }),
        )
    }

    /// Like [`start`](Self::start) but `handler` decides what happens; it is
    /// called again on every poll while the call stays overdue.
    pub fn with_handler(limit: Duration, handler: Handler) -> Watchdog {
        let inner = Arc::new(Inner {
            limit,
            inflight: Mutex::new(Inflight {
                next_id: 0,
                calls: BTreeMap::new(),
            }),
        });
        let weak: Weak<Inner> = Arc::downgrade(&inner);
        // The thread holds only a weak reference: once the watchdog and all
        // of its guards are gone, it ends on its next poll.
        let spawned = thread::Builder::new().name("wireview-watchdog".into()).spawn(move || {
            loop {
                thread::sleep(POLL);
                let Some(inner) = weak.upgrade() else { return };
                if let Some((what, waited)) = inner.overdue() {
                    handler(what, waited);
                }
            }
        });
        if spawned.is_err() {
            println!("watchdog: cannot start its thread; hangs will not be detected");
        }
        Watchdog { inner }
    }

    /// The limit this watchdog enforces.
    pub fn limit(&self) -> Duration {
        self.inner.limit
    }

    /// Mark `what` as running until the guard is dropped.
    pub fn watch(&self, what: &'static str) -> Guard {
        let mut inflight = self.inner.lock();
        let id = inflight.next_id;
        inflight.next_id = inflight.next_id.wrapping_add(1);
        inflight.calls.insert(id, (what, Instant::now()));
        Guard {
            inner: Arc::clone(&self.inner),
            id,
        }
    }
}

/// Command-line parser for `--watchdog SECONDS`: 0 turns it off, otherwise
/// 1 to 3600 seconds.
pub fn parse_limit(s: &str) -> Result<Duration, String> {
    let secs: u64 = s.trim().parse().map_err(|_| format!("not a whole number of seconds: {s:?}"))?;
    let limit = Duration::from_secs(secs);
    if limit > MAX_LIMIT {
        return Err(format!(
            "the watchdog limit is at most {} seconds (0 turns it off)",
            MAX_LIMIT.as_secs()
        ));
    }
    Ok(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counting(limit: Duration) -> (Watchdog, Arc<AtomicUsize>, Arc<Mutex<Option<&'static str>>>) {
        let trips = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(None));
        let (t, l) = (Arc::clone(&trips), Arc::clone(&last));
        let wd = Watchdog::with_handler(
            limit,
            Box::new(move |what, _| {
                t.fetch_add(1, Ordering::SeqCst);
                *l.lock().unwrap() = Some(what);
            }),
        );
        (wd, trips, last)
    }

    #[test]
    fn a_call_that_returns_in_time_does_not_trip() {
        let (wd, trips, _) = counting(Duration::from_millis(300));
        for _ in 0..3 {
            let _g = wd.watch("quick");
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(700));
        assert_eq!(trips.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_overdue_call_trips_with_its_name() {
        let (wd, trips, last) = counting(Duration::from_millis(200));
        let _quick = wd.watch("quick");
        drop(_quick);
        let _stuck = wd.watch("stuck");
        thread::sleep(Duration::from_millis(900));
        assert!(trips.load(Ordering::SeqCst) >= 1, "the overdue call was not reported");
        assert_eq!(*last.lock().unwrap(), Some("stuck"));
    }

    #[test]
    fn dropping_the_guard_stops_the_reports() {
        let (wd, trips, _) = counting(Duration::from_millis(100));
        let g = wd.watch("stuck for a while");
        thread::sleep(Duration::from_millis(500));
        drop(g);
        let after_drop = trips.load(Ordering::SeqCst);
        assert!(after_drop >= 1);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(
            trips.load(Ordering::SeqCst),
            after_drop,
            "reports continued after the call returned"
        );
    }

    #[test]
    fn the_thread_ends_with_the_last_handle() {
        let (wd, _, _) = counting(Duration::from_millis(100));
        let weak = Arc::downgrade(&wd.inner);
        drop(wd);
        thread::sleep(Duration::from_millis(600));
        assert!(weak.upgrade().is_none(), "the poll thread kept the watchdog alive");
    }

    #[test]
    fn limit_parsing() {
        assert_eq!(parse_limit("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_limit(" 20 ").unwrap(), Duration::from_secs(20));
        assert_eq!(parse_limit("3600").unwrap(), MAX_LIMIT);
        assert!(parse_limit("3601").is_err());
        assert!(parse_limit("-1").is_err());
        assert!(parse_limit("1.5").is_err());
        assert!(parse_limit("soon").is_err());
    }
}
