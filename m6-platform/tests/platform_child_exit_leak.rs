//! C7 of the platform contract, in its own test binary.
//!
//! It counts every descriptor the process holds, so no other test may open or
//! close one while it runs: a test binary of its own guarantees that.

use std::process::{Command, Stdio};

use m6_platform::{process, ChildExit};

/// C7. Break: `Drop` forgets the descriptor.
#[test]
fn a_dropped_child_exit_leaks_no_descriptor() {
    let me = std::process::id();
    let mut child = Command::new("bash")
        .args(["-c", "read x"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn bash");
    let before = process::open_descriptors(me).expect("count");
    for _ in 0..64 {
        let exit = ChildExit::new(&child).expect("ChildExit::new");
        drop(exit);
    }
    let after = process::open_descriptors(me).expect("count");
    assert_eq!(before, after, "descriptors before and after 64 drops");
    drop(child.stdin.take());
    child.wait().expect("wait");
}
