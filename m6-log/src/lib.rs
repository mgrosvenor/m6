//! A buffered JSON-lines logger for a single-threaded event loop.
//!
//! One process has one loop thread. A log call copies one JSON line into a
//! buffer allocated at start, and the loop writes the buffer out when it is
//! nearly full, on its accounting tick and at stop. There is no thread, no
//! channel, no lock and no heap allocation per line. Warnings and errors are
//! written at once.
//!
//! A service's loop makes five short calls, in plain sight:
//!
//! ```text
//! m6_core::log::tick(m6_platform::wall());          // on the accounting tick
//! m6_core::log::watch(poll.registry(), LOG)?;     // after one turn's events
//! LOG => m6_core::log::flush(),                   // in the match on the token
//! Some(Signal::ReopenLogs) => m6_core::log::reopen()?,  // SIGUSR1
//! m6_core::log::stop()?;                          // after the loop
//! ```
//!
//! [`init`] and [`stop`] run at start and stop. `watch` lives in `m6-core`,
//! over [`Sink::fd`] and [`Sink::wants_write`], because `m6-log` does not
//! depend on `mio`.
//!
//! The line format, buffering, loss on crash, levels, reopen and dropped-line
//! counting are described in `docs/m6-log.md`.
//!
//! The writer sees one thread. The Sinks and the level are thread-local, and each
//! thread that calls [`init`] marks itself, and a process-wide flag records that one did. A log call from
//! another thread is a programming error that the macros assert against in a
//! debug build. A release build has no assert, and it drops the other thread's
//! line because only the initialising thread has the Sinks.

#![deny(unsafe_code)]

mod line;
mod macros;
mod sink;

use std::cell::{Cell, RefCell};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

pub use line::{line_at, Field, Level, Line, MAX_FIELD};
pub use sink::{Sink, BUFFER_BYTES};

thread_local! {
    static LEVEL: Cell<Level> = const { Cell::new(Level::Info) };
    static OPERATIONAL: RefCell<Option<Sink>> = const { RefCell::new(None) };
    static ANALYTICS: RefCell<Option<Sink>> = const { RefCell::new(None) };
    /// Whether this thread called [`init`], so it owns Sinks.
    static INITIALISED_HERE: Cell<bool> = const { Cell::new(false) };
}

/// Whether any thread has called [`init`]. With `INITIALISED_HERE`, it lets a
/// debug build name a log call from a thread that did not call `init`, without
/// a lock. The design has one thread per process.
static ANY_INIT: AtomicBool = AtomicBool::new(false);

/// Open the log: set the level and open both Sinks.
///
/// The operational Sink writes to stdout, non-blocking unless stdout is a
/// terminal. The analytics Sink writes to `analytics` when it is given, and
/// regular files never return `EAGAIN`. Called once at start, before the loop.
pub fn init(level: Level, analytics: Option<&Path>) -> io::Result<()> {
    let operational = Sink::stdout()?;
    let analytics = match analytics {
        Some(path) => Some(Sink::file(path)?),
        None => None,
    };
    LEVEL.with(|slot| slot.set(level));
    INITIALISED_HERE.with(|here| here.set(true));
    ANY_INIT.store(true, Ordering::Relaxed);
    OPERATIONAL.with(|slot| *slot.borrow_mut() = Some(operational));
    ANALYTICS.with(|slot| *slot.borrow_mut() = analytics);
    Ok(())
}

/// The loop's accounting tick: flush both Sinks.
///
/// The time is accepted for the loop's shape. The line's timestamp cache
/// refreshes from each line's own clock, so the tick has nothing to do with it.
pub fn tick(_now: SystemTime) {
    flush();
}

/// Flush both Sinks, never waiting on a full destination.
pub fn flush() {
    OPERATIONAL.with(|slot| {
        if let Some(sink) = slot.borrow_mut().as_mut() {
            sink.flush();
        }
    });
    ANALYTICS.with(|slot| {
        if let Some(sink) = slot.borrow_mut().as_mut() {
            sink.flush();
        }
    });
}

/// Flush the analytics Sink and reopen its file at the path. SIGUSR1.
pub fn reopen() -> io::Result<()> {
    ANALYTICS.with(|slot| match slot.borrow_mut().as_mut() {
        Some(sink) => sink.reopen(),
        None => Ok(()),
    })
}

/// Stop the log: finish both Sinks, which restores stdout's flags, writes
/// everything buffered and then the dropped-count line. Called after the loop.
pub fn stop() -> io::Result<()> {
    let operational = OPERATIONAL.with(|slot| slot.borrow_mut().take());
    let analytics = ANALYTICS.with(|slot| slot.borrow_mut().take());
    let mut result = Ok(());
    if let Some(mut sink) = operational {
        if let Err(error) = sink.finish() {
            result = Err(error);
        }
    }
    if let Some(mut sink) = analytics {
        if let Err(error) = sink.finish() {
            if result.is_ok() {
                result = Err(error);
            }
        }
    }
    result
}

