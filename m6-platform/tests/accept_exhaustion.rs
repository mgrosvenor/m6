//! What `accept` does when the process is out of descriptors, measured on the
//! kernel this test runs on.
//!
//! `harness = false` so the probe can run in a child of this binary. It fills
//! the descriptor table and lowers the process's open-file limit to do so, and
//! that must not happen in the process running the other tests. Nothing else is
//! needed to probe with: the child is this same binary, run with an environment
//! variable set.
//!
//! Both assertions run on every target, and each target states its own answer:
//! on Linux `EMFILE` leaves the queued connection queued and an empty queue is
//! `EMFILE` too; on macOS the `EMFILE` takes and drops the connection and an
//! empty queue is `WouldBlock`.

use std::process::Command;

/// Set in the child, so the same binary measures instead of checking.
const CHILD: &str = "M6_ACCEPT_EXHAUSTION_PROBE";

fn main() {
    if std::env::var_os(CHILD).is_some() {
        measure();
        return;
    }

    let exe = std::env::current_exe().expect("the test binary's path");
    let out = Command::new(exe)
        .env(CHILD, "1")
        .output()
        .expect("run the probe child");
    assert!(
        out.status.success(),
        "the probe child failed ({}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();
    let measured = |field: Option<&str>| match field {
        Some("yes") => true,
        Some("no") => false,
        other => panic!("the probe child said {other:?}, not yes or no:\n{text}"),
    };
    let queued_survives_emfile = measured(fields.next());
    let empty_queue_is_would_block = measured(fields.next());

    // The target's documented answer, and the measurement, must agree. The
    // accept loops are shaped by this: a kernel that leaves the connection
    // queued needs the drain, and one that drops it must not have the refusal
    // counted twice.
    let takes_queued = m6_platform::net::emfile_takes_queued_connection();
    assert_eq!(
        queued_survives_emfile,
        !takes_queued,
        "the queued connection {} survive an `EMFILE`",
        if queued_survives_emfile {
            "did"
        } else {
            "did not"
        }
    );
    assert_eq!(
        empty_queue_is_would_block,
        takes_queued,
        "an empty queue with a full table answered {}",
        if empty_queue_is_would_block {
            "WouldBlock"
        } else {
            "EMFILE"
        }
    );
}

/// The child: measure and report, so the parent can assert.
fn measure() {
    let facts = m6_platform::net::accept_exhaustion().expect("the kernel probe");
    println!(
        "{} {}",
        if facts.queued_survives_emfile {
            "yes"
        } else {
            "no"
        },
        if facts.empty_queue_is_would_block {
            "yes"
        } else {
            "no"
        }
    );
}
