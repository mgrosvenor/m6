//! One shared assertion set for every backend example, run directly against the
//! backend's Unix socket.
//!
//! `docs/m6-backend-examples.md` §7 asks for exactly this: one set of checks,
//! parameterised by language, so that adding a language is adding a directory
//! rather than writing a seventh test. Everything here is a requirement from
//! `docs/m6-backend-protocol.md`, and the section number is named at each
//! assertion so a failure points at the clause it broke.
//!
//! The assertions that need the proxy in the path, per §7 of the examples doc
//! (`X-Forwarded-For`, `Via`, compression and caching applied on top of an
//! uncompressed backend, a backend 404 passed through rather than replaced),
//! live in `backends_through_proxy.rs`. They are split because these checks need
//! nothing but a socket, and a failure here should not be hidden behind the cost
//! of standing up a TLS edge.
//!
//! # A missing runtime must not silently pass
//!
//! §8: on a development machine an absent toolchain skips that language with a
//! visible warning; on the build host every runtime must be present and a
//! missing one fails the run. `M6_BACKENDS_REQUIRE_ALL=1` selects the strict
//! behaviour, and `deploy/run-tests.sh` sets it. Without that, a test suite that
//! has never run for four of six languages reports success, which is the
//! "regression test that has never failed" trap this project has already hit
//! once with h2spec and h3spec.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where the examples live, and the one payload file they all serve.
fn backends_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backends")
}

fn payload_path() -> PathBuf {
    backends_dir().join("status.json")
}

/// The bytes every example must return from `/status`, read from the same file
/// the examples themselves read.
fn payload() -> Vec<u8> {
    std::fs::read(payload_path()).expect("tests/backends/status.json must exist")
}

/// Short-lived scratch directory for sockets and built binaries.
///
/// Deliberately under `/tmp` with a short name rather than `tempfile::tempdir`:
/// a Unix socket path is capped at 104 bytes on macOS and 108 on Linux
/// (`sockaddr_un::sun_path`), and macOS temp directories under
/// `/var/folders/...` are long enough to blow that on their own. The C and C++
/// examples check the length and exit 2; Go and Rust fail at bind. Either way it
/// looks like a broken example rather than a path that did not fit.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        // A counter, not just the pid and the language: cargo runs the tests in
        // this file in parallel threads of ONE process, so a name built from
        // pid+language collides between tests. The first version did exactly
        // that, and `remove_dir_all` in one test deleted the binary another test
        // was linking, which surfaced as `ld: open() failed, errno=2`.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = PathBuf::from(format!("/tmp/m6bx-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Which languages exist, and how to build and run each one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lang {
    C,
    Cpp,
    Python,
    Go,
    RustPlain,
    RustM6core,
}

const ALL: [Lang; 6] = [
    Lang::C,
    Lang::Cpp,
    Lang::Python,
    Lang::Go,
    Lang::RustPlain,
    Lang::RustM6core,
];

impl Lang {
    fn dir(self) -> &'static str {
        match self {
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::RustPlain => "rust-plain",
            Lang::RustM6core => "rust-m6core",
        }
    }

    /// The tool that must be present for this language to be testable, and the
    /// name reported when it is not.
    fn toolchain(self) -> &'static str {
        match self {
            Lang::C => "cc",
            Lang::Cpp => "c++",
            Lang::Python => "python3",
            Lang::Go => "go",
            // Built by cargo as part of the workspace, so if the suite is
            // running at all these exist.
            Lang::RustPlain | Lang::RustM6core => "cargo",
        }
    }

    fn tool_present(self) -> bool {
        match self {
            Lang::RustPlain | Lang::RustM6core => true,
            _ => which(self.toolchain()).is_some(),
        }
    }
}

fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(tool))
        .find(|p| p.is_file())
}

/// True on the build host, where a missing runtime is a failure rather than a
/// skip. Set by `deploy/run-tests.sh`.
fn require_all() -> bool {
    std::env::var("M6_BACKENDS_REQUIRE_ALL").is_ok_and(|v| v == "1")
}