/// Build one analytics line with [`Line`] into the analytics Sink.
///
/// m6-http writes its per-request line field by field, so it reaches the Sink
/// directly. The function answers whether the analytics Sink is open.
pub fn with_analytics(f: impl FnOnce(&mut Sink)) -> bool {
    ANALYTICS.with(|slot| match slot.borrow_mut().as_mut() {
        Some(sink) => {
            f(sink);
            true
        }
        None => false,
    })
}

/// Lines the operational Sink has dropped, for the tests.
#[doc(hidden)]
pub fn operational_dropped() -> u64 {
    OPERATIONAL.with(|slot| slot.borrow().as_ref().map_or(0, Sink::dropped))
}

/// Lines the operational Sink has accepted, for the tests.
#[doc(hidden)]
pub fn operational_lines() -> u64 {
    OPERATIONAL.with(|slot| slot.borrow().as_ref().map_or(0, Sink::lines))
}

/// Bytes the operational Sink has written to its destination, for the tests.
#[doc(hidden)]
pub fn operational_written() -> u64 {
    OPERATIONAL.with(|slot| slot.borrow().as_ref().map_or(0, Sink::written))
}

/// Lines the analytics Sink has accepted, for the tests.
#[doc(hidden)]
pub fn analytics_lines() -> u64 {
    ANALYTICS.with(|slot| slot.borrow().as_ref().map_or(0, Sink::lines))
}

/// Lines the analytics Sink has dropped, for the tests.
#[doc(hidden)]
pub fn analytics_dropped() -> u64 {
    ANALYTICS.with(|slot| slot.borrow().as_ref().map_or(0, Sink::dropped))
}

/// Bytes the analytics Sink has written to its destination, for the tests.
#[doc(hidden)]
pub fn analytics_written() -> u64 {
    ANALYTICS.with(|slot| slot.borrow().as_ref().map_or(0, Sink::written))
}

/// Whether a line at `level` is written at the configured level.
#[doc(hidden)]
pub fn __level_enabled(level: Level) -> bool {
    LEVEL.with(|configured| level.enabled(configured.get()))
}

/// Write a line into the operational Sink, if it is open.
#[doc(hidden)]
pub fn __with_operational(f: impl FnOnce(&mut Sink)) -> bool {
    #[cfg(debug_assertions)]
    {
        assert!(
            !ANY_INIT.load(Ordering::Relaxed) || INITIALISED_HERE.with(Cell::get),
            "m6-log: a log call from a thread that did not call init"
        );
    }
    OPERATIONAL.with(|slot| match slot.borrow_mut().as_mut() {
        Some(sink) => {
            f(sink);
            true
        }
        None => false,
    })
}

/// Write a warning or an error at once: flush the Sink after the line.
#[doc(hidden)]
pub fn __flush_if_urgent(level: Level, sink: &mut Sink) {
    if matches!(level, Level::Error | Level::Warn) {
        sink.flush();
    }
}

/// Count one line in the process's log heartbeat.
#[doc(hidden)]
pub fn __pulse_record() {
    PULSE.record();
}

// ── The log heartbeat ───────────────────────────────────────────────────────

/// A heartbeat for the operational log.
///
/// A monitor can see that a process has stopped logging without reading the
/// log: the process knows what it emitted. The count is of lines the level let
/// through, which is the thing worth observing.
pub struct LogPulse {
    events: AtomicU64,
    /// Milliseconds since process start, at the last emitted line.
    last_ms: AtomicU64,
}

impl LogPulse {
    const fn new() -> LogPulse {
        LogPulse {
            events: AtomicU64::new(0),
            last_ms: AtomicU64::new(0),
        }
    }

    fn record(&self) {
        self.events.fetch_add(1, Ordering::Relaxed);
        self.last_ms.store(elapsed_ms(), Ordering::Relaxed);
    }

    /// Lines the operational log has emitted since start.
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// Seconds since the last emitted line, or `None` if none ever was.
    pub fn seconds_since_last(&self) -> Option<u64> {
        if self.events.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some(elapsed_ms().saturating_sub(self.last_ms.load(Ordering::Relaxed)) / 1_000)
    }
}

static PULSE: LogPulse = LogPulse::new();

/// The process-wide log heartbeat.
pub fn pulse() -> &'static LogPulse {
    &PULSE
}

fn elapsed_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}
