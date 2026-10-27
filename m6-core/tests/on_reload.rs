//! `on_reload` runs after a reload with the new config, and an `Err` from it
//! does not stop the service.
//!
//! This binary has its own `main` (`harness = false` in `Cargo.toml`) because
//! an `App` reads its site directory and config path from the process's own
//! arguments and installs a signal mask, neither of which fits the libtest
//! harness. It re-runs itself: with `M6_ON_RELOAD_CHILD` set it is the service,
//! without it, it is the test that drives the service.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use m6_core::prelude::*;

const CHILD_ENV: &str = "M6_ON_RELOAD_CHILD";

fn until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ready()
}

/// A config carrying `marker`.
fn conf_text(marker: &str) -> String {
    format!("marker = \"{marker}\"\n[thread_pool]\nsize = 2\n")
}

fn main() {
    if std::env::var(CHILD_ENV).is_ok() {
        service();
        return;
    }
    // With the `flash` feature on, an App exits 2 at start unless
    // `flash_secret` reaches `FrameworkState::build`. Core lists that key as a
    // framework key and removes it from the dictionary build reads, so this
    // test's service does not start in that build. That is separate from
    // reload and out of scope here.
    if cfg!(feature = "flash") {
        println!("on_reload ... skipped: a service cannot start with the flash feature");
        return;
    }
    the_hook_sees_the_new_config_and_an_error_is_survived();
    println!("the_hook_sees_the_new_config_and_an_error_is_survived ... ok");
}

/// The service: a global state that records the `marker` key of the config it
/// was last given, in the file named by `HOOK_OUT`.
fn service() {
    let out = std::env::var("HOOK_OUT").expect("HOOK_OUT");
    App::with_global(|_ctx| Ok(()))
        .on_reload(move |_g, ctx| {
            let marker = ctx
                .config
                .get("marker")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if marker == "bad" {
                return Err(Error::from(anyhow::anyhow!("marker was bad")));
            }
            std::fs::write(&out, marker).expect("write marker");
            Ok(())
        })
        .run()
        .expect("run");
}

fn the_hook_sees_the_new_config_and_an_error_is_survived() {
    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("hook.conf");
    let socket = dir.path().join("hook.sock");
    let marker_file = dir.path().join("marker.txt");
    let log = dir.path().join("out.log");
    std::fs::write(&conf, conf_text("one")).unwrap();

    let logfile = std::fs::File::create(&log).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg(dir.path())
        .arg(&conf)
        .env(CHILD_ENV, "1")
        .env("HOOK_OUT", &marker_file)
        .env("M6_SOCKET_OVERRIDE", &socket)
        .stdout(Stdio::from(logfile.try_clone().unwrap()))
        .stderr(Stdio::from(logfile))
        .spawn()
        .expect("spawn service");
    let read_log = || std::fs::read_to_string(&log).unwrap_or_default();

    assert!(
        until(Duration::from_secs(10), || Path::new(&socket).exists()),
        "the service never bound its socket\n{}",
        read_log()
    );

    // The hook has not run: nothing has been reloaded.
    assert!(!marker_file.exists());

    // A new config reaches the hook.
    std::fs::write(&conf, conf_text("two")).unwrap();
    assert!(
        until(Duration::from_secs(10), || {
            std::fs::read_to_string(&marker_file).is_ok_and(|s| s == "two")
        }),
        "on_reload never saw the new config\n{}",
        read_log()
    );

    // A hook that fails is logged, and the service keeps running.
    std::fs::write(&conf, conf_text("bad")).unwrap();
    assert!(
        until(Duration::from_secs(10), || read_log()
            .contains("Reload hook failed")),
        "a failing hook was not logged\n{}",
        read_log()
    );
    assert!(child.try_wait().unwrap().is_none(), "the service exited");

    // And it still reloads afterwards.
    std::fs::write(&conf, conf_text("three")).unwrap();
    assert!(
        until(Duration::from_secs(10), || {
            std::fs::read_to_string(&marker_file).is_ok_and(|s| s == "three")
        }),
        "the service stopped reloading after a failed hook\n{}",
        read_log()
    );

    child.kill().ok();
    child.wait().ok();
}