/// How long a scratch directory can be untouched before it is assumed abandoned.
///
/// Generous on purpose. This file's own run takes about two minutes, so six
/// hours cannot catch a live one even on a machine building from cold under
/// heavy load, and the cost of being wrong is asymmetric: deleting a live run's
/// binaries breaks that run, while leaving a dead one costs 95MB until the next
/// invocation sweeps it.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Remove scratch directories left behind by runs that are over.
///
/// The directory below is named by process id and nothing used to remove it, so
/// every run of this test binary leaked about 95MB: the compiled C, C++ and Go
/// backends plus `GOCACHE`. `/tmp` on the build host is a 3.7GB **tmpfs**, which
/// that fills in roughly 39 runs.
///
/// It did. Found at 82% on 2026-09-18, and it failed the gate twice over without
/// either failure naming a disk:
///
///     boom_is_relayed_as_a_backend_error -> go link: no space left on device
///     FAIL render:minimal: 264877ns against 201827ns, over the 20% margin
///
/// The performance one is the trap. A 31% regression on a RAM-backed filesystem
/// that is nearly full looks exactly like a real regression, and the run before
/// it on the same commit had passed. Clearing the directories made it pass
/// again. It also gets worse on its own: a fuller tmpfs is less RAM, so the
/// measurement degrades before it breaks.
///
/// Swept by age rather than by asking whether the owning pid is alive. Process
/// ids are recycled, so a liveness check can be wrong in the direction that
/// deletes a running build's output; an age check cannot. Per-process naming is
/// kept, because two concurrent runs must not share an output path -- the
/// comment in `built_binary` records a collision of exactly that shape.
///
/// Best effort throughout. A test that cannot tidy up is not a test that should
/// fail, and a second runner sweeping at the same moment will find entries
/// already gone.
fn sweep_stale_build_dirs() {
    // BOTH roots, because the two kinds of scratch live on different
    // filesystems now. See BUILD_ROOT_DIR for why.
    sweep_stale_build_dirs_in(Path::new("/tmp"), STALE_AFTER);
    sweep_stale_build_dirs_in(&build_root_dir(), STALE_AFTER);
}

/// Where the COMPILED example binaries go, which is not where the sockets go.
///
/// `/var/tmp`, not `/tmp`, since 2026-09-22. The build host was hardened that
/// day and the baseline mounts `/tmp` **noexec** (site issue #98), so every
/// compiled backend became unspawnable the moment it was built:
///
///     cannot spawn c example: Permission denied (os error 13)
///
/// 14 tests failed that way, and the message names the example rather than the
/// mount, so it reads as a broken example.
///
/// The two requirements were never the same and had merely been satisfied by
/// one directory. SOCKETS need a SHORT path, because `sockaddr_un::sun_path`
/// caps at 104 bytes on macOS and 108 on Linux; they stay in `/tmp`.
/// EXECUTABLES need a filesystem that permits exec; they come here. Measured on
/// the hardened box rather than assumed: `/tmp` EXEC BLOCKED, `/var/tmp` EXEC
/// OK.
///
/// It is the better home for them anyway, and fixes a second problem this file
/// already documents. `/tmp` on the build host is a 3.7GB tmpfs, each run
/// leaves about 95MB of compiled C, C++ and Go plus GOCACHE, and that fills it
/// in roughly 39 runs -- which it did, producing a "no space left on device"
/// link failure and a 31% phantom performance regression on a RAM disk that was
/// nearly full. `/var/tmp` is disk-backed, so the big artefacts stop competing
/// with RAM. The sweep still runs, because unbounded growth on disk is only
/// slower, not fine.
/// Resolved once, from m6-core's testkit, because two test files need it and
/// that is the point at which a third copy becomes inevitable. Standing rule 11.
fn build_root_dir() -> std::path::PathBuf {
    m6_core::testkit::exec_scratch_root()
}

/// The sweep, over a named directory and threshold so it can be tested.
///
/// Split out because the alternative is a test that writes into the real `/tmp`
/// and deletes things there, which is the kind of test that is correct until the
/// day it runs somewhere unexpected.
fn sweep_stale_build_dirs_in(root: &Path, stale_after: std::time::Duration) {
    let now = std::time::SystemTime::now();
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("m6bx-build-") && !name.starts_with("m6bx-") {
            continue;
        }
        // Never anything this process owns, whatever its timestamp says. Both
        // shapes carry the pid: `m6bx-build-<pid>` here, and `m6bx-<pid>-<tag>-<n>`
        // from `Scratch::new`. Matching on the pid rather than on one exact name
        // covers both, so a long run cannot sweep its own per-backend scratch.
        let own = format!("-{}", std::process::id());
        if name.starts_with(&format!("m6bx-build{own}")) || name.starts_with(&format!("m6bx{own}-"))
        {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > stale_after);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Compile each language once per test binary, not once per test.
///
/// Seven tests times three compiled languages was twenty-one invocations of a
/// compiler, and on the build host that was most of the 110 seconds this file
/// took. The built binaries do not depend on which test asked for them, so they
/// are cached here and the scratch directory per backend holds only its socket.
fn built_binary(lang: Lang, src: &Path) -> PathBuf {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<&'static str, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    // The lock is held across the compile deliberately: two tests starting the
    // same language at once would otherwise both build, to the same output path,
    // and the second link would truncate the binary the first had just started
    // running. That is the same shape as the scratch-directory collision this
    // file already had once.
    let mut guard = cache.lock().expect("build cache");
    if let Some(p) = guard.get(lang.dir()) {
        return p.clone();
    }

    sweep_stale_build_dirs();

    let dir = build_root_dir().join(format!("m6bx-build-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("build dir");
    let bin = dir.join(lang.dir());

    match lang {
        Lang::C => compile(
            "cc",
            &[
                "-std=c11",
                "-O2",
                "-pthread",
                "-o",
                bin.to_str().unwrap(),
                src.join("main.c").to_str().unwrap(),
            ],
        ),
        Lang::Cpp => compile(
            "c++",
            &[
                "-std=c++17",
                "-O2",
                "-pthread",
                "-o",
                bin.to_str().unwrap(),
                src.join("main.cpp").to_str().unwrap(),
            ],
        ),
        Lang::Go => {
            let out = Command::new("go")
                .args(["build", "-o", bin.to_str().unwrap(), "."])
                .current_dir(src)
                // Go wants a writable cache and the environment may have no HOME.
                .env("GOCACHE", dir.join("gocache"))
                .output()
                .expect("go build");
            assert!(
                out.status.success(),
                "go build failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Lang::Python | Lang::RustPlain | Lang::RustM6core => {
            unreachable!("{} is not compiled here", lang.dir())
        }
    }
    guard.insert(lang.dir(), bin.clone());
    bin
}

/// A running backend, killed and cleaned up on drop.
struct Backend {
    lang: Lang,
    child: Child,
    sock: PathBuf,
    _scratch: Scratch,
}

impl Backend {
    /// Build the example if its language needs building, start it, and wait for
    /// the socket to appear.
    ///
    /// Returns `None` when the toolchain is absent and this is not the build
    /// host, which is the skip path of §8.
    fn start(lang: Lang) -> Option<Backend> {
        if !lang.tool_present() {
            let msg = format!(
                "backend example {}: {} is not installed",
                lang.dir(),
                lang.toolchain()
            );
            assert!(
                !require_all(),
                "{msg}, and M6_BACKENDS_REQUIRE_ALL=1. Every runtime must be \
                 present on the build host: install it rather than letting the \
                 suite report success for a language it never ran."
            );
            eprintln!("SKIPPING {msg} (set M6_BACKENDS_REQUIRE_ALL=1 to make this fatal)");
            return None;
        }

        let scratch = Scratch::new(lang.dir());
        let sock = scratch.path().join("b.sock");
        let src = backends_dir().join(lang.dir());

        let mut cmd = match lang {
            Lang::C | Lang::Cpp | Lang::Go => {
                let mut c = Command::new(built_binary(lang, &src));
                c.arg(&sock).arg(payload_path());
                c
            }
            Lang::Python => {
                let mut c = Command::new("python3");
                c.arg(src.join("main.py")).arg(&sock).arg(payload_path());
                c
            }
            Lang::RustPlain => {
                let mut c = Command::new(m6_core::testkit::binary("m6-example-rust-plain"));
                c.arg(&sock).arg(payload_path());
                c
            }
            Lang::RustM6core => {
                // A core service is configured rather than argument-driven: it
                // takes <site-dir> <config-path> and reads the socket path from
                // config or M6_SOCKET_OVERRIDE. That difference is part of what
                // the example demonstrates, so the harness accommodates it
                // rather than bending the example to match the others.
                let site = scratch.path().join("site");
                std::fs::create_dir_all(&site).unwrap();
                std::fs::copy(payload_path(), site.join("status.json")).unwrap();
                std::fs::write(
                    site.join("site.toml"),
                    "[site]\nname = \"backend-example\"\ndomain = \"localhost\"\n\n\
                     [log]\nlevel = \"warn\"\nformat = \"text\"\n",
                )
                .unwrap();
                let conf = scratch.path().join("example.toml");
                std::fs::write(
                    &conf,
                    // 0666 because protocol §1.2 step 3 requires it. Core's
                    // default is 0660; see docs/m6-backend-examples.md §10 for
                    // why the two documents disagree.
                    "[log]\nlevel = \"warn\"\n\n[server]\nsocket_mode = \"0666\"\n",
                )
                .unwrap();
                let mut c = Command::new(m6_core::testkit::binary("m6-example-rust-m6core"));
                c.arg(&site).arg(&conf).env("M6_SOCKET_OVERRIDE", &sock);
                c
            }
        };

        let child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot spawn {} example: {e}", lang.dir()));

        let b = Backend {
            lang,
            child,
            sock: sock.clone(),
            _scratch: scratch,
        };

        // Bind before announcing readiness (spec §8.1), so the socket appearing
        // is the signal. Waited for rather than slept on: a fixed sleep is
        // either flaky or slow, and under a loaded build host it is both.
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if sock.exists() && UnixStream::connect(&sock).is_ok() {
                return Some(b);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "{} example never bound {} within 20s",
            lang.dir(),
            sock.display()
        );
    }

    /// One request, one response, one connection (spec §1.3).
    fn request(&self, raw: &str) -> Response {
        let mut s = UnixStream::connect(&self.sock)
            .unwrap_or_else(|e| panic!("{}: connect: {e}", self.lang.dir()));
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        Response::parse(&buf, self.lang)
    }

    fn get(&self, path: &str) -> Response {
        self.request(&format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        ))
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock);
    }
}

fn compile(tool: &str, args: &[&str]) {
    let out = Command::new(tool)
        .args(args)
        // -Wall -Wextra deliberately not passed here: this is a conformance
        // test, and the warning-free build of the examples is the build host's
        // job via deploy/run-tests.sh. A warning should fail that gate, not
        // make this test's failure message about compiler output.
        .output()
        .unwrap_or_else(|e| panic!("{tool}: {e}"));
    assert!(
        out.status.success(),
        "{tool} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A parsed HTTP/1.1 response, checked for the framing rules of spec §3.2 as it
/// is parsed rather than afterwards.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn parse(raw: &[u8], lang: Lang) -> Response {
        let sep = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| {
                panic!(
                    "{}: response has no header terminator (spec §3.1)",
                    lang.dir()
                )
            });
        let head = std::str::from_utf8(&raw[..sep]).expect("header section is ASCII");
        let body = raw[sep + 4..].to_vec();

        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap();
        assert!(
            status_line.starts_with("HTTP/1.1 "),
            "{}: status line must begin HTTP/1.1 (spec §3.1), got {status_line:?}",
            lang.dir()
        );
        let status: u16 = status_line[9..12]
            .parse()
            .unwrap_or_else(|_| panic!("{}: unparseable status: {status_line:?}", lang.dir()));

        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line
                .split_once(':')
                .unwrap_or_else(|| panic!("{}: header line without a colon: {line:?}", lang.dir()));
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }

        // Spec §3.2: the proxy refuses a response carrying both
        // Transfer-Encoding and Content-Length, or two different
        // Content-Lengths. Asserted here so a broken example fails with the
        // clause it broke rather than as a 502 three layers away.
        let cls: Vec<&String> = headers
            .iter()
            .filter(|(n, _)| n == "content-length")
            .map(|(_, v)| v)
            .collect();
        assert!(
            cls.len() <= 1,
            "{}: {} Content-Length fields (spec §3.2)",
            lang.dir(),
            cls.len()
        );
        assert!(
            !headers.iter().any(|(n, _)| n == "transfer-encoding") || cls.is_empty(),
            "{}: sent both Transfer-Encoding and Content-Length (spec §3.2)",
            lang.dir()
        );
        assert!(
            head.len() <= 8192,
            "{}: header section is {} bytes, over the 8192 the proxy accepts (spec §3.2)",
            lang.dir(),
            head.len()
        );

        Response {
            status,
            headers,
            body,
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn content_length(&self) -> Option<usize> {
        self.header("content-length").and_then(|v| v.parse().ok())
    }
}

// ── The shared assertion set ────────────────────────────────────────────────

/// Run `f` against every language that can be tested, and assert at the end that
/// at least one was, so an environment with no toolchains at all cannot report a
/// green run.
fn for_each_backend(f: impl Fn(&Backend)) {
    let mut ran = 0;
    for lang in ALL {
        if let Some(b) = Backend::start(lang) {
            f(&b);
            ran += 1;
        }
    }
    assert!(
        ran > 0,
        "no backend example could be tested: every toolchain is missing, which \
         is not a pass"
    );
}

#[test]
fn the_five_routes_answer_as_specified() {
    for_each_backend(|b| {
        let lang = b.lang.dir();

        let root = b.get("/");
        assert_eq!(root.status, 200, "{lang}: / must be 200");
        assert_eq!(
            root.header("content-type"),
            Some("text/html; charset=utf-8"),
            "{lang}: / needs a textual Content-Type WITH a charset (spec §3.4). \
             Omitting the charset makes clients fall back to Latin-1 and render \
             UTF-8 as mojibake, which has happened in production here."
        );

        let health = b.get("/health");
        assert_eq!(health.status, 200, "{lang}: /health must be 200");
        assert_eq!(health.body, b"ok", "{lang}: /health body must be `ok`");
        assert_eq!(
            health.header("content-type"),
            Some("text/plain; charset=utf-8"),
            "{lang}: /health needs text/plain with a charset (spec §3.4)"
        );

        // Spec §4: 5xx is what the proxy counts as a backend error, and what it
        // may replace with a styled error page. A 200 carrying an error page
        // would be invisible to it.
        let boom = b.get("/boom");
        assert_eq!(
            boom.status, 500,
            "{lang}: /boom must be 500 so the proxy sees a backend error (spec §4)"
        );
        assert!(!boom.body.is_empty(), "{lang}: /boom needs a body");

        // Spec §4: a path the backend does not serve SHOULD be 404, not 200, so
        // the status is honest to caches and crawlers.
        let missing = b.get("/no-such-path-exists");
        assert_eq!(
            missing.status, 404,
            "{lang}: an unknown path must 404, not 200 (spec §4)"
        );
        assert!(
            !missing.body.is_empty(),
            "{lang}: the 404 needs a small not-found page \
             (docs/m6-backend-examples.md §3)"
        );
    });
}

#[test]
fn content_length_is_accurate_on_every_response() {
    // Spec §9: "Emit an accurate Content-Length". The proxy validates framing
    // strictly and refuses rather than guessing, because a recipient that
    // guesses is the classic request-smuggling primitive.
    for_each_backend(|b| {
        for path in ["/", "/status", "/health", "/boom", "/nope"] {
            let r = b.get(path);
            let declared = r.content_length().unwrap_or_else(|| {
                panic!(
                    "{}: {path} sent no Content-Length. The spec strongly \
                     RECOMMENDS it and every example is supposed to use it \
                     (spec §3.2)",
                    b.lang.dir()
                )
            });
            assert_eq!(
                declared,
                r.body.len(),
                "{}: {path} declared Content-Length {declared} and delivered {} \
                 bytes (spec §3.2)",
                b.lang.dir(),
                r.body.len()
            );
        }
    });
}

#[test]
fn status_is_byte_identical_in_every_language() {
    // docs/m6-backend-examples.md §5.1 and §7. The benchmark compares this
    // route across languages, so a payload that has quietly diverged would make
    // the comparison meaningless. Asserted, not assumed.
    let expected = payload();
    let mut seen: Vec<(&'static str, Vec<u8>)> = Vec::new();

    for lang in ALL {
        if let Some(b) = Backend::start(lang) {
            let r = b.get("/status");
            assert_eq!(r.status, 200, "{}: /status must be 200", lang.dir());
            assert_eq!(
                r.header("content-type"),
                Some("application/json; charset=utf-8"),
                "{}: /status must be JSON with a charset (spec §3.4)",
                lang.dir()
            );
            assert_eq!(
                r.body,
                expected,
                "{}: /status is not the bytes of tests/backends/status.json. \
                 Every example reads that one file and must serve it verbatim: \
                 re-serialising it is what lets the six drift apart.",
                lang.dir()
            );
            seen.push((lang.dir(), r.body));
        }
    }

    assert!(!seen.is_empty(), "no language was tested");
    // Belt and braces: compare them to each other as well as to the file, so a
    // wrong expectation file cannot make a divergence invisible.
    for (name, body) in &seen[1..] {
        assert_eq!(
            *body, seen[0].1,
            "{name} and {} disagree about /status",
            seen[0].0
        );
    }
}

#[test]
fn head_sends_the_length_but_no_body() {
    // Spec §3.3: no body for HEAD, and the Content-Length the equivalent GET
    // would have produced SHOULD still be sent. Sending a body here makes the
    // proxy mis-frame the response or stall until timeout.
    for_each_backend(|b| {
        let lang = b.lang.dir();
        let get = b.get("/status");
        let head =
            b.request("HEAD /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");

        assert_eq!(head.status, 200, "{lang}: HEAD /status must be 200");
        assert!(
            head.body.is_empty(),
            "{lang}: HEAD sent {} body bytes; it must send none (spec §3.3)",
            head.body.len()
        );
        assert_eq!(
            head.content_length(),
            get.content_length(),
            "{lang}: HEAD must report the Content-Length of the equivalent GET \
             (spec §3.3)"
        );
    });
}

#[test]
fn the_socket_is_world_read_write() {
    // Spec §1.2 step 3, called out there as "the single most common cause of a
    // backend that starts cleanly and is never contacted": the proxy runs as a
    // different user and cannot connect otherwise.
    use std::os::unix::fs::PermissionsExt;
    for_each_backend(|b| {
        let mode = std::fs::metadata(&b.sock)
            .expect("socket exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode,
            0o666,
            "{}: socket mode is {mode:04o}, spec §1.2 step 3 requires 0666",
            b.lang.dir()
        );
    });
}

#[test]
fn a_request_body_is_drained_not_ignored() {
    // Spec §2.6: the backend MUST read exactly Content-Length bytes. A backend
    // that ignores the body leaves those bytes queued on a connection it is
    // about to close; one that reads past it blocks until the proxy's timeout.
    // Either way the symptom is a stall rather than an error, so it is worth a
    // test of its own.
    for_each_backend(|b| {
        let body = "x".repeat(512);
        let r = b.request(&format!(
            "POST /health HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        ));
        // The status is deliberately not asserted. Four examples route on path
        // alone and answer 200; m6-core routes on method, so `route_get` does
        // not match a POST and core answers 404. Both are conformant, and
        // arguing about which is nicer would be this test drifting off its
        // subject.
        //
        // What it actually guards is that a response ARRIVES, framed, without
        // the connection stalling. `Response::parse` has already checked the
        // framing by the time this runs, and a backend that failed to drain the
        // body would have hung instead of reaching here, so getting this far
        // with any status is the pass condition.
        assert!(
            (100..600).contains(&r.status),
            "{}: a POST with a body produced status {}, which is not a status \
             (spec §3.1)",
            b.lang.dir(),
            r.status
        );
    });
}

#[test]
fn sigterm_drains_removes_the_socket_and_exits_zero() {
    // Spec §8.2 and §8.3. Removing the socket is what withdraws the member from
    // the pool: a backend that exits leaving the file behind makes the proxy
    // keep selecting a member that refuses every connection.
    //
    // This is the check that found the real bug in rust-plain, which set a flag
    // in its signal handler and left accept() blocked forever.
    for lang in ALL {
        let Some(mut b) = Backend::start(lang) else {
            continue;
        };
        let sock = b.sock.clone();
        assert!(sock.exists(), "{}: socket should exist", lang.dir());

        unsafe {
            // SIGTERM, the signal systemd sends on stop.
            libc_kill(b.child.id() as i32, 15);
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(s) = b.child.try_wait().expect("try_wait") {
                break s;
            }
            assert!(
                Instant::now() < deadline,
                "{}: still running 10s after SIGTERM. It must stop accepting, \
                 finish what is in flight, remove its socket and exit 0 \
                 (spec §8.2).",
                lang.dir()
            );
            std::thread::sleep(Duration::from_millis(25));
        };

        assert_eq!(
            status.code(),
            Some(0),
            "{}: exit code must be 0 for a clean shutdown (spec §8.3)",
            lang.dir()
        );
        assert!(
            !sock.exists(),
            "{}: socket {} still exists after shutdown. The proxy would keep \
             this member in the pool and every connection to it would be \
             refused (spec §8.2).",
            lang.dir(),
            sock.display()
        );
    }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

#[cfg(test)]
mod build_dir_is_usable {
    use super::build_root_dir;

    /// The directory the compiled examples go in must permit exec.
    ///
    /// This test exists because nothing caught the day it stopped. The build
    /// host was hardened on 2026-09-22 and the baseline mounts `/tmp` noexec
    /// (site #98), where the compiled backends then lived. Fourteen tests
    /// failed with
    ///
    ///     cannot spawn c example: Permission denied (os error 13)
    ///
    /// which names the example and not the mount, so the first reading is that
    /// the C example is broken. It was not; the filesystem had changed under
    /// it.
    ///
    /// Asserted by ACTUALLY EXECUTING something rather than by reading mount
    /// options: `findmnt` returned nothing at all on that box for these paths,
    /// and a check that cannot see is a check that lies. This one runs a file
    /// or fails.
    #[test]
    fn the_build_directory_permits_exec() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = build_root_dir();
        let probe = dir.join(format!("m6bx-execprobe-{}", std::process::id()));

        let mut f = match std::fs::File::create(&probe) {
            Ok(f) => f,
            Err(e) => panic!("cannot write to the build directory {}: {e}", dir.display()),
        };
        f.write_all(b"#!/bin/sh\nexit 7\n").expect("write probe");
        drop(f);
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let result = std::process::Command::new(&probe).status();
        let _ = std::fs::remove_file(&probe);

        let status = result.unwrap_or_else(|e| {
            panic!(
                "{} does not permit exec: {e}\n\
                 The backend example binaries are built there and must be\n\
                 spawnable. A noexec mount here fails as \"cannot spawn <lang>\n\
                 example: Permission denied\", which reads as a broken example\n\
                 rather than as a mount option. See exec_scratch_root.",
                dir.display()
            )
        });
        assert_eq!(
            status.code(),
            Some(7),
            "the probe ran but did not report its own exit code"
        );
    }
}

#[cfg(test)]
mod scratch_sweeping {
    use super::sweep_stale_build_dirs_in;
    use std::time::Duration;

    /// Backdate a directory's modification time so the sweep sees it as old.
    fn backdate(p: &std::path::Path, secs: u64) {
        let when = std::time::SystemTime::now() - Duration::from_secs(secs);
        let _ = filetime::set_file_mtime(p, filetime::FileTime::from_system_time(when));
    }

    #[test]
    fn a_stale_build_dir_is_removed_and_a_fresh_one_is_not() {
        let root = tempfile::tempdir().expect("tempdir");
        let old = root.path().join("m6bx-build-999999");
        let new = root.path().join("m6bx-build-999998");
        std::fs::create_dir_all(old.join("gocache")).unwrap();
        std::fs::write(old.join("go"), b"binary").unwrap();
        std::fs::create_dir_all(&new).unwrap();
        backdate(&old, 60 * 60 * 24);

        sweep_stale_build_dirs_in(root.path(), Duration::from_secs(6 * 60 * 60));

        assert!(!old.exists(), "a day-old scratch directory must be swept");
        assert!(new.exists(), "a fresh one must be left alone");
    }

    #[test]
    fn this_processes_own_directories_are_never_swept() {
        // The guard that matters. Both shapes carry the pid, and a run long
        // enough to pass the threshold must not delete its own binaries out
        // from under itself.
        let root = tempfile::tempdir().expect("tempdir");
        let pid = std::process::id();
        let build = root.path().join(format!("m6bx-build-{pid}"));
        let scratch = root.path().join(format!("m6bx-{pid}-go-0"));
        std::fs::create_dir_all(&build).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        backdate(&build, 60 * 60 * 24 * 7);
        backdate(&scratch, 60 * 60 * 24 * 7);

        sweep_stale_build_dirs_in(root.path(), Duration::from_secs(6 * 60 * 60));

        assert!(build.exists(), "our own build dir must survive any age");
        assert!(scratch.exists(), "our own scratch dir must survive any age");
    }

    #[test]
    fn unrelated_directories_are_left_alone() {
        let root = tempfile::tempdir().expect("tempdir");
        let other = root.path().join("somebody-elses-data");
        std::fs::create_dir_all(&other).unwrap();
        backdate(&other, 60 * 60 * 24 * 30);

        sweep_stale_build_dirs_in(root.path(), Duration::from_secs(6 * 60 * 60));

        assert!(other.exists(), "the sweep must only touch its own names");
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        // Best effort: a test that cannot tidy up must not fail the run.
        sweep_stale_build_dirs_in(
            std::path::Path::new("/nonexistent-dir-for-this-test"),
            Duration::from_secs(1),
        );
    }
}
