//! The admin dashboard service: one process and one event loop.
//!
//! The loop accepts on the listener's readability, reads and answers each
//! connection until its socket would block, and waits for the next readiness
//! event. Each connection slot keeps its own buffers, made at start.
//!
//! A bench runs as one child process. Its stdout is a pipe registered on its
//! own token, so the loop reads the bench's output without holding a thread.
//! Its exit is a second descriptor (`ChildExit`), readable when the child
//! exits, so the loop reaps the child there without waiting.

#![deny(unsafe_code)]

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bench::JobStore;
use genid_publisher::Publisher;
use m6_core::client::Upstream;
use m6_core::platform::process::raise_fd_limit;
use m6_core::platform::ChildExit;
use m6_core::prelude::*;
use m6_core::signal::{Action, Lifecycle};
use m6_core::site::finish_reload;
use m6_core::{genid_failed, genid_ok};
use ops::ServiceConfig;
use render_admin::{bench, logs, ops, perf, system};
use serde_json::{json, Value};

const NAME: &str = env!("CARGO_PKG_NAME");

const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The running bench's output pipe, when there is one.
const BENCH: Token = Token(usize::MAX - 2);
/// The running bench's exit descriptor, readable when the child exits.
const BENCH_EXIT: Token = Token(usize::MAX - 3);
/// The log's stdout, registered only while it holds bytes a full pipe refused.
const LOG: Token = Token(usize::MAX - 4);
/// The accounting tick, and the longest the loop waits when nothing happens.
const TICK: Duration = Duration::from_secs(1);
/// The bench read buffer, made at start and reused for every readable turn.
const BENCH_READ_CAP: usize = 8 * 1024;

/// The routes this service answers in code, in the order the match reads them.
const ROUTES: &[CodeRoute] = &[
    // Performance tab: last N "periodic stats" entries from m6-http log
    CodeRoute {
        pattern: "/api/admin/perf",
        method: RouteMethod::Get,
    },
    // Routes tab: per-route request counts, cache hit rates, avg latency
    CodeRoute {
        pattern: "/api/admin/routes",
        method: RouteMethod::Get,
    },
    // System tab: CPU, RAM, disk, uptime
    CodeRoute {
        pattern: "/api/admin/system",
        method: RouteMethod::Get,
    },
    // Bench tab: list targets
    CodeRoute {
        pattern: "/api/admin/bench",
        method: RouteMethod::Get,
    },
    // Bench tab: start a job -> { job_id }
    CodeRoute {
        pattern: "/api/admin/bench",
        method: RouteMethod::Post,
    },
    // Bench tab: poll a job by id
    CodeRoute {
        pattern: "/api/admin/bench/{id}",
        method: RouteMethod::Get,
    },
    // Logs tab: last N log lines
    CodeRoute {
        pattern: "/api/admin/logs",
        method: RouteMethod::Get,
    },
    // Config: read site.toml
    CodeRoute {
        pattern: "/api/admin/config",
        method: RouteMethod::Get,
    },
    // Config: write site.toml (validates TOML first)
    CodeRoute {
        pattern: "/api/admin/config",
        method: RouteMethod::Put,
    },
    // Publish: POST /genid to m6-http, which reloads every backend and flushes
    // its cache
    CodeRoute {
        pattern: "/api/admin/publish",
        method: RouteMethod::Post,
    },
    // Cache flush: another name for publish
    CodeRoute {
        pattern: "/api/admin/cache/flush",
        method: RouteMethod::Post,
    },
    // Services: restart by name via SIGTERM
    CodeRoute {
        pattern: "/api/admin/services/{name}/restart",
        method: RouteMethod::Post,
    },
    // Backends: topology summary + per-backend sample/alltime detail
    CodeRoute {
        pattern: "/api/admin/backends",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/api/admin/backends/{name}/sample",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/api/admin/backends/{name}/alltime",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/api/admin/m6http/sample",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/api/admin/m6http/alltime",
        method: RouteMethod::Get,
    },
];

// ── Global state ──────────────────────────────────────────────────────────────

pub struct Global {
    /// m6-http JSON-lines log file to read perf/log data from.
    log_file: PathBuf,
    /// Number of "request complete" log entries to sample for route stats.
    routes_sample: usize,
    /// site.toml for the managed site (for config read/write/touch and the topology view).
    site_toml: PathBuf,
    /// System config m6-http starts with; the source of the bind address.
    system_toml: PathBuf,
    /// Number of perf history entries to return.
    perf_history: usize,
    /// Number of log lines to return by default.
    log_tail_default: usize,
    /// Path to the m6-bench binary.
    bench_bin: PathBuf,
    /// Default bench duration in seconds.
    bench_duration: u64,
    /// Default bench concurrency.
    bench_concurrency: u64,
    /// Named services that can be restarted (each has a PID file).
    services: Vec<ServiceConfig>,
    /// In-memory bench job store.
    jobs: JobStore,
    /// The bench running now, if any. One at a time.
    bench: Option<bench::Running>,
    /// Where `POST /genid` goes, and what it carries.
    publisher: Publisher,
}

fn load_state(site: &Site) -> m6_core::Result<Global> {
    let cfg = site.user_config();
    let get = |key: &str, default: &str| -> String {
        cfg.get(key)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    let get_usize = |key: &str, default: usize| -> usize {
        cfg.get(key)
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(default)
    };
    let get_u64 = |key: &str, default: u64| -> u64 {
        cfg.get(key).and_then(Value::as_u64).unwrap_or(default)
    };

    // Resolve paths relative to the site_dir (first CLI arg) when they are relative.
    let site_dir = site.site_dir();
    let resolve = |raw: String| -> PathBuf {
        let p = PathBuf::from(&raw);
        if p.is_absolute() {
            p
        } else {
            site_dir.join(p)
        }
    };

    let log_file = PathBuf::from(get("log_file", "/tmp/m6/m6-http.log"));
    let system_toml = resolve(get("system_toml", "configs/system-dev.toml"));
    let bench_bin = PathBuf::from(get("bench_bin", "m6-bench"));

    // A bench reads its child's exit from a descriptor. A Linux kernel can lack
    // pidfd_open (before 5.3), so probe once here and fail at start rather than
    // on a request.
    ChildExit::supported().map_err(|e| Error::Other(anyhow::anyhow!("bench: {e}")))?;

    // Parse services: array of { name, pid_file } objects.
    let services: Vec<ServiceConfig> = cfg
        .get("services")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let name = v.get("name")?.as_str()?.to_string();
                    let pid_file = v.get("pid_file")?.as_str()?.to_string();
                    Some(ServiceConfig { name, pid_file })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Global {
        log_file,
        routes_sample: get_usize("routes_sample", 10_000),
        site_toml: site.site_toml().to_path_buf(),
        system_toml,
        perf_history: get_usize("perf_history", 60),
        log_tail_default: get_usize("log_tail_default", 100),
        bench_bin,
        bench_duration: get_u64("bench_duration_secs", 10),
        bench_concurrency: get_u64("bench_concurrency", 50),
        services,
        jobs: bench::new_job_store(),
        bench: None,
        publisher: Publisher::load(cfg).map_err(|e| Error::Other(anyhow::anyhow!(e)))?,
    })
}

// ── Handlers ──────────────────────────────────────────────────────────────────

fn handle_perf(req: &Request, g: &Global) -> m6_core::Result<Response> {
    let n = req
        .dict()
        .get("n")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(g.perf_history);
    Ok(Response::json(perf::perf_blob(&g.log_file, n)))
}

fn handle_routes(_req: &Request, g: &Global) -> m6_core::Result<Response> {
    let n = _req
        .dict()
        .get("n")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(g.routes_sample);
    Ok(Response::json(perf::routes_blob(&g.log_file, n)))
}

fn handle_system(_req: &Request, _g: &Global) -> m6_core::Result<Response> {
    Ok(Response::json(system::system_blob()))
}

fn handle_bench_targets(_req: &Request, g: &Global) -> m6_core::Result<Response> {
    Ok(Response::json(bench::targets_blob(&g.system_toml)))
}

fn handle_bench_start(req: &Request, g: &mut Global, poll: &Poll) -> m6_core::Result<Response> {
    let body: Value = req
        .body_json()
        .map_err(|_| Error::BadRequest("invalid JSON body".into()))?;

    let addr = body
        .get("addr")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::BadRequest("missing 'addr'".into()))?
        .to_string();

    let proto = body
        .get("proto")
        .and_then(Value::as_str)
        .unwrap_or("all")
        .to_string();

    let duration = body
        .get("duration_secs")
        .and_then(Value::as_u64)
        .unwrap_or(g.bench_duration);

    let concurrency = body
        .get("concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(g.bench_concurrency);

    // One bench at a time: the loop has one pipe token for it.
    if g.bench.is_some() {
        return Ok(Response::json_status(
            json!({ "error": "a bench is already running" }),
            409,
        ));
    }

    let (id, run) = bench::start_bench(
        &g.bench_bin,
        &mut g.jobs,
        &addr,
        &proto,
        duration,
        concurrency,
    );
    if let Some(run) = run {
        poll.registry()
            .register_fd(run.pipe_fd(), BENCH, Interest::READABLE)
            .map_err(|e| Error::Other(e.into()))?;
        poll.registry()
            .register_fd(run.exit_fd(), BENCH_EXIT, Interest::READABLE)
            .map_err(|e| Error::Other(e.into()))?;
        g.bench = Some(run);
    }
    Ok(Response::json(json!({ "job_id": id })))
}

fn handle_bench_poll(req: &Request, g: &Global) -> m6_core::Result<Response> {
    let id = req["id"].as_str().unwrap_or("");
    match bench::poll_job(&g.jobs, id) {
        Some(blob) => Ok(Response::json(blob)),
        None => Err(Error::NotFound),
    }
}

fn handle_logs(req: &Request, g: &Global) -> m6_core::Result<Response> {
    let n = req
        .dict()
        .get("n")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(g.log_tail_default);
    let offset = req
        .dict()
        .get("offset")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u64>().ok());
    Ok(Response::json(logs::logs_blob(&g.log_file, n, offset)))
}

fn handle_config_read(_req: &Request, g: &Global) -> m6_core::Result<Response> {
    Ok(Response::json(ops::config_read(&g.site_toml)))
}

fn handle_config_write(req: &Request, g: &Global) -> m6_core::Result<Response> {
    let body: Value = req
        .body_json()
        .map_err(|_| Error::BadRequest("invalid JSON body".into()))?;

    // Body must be a JSON object — the full config tree to write back as TOML.
    if !body.is_object() {
        return Err(Error::BadRequest("body must be a JSON object".into()));
    }

    ops::config_write(&g.site_toml, &body).map_err(Error::BadRequest)?;

    Ok(Response::json(json!({ "ok": true })))
}

/// Publish: the loop sends `POST /genid` to m6-http, which reloads every backend
/// and flushes its cache, and answers once that is done.
fn handle_publish(_req: &Request, _g: &Global) -> m6_core::Result<Answer> {
    Ok(Answer::AfterPublish(Response::json(json!({ "ok": true }))))
}

fn handle_restart(req: &Request, g: &Global) -> m6_core::Result<Response> {
    let name = req["name"].as_str().unwrap_or("");
    let svc = g
        .services
        .iter()
        .find(|s| s.name == name)
        .ok_or(Error::NotFound)?;
    Ok(Response::json(ops::restart_service(svc)))
}

fn handle_backends_summary(_req: &Request, g: &Global) -> m6_core::Result<Response> {
    Ok(Response::json(perf::backends_summary_blob(
        &g.log_file,
        &g.site_toml,
        &g.system_toml,
        g.routes_sample,
    )))
}

fn handle_backend_sample(req: &Request, g: &Global) -> m6_core::Result<Response> {
    let name = req["name"].as_str().unwrap_or("");
    let n = req
        .dict()
        .get("n")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(g.routes_sample);
    Ok(Response::json(perf::backend_stats_blob(
        &g.log_file,
        &g.site_toml,
        name,
        n,
    )))
}

fn handle_backend_alltime(req: &Request, g: &Global) -> m6_core::Result<Response> {
    let name = req["name"].as_str().unwrap_or("");
    Ok(Response::json(perf::backend_stats_blob(
        &g.log_file,
        &g.site_toml,
        name,
        usize::MAX,
    )))
}

fn handle_m6http_sample(req: &Request, g: &Global) -> m6_core::Result<Response> {
    let n = req
        .dict()
        .get("n")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(g.routes_sample);
    Ok(Response::json(perf::m6http_stats_blob(&g.log_file, n)))
}

fn handle_m6http_alltime(_req: &Request, g: &Global) -> m6_core::Result<Response> {
    Ok(Response::json(perf::m6http_stats_blob(
        &g.log_file,
        usize::MAX,
    )))
}

// ── the service's own event loop ─────────────────────────────────────────────
//
// m6-core supplies the parts (the reactor, the signal descriptor, the
// connection slots, the read deadlines, routing, the shutdown lines). The
// service writes the loop that ties them together, so nothing here is hidden
// in a framework. docs/m6-app-anatomy.md in m6 walks through it step by step.
//
// start: blocking and allocation are fine. run: nothing blocks. stop: blocking
// is fine again.
//
// A publish is the one request that waits on something else. Its answer is
// built and held while a `POST /genid` runs on the connection slot's own
// `Upstream`, and goes out when m6-http has answered, so a reader that sees
// "published" can fetch the page.

/// What a handler asks the loop to do with its answer.
enum Answer {
    /// Send it now.
    Now(Response),
    /// The site's files changed: hold the answer, advance the site's
    /// generation with `POST /genid`, then send it.
    AfterPublish(Response),
}

/// What one client's turn asks of the loop.
enum Step {
    /// Nothing more to do now.
    Keep,
    /// Close the connection.
    Close,
    /// A `POST /genid` was started: watch its socket and start its deadline.
    StartOutgoing,
}

/// One connection slot: the client's connection, its publish slot, and the
/// answer held while the publish is in flight.
struct Conn {
    conn: ServerConn,
    up: Upstream,
    held: Option<Response>,
    /// The publish socket is registered with the poll.
    registered: bool,
    /// What the client's socket and the publish socket are registered for, so
    /// only a change costs a reregister.
    interest: Interest,
    up_interest: Interest,
}

impl Conn {
    fn publishing(&self) -> bool {
        self.held.is_some()
    }
}

/// A client's token is twice its slot's number, and the slot's publish
/// connection is the odd token after it.
fn main() -> io::Result<()> {
    // Block the termination signals before anything can start a thread.
    m6_core::signal::block();

    // ── start ────────────────────────────────────────────────────────────
    let mut site = Site::load(NAME, ROUTES, &[], &TeraFactory);
    // State is a plain value in `main`: one thread owns it, so it needs no lock.
    // A start that cannot build it prints why and exits 2.
    let mut state = load_state(&site).unwrap_or_else(|e| fail("loading the dashboard state", e));
    let read_timeout = site.read_timeout();
    let ceiling = raise_fd_limit().unwrap_or_else(|e| fail("raising the open file limit", e));
    let table = site.limits().get(&m6_core::limits::CONNECTIONS);
    let ceiling = ceiling.min(table.max());
    let initial = table.start().min(ceiling);

    let mut poll = Poll::new().unwrap_or_else(|e| fail("creating the poll", e));
    // The signal descriptor opens before the socket does: the socket is how a
    // caller knows the service is ready, and a stop sent right after must find
    // the handler armed.
    let mut signals = Signals::new().unwrap_or_else(|e| fail("opening the signal descriptor", e));
    let mut listener = listen_unix(site.socket_path(), site.socket_mode())
        .unwrap_or_else(|e| fail(&format!("binding {}", site.socket_path().display()), e));
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)?;
    poll.registry()
        .register_fd(signals.raw_fd(), SIGNALS, Interest::READABLE)?;

    let mut lifecycle = Lifecycle::new(site.name(), Some(site.socket_path().to_path_buf()));
    lifecycle.started();

    // The connection table and its buffers follow the `[limits]` keys.
    let read_limit = site.limits().get(&m6_core::limits::REQUEST_BODY).clone();
    let write_limit = site.limits().get(&m6_core::limits::RESPONSE_BUFFER).clone();
    let upstream_read = site.limits().get(&m6_core::limits::UPSTREAM_READ).clone();
    let upstream_request = site
        .limits()
        .get(&m6_core::limits::UPSTREAM_REQUEST)
        .clone();
    let mut conns = Slots::new(initial, ceiling, move || Conn {
        conn: ServerConn::with_limits(&read_limit, &write_limit),
        up: Upstream::with_limits(&upstream_read, &upstream_request),
        held: None,
        registered: false,
        interest: Interest::READABLE,
        up_interest: Interest::READABLE,
    });
    // `read_timeout_s = 0` means no deadline, which is the zero duration.
    let mut deadlines = Timeouts::new(initial, read_timeout.unwrap_or(Duration::ZERO));
    // The deadline of a publish in flight, apart from the client's read deadline.
    let mut outgoing = Timeouts::new(initial, state.publisher.timeout());
    let mut events = Events::with_capacity(256);
    let mut bench_buf = vec![0u8; BENCH_READ_CAP];
    let mut accepting = true;
    let mut last_tick = m6_core::platform::now();

    // ── run ──────────────────────────────────────────────────────────────
    'run: loop {
        let wait = deadlines
            .timeout(m6_core::platform::now(), TICK)
            .min(outgoing.timeout(m6_core::platform::now(), TICK));
        // An interrupted wait is not an error: wait again.
        match poll.poll(&mut events, Some(wait)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        let now = m6_core::platform::now();
        for ev in &events {
            match ev.token() {
                // New connections: a free slot for each, registered, with a
                // read deadline.
                LISTENER => {
                    while let Some(taken) = conns.take() {
                        if let Some(new_len) = taken.grew_to {
                            deadlines.grow(new_len);
                            outgoing.grow(new_len);
                        }
                        let mut stream = match listener.accept() {
                            Ok((stream, _)) => stream,
                            Err(e) => {
                                conns.free(taken.key);
                                match m6_core::platform::net::accept_error(&e) {
                                    m6_core::platform::net::Accept::Drained => break,
                                    m6_core::platform::net::Accept::SkipOne => continue,
                                    m6_core::platform::net::Accept::Stop => {
                                        eprintln!("{NAME}: accept error: {e}");
                                        break;
                                    }
                                }
                            }
                        };
                        poll.registry().register(
                            &mut stream,
                            Token(2 * taken.key),
                            Interest::READABLE,
                        )?;
                        if read_timeout.is_some() {
                            deadlines.touch(Token(taken.key), now);
                        }
                        let conn = conns.get_mut(taken.key);
                        conn.conn.attach(stream);
                        conn.interest = Interest::READABLE;
                    }
                    // At the ceiling, stop accepting: new clients wait in the
                    // kernel's backlog.
                    if conns.is_full() {
                        poll.registry().deregister(&mut listener)?;
                        accepting = false;
                    }
                }
                SIGNALS => {
                    let mut stop = false;
                    while let Some(signal) = signals.read() {
                        if lifecycle.on_signal(signal) == Action::Stop {
                            stop = true;
                        }
                    }
                    if stop {
                        break 'run;
                    }
                }
                LOG => m6_core::log::flush(),
                // The bench's output: read what it has written. The exit event
                // reaps the child, and the job is done when both have happened.
                BENCH => {
                    bench::bench_event(
                        &mut state.bench,
                        &mut state.jobs,
                        &poll,
                        bench::BenchEvent::Pipe(&mut bench_buf),
                    )?;
                }
                // The bench child's exit descriptor: readable once the child has
                // exited. Reap it. The job is done when its pipe has also
                // reached end of file.
                BENCH_EXIT => {
                    bench::bench_event(
                        &mut state.bench,
                        &mut state.jobs,
                        &poll,
                        bench::BenchEvent::Exit,
                    )?;
                }
                // A publish connection: one step of its exchange with m6-http.
                Token(t) if t % 2 == 1 => {
                    publish_event(&mut poll, &mut conns, &mut outgoing, t / 2)?;
                }
                // A client: its turn, then wait for what it still needs, push
                // its deadline back, or close it.
                Token(t) => {
                    let k = t / 2;
                    match client_turn(conns.get_mut(k), &mut site, &mut state, &poll, now)? {
                        Step::Close => free(&poll, &mut conns, &mut deadlines, &mut outgoing, k)?,
                        step => {
                            let conn = conns.get_mut(k);
                            if let Step::StartOutgoing = step {
                                let interest = conn.up.interest();
                                poll.registry().register_fd(
                                    conn.up.fd(),
                                    Token(2 * k + 1),
                                    interest,
                                )?;
                                conn.up_interest = interest;
                                conn.registered = true;
                                outgoing.touch(Token(k), now);
                            }
                            refresh_client(&poll, conn, k)?;
                            if read_timeout.is_some() {
                                deadlines.touch(Token(k), now);
                            }
                        }
                    }
                }
            }
        }

        // A full stdout holds its bytes. Watch it for writability only while
        // it does, so the loop never waits on the log.
        if let Err(e) = m6_core::log::watch(poll.registry(), LOG) {
            m6_core::log::error!(error = %e, "log descriptor registration failed");
        }
        // A publish m6-http did not answer in time: 504, and drop the exchange.
        while let Some(Token(k)) = outgoing.expired(m6_core::platform::now()) {
            let conn = conns.get_mut(k);
            if conn.publishing() {
                end_publish(&poll, conn)?;
                conn.conn
                    .respond(504, &[], b"m6-http took too long to publish");
                let _ = conn.conn.flush()?;
                refresh_client(&poll, conn, k)?;
            }
        }
        // A client that sent nothing before its deadline is closed unanswered,
        // and its publish, if any, is dropped with it.
        while let Some(Token(k)) = deadlines.expired(m6_core::platform::now()) {
            if !conns.get_mut(k).publishing() {
                free(&poll, &mut conns, &mut deadlines, &mut outgoing, k)?;
            }
        }
        // A slot came free while the table was full: accept again.
        if !accepting && !conns.is_full() {
            poll.registry()
                .register(&mut listener, LISTENER, Interest::READABLE)?;
            accepting = true;
        }
        if m6_core::platform::now() - last_tick >= TICK {
            m6_core::log::tick(m6_core::platform::wall());
            last_tick = m6_core::platform::now();
        }
    }

    // ── stop ─────────────────────────────────────────────────────────────
    // Connections still open are dropped: a stop does not wait for a request
    // in flight. `complete` removes the socket and logs the line.
    lifecycle.complete();
    m6_core::log::stop()?;
    Ok(())
}

/// One client's turn: write what it owes, read, dispatch, write.
fn client_turn(
    conn: &mut Conn,
    site: &mut Site,
    state: &mut Global,
    poll: &Poll,
    now: Instant,
) -> io::Result<Step> {
    // While a publish is in flight the client's token stays readable, so a
    // client that leaves is noticed and its exchange dropped.
    if conn.publishing() {
        let mut heads = head_slots();
        return match conn.conn.read(&mut heads)? {
            Read::Closed => Ok(Step::Close),
            _ => Ok(Step::Keep),
        };
    }
    loop {
        // Write what is owed before reading anything.
        if conn.conn.interest() == Interest::WRITABLE {
            match conn.conn.flush()? {
                Flush::Done => {}
                Flush::More => return Ok(Step::Keep),
                Flush::Close => return Ok(Step::Close),
            }
        }
        let mut heads = head_slots();
        let (response, idle) = match conn.conn.read(&mut heads)? {
            Read::Request(head) => {
                let response = match site.handle(&head) {
                    // A page, a 404 or an error, already finished.
                    Handled::Done(response) => response,
                    // One of ROUTES: run its handler, then `finish` adds the
                    // site-wide parts (templates, Cache-Control, CSRF, flash).
                    Handled::Code(index, req) => match route(index, &req, state, poll) {
                        Ok(Answer::Now(response)) => site.finish(&req, response),
                        Ok(Answer::AfterPublish(response)) => {
                            let response = site.finish(&req, response);
                            match state.publisher.start(&mut conn.up, now) {
                                Ok(()) => {
                                    conn.held = Some(response);
                                    return Ok(Step::StartOutgoing);
                                }
                                Err(e) => not_published(&e.to_string()),
                            }
                        }
                        Err(e) => site.finish(&req, m6_core::response::error_to_response(&e)),
                    },
                    // `POST /genid` on this service: read config, templates and
                    // params again through the start path, and keep the old
                    // site if that fails. m6-http sends it to every backend
                    // when the site's generation advances.
                    Handled::Reload(reload) => {
                        let response = match site.reload(ROUTES, &[], &TeraFactory) {
                            Ok(next) => {
                                *site = next;
                                genid_ok()
                            }
                            Err(e) => genid_failed(&e),
                        };
                        finish_reload(&reload, response)
                    }
                    Handled::Named(..) => unreachable!("{NAME} registers no named handlers"),
                };
                (Some(response), false)
            }
            Read::More => (None, true),
            Read::Closed => return Ok(Step::Close),
        };
        if let Some(response) = response {
            conn.conn.respond_response(response);
        }
        match conn.conn.flush()? {
            // Answered and kept alive: a pipelined request may already be
            // buffered, so read again before waiting.
            Flush::Done if !idle => {}
            Flush::Done | Flush::More => return Ok(Step::Keep),
            Flush::Close => return Ok(Step::Close),
        }
    }
}

/// The answer when the files changed but the site's generation did not.
fn not_published(cause: &str) -> Response {
    Response::json_status(
        json!({"error": format!("the change was saved but not published: {cause}")}),
        502,
    )
}

/// One step of a slot's `POST /genid`. m6-http answering 200 sends the held
/// answer, and any other result answers 502 with the cause.
fn publish_event(
    poll: &mut Poll,
    conns: &mut Slots<Conn>,
    outgoing: &mut Timeouts,
    k: usize,
) -> io::Result<()> {
    let conn = conns.get_mut(k);
    // The client can leave in the same poll batch, so an event for a slot with
    // no publish in flight is stale and ignored.
    if !conn.publishing() {
        return Ok(());
    }
    match conn.up.drive() {
        Ok(false) => {
            let interest = conn.up.interest();
            if interest != conn.up_interest {
                poll.registry()
                    .reregister_fd(conn.up.fd(), Token(2 * k + 1), interest)?;
                conn.up_interest = interest;
            }
            return Ok(());
        }
        Ok(true) => {
            let held = conn
                .held
                .take()
                .expect("a publish in flight holds its answer");
            let answer = match genid_publisher::outcome(&conn.up) {
                Ok(()) => held,
                Err(cause) => not_published(&cause),
            };
            conn.conn.respond_response(answer);
        }
        Err(e) => {
            conn.held = None;
            conn.conn.respond_response(not_published(&e.to_string()));
        }
    }
    outgoing.clear(Token(k));
    end_publish(poll, conn)?;
    let _ = conn.conn.flush()?;
    refresh_client(poll, conn, k)
}

/// Drop a publish's socket and make the slot's connection free again.
fn end_publish(poll: &Poll, conn: &mut Conn) -> io::Result<()> {
    if conn.registered {
        poll.registry().deregister_fd(conn.up.fd())?;
        conn.registered = false;
    }
    conn.up.close();
    conn.held = None;
    Ok(())
}

/// Register the client for what its connection now waits for.
fn refresh_client(poll: &Poll, conn: &mut Conn, k: usize) -> io::Result<()> {
    let interest = conn.conn.interest();
    if interest != conn.interest {
        let stream = conn
            .conn
            .stream()
            .expect("every registered token has an attached connection");
        poll.registry().reregister(stream, Token(2 * k), interest)?;
        conn.interest = interest;
    }
    Ok(())
}

/// Close a client: forget both deadlines, drop its publish, deregister it and
/// free the slot.
fn free(
    poll: &Poll,
    conns: &mut Slots<Conn>,
    deadlines: &mut Timeouts,
    outgoing: &mut Timeouts,
    k: usize,
) -> io::Result<()> {
    deadlines.clear(Token(k));
    outgoing.clear(Token(k));
    let conn = conns.get_mut(k);
    end_publish(poll, conn)?;
    if let Some(mut stream) = conn.conn.detach() {
        poll.registry().deregister(&mut stream)?;
        conns.free(k);
    }
    Ok(())
}

/// The body of one route, by its index in [`ROUTES`].
fn route(index: usize, req: &Request, state: &mut Global, poll: &Poll) -> m6_core::Result<Answer> {
    let now = |response: m6_core::Result<Response>| response.map(Answer::Now);
    match index {
        0 => now(handle_perf(req, state)),
        1 => now(handle_routes(req, state)),
        2 => now(handle_system(req, state)),
        3 => now(handle_bench_targets(req, state)),
        4 => now(handle_bench_start(req, state, poll)),
        5 => now(handle_bench_poll(req, state)),
        6 => now(handle_logs(req, state)),
        7 => now(handle_config_read(req, state)),
        8 => now(handle_config_write(req, state)),
        9 | 10 => handle_publish(req, state),
        11 => now(handle_restart(req, state)),
        12 => now(handle_backends_summary(req, state)),
        13 => now(handle_backend_sample(req, state)),
        14 => now(handle_backend_alltime(req, state)),
        15 => now(handle_m6http_sample(req, state)),
        16 => now(handle_m6http_alltime(req, state)),
        _ => unreachable!("the seventeen code routes are indexes 0 to 16"),
    }
}

/// A failure at start is an error: print it and exit 2.
fn fail(what: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("{NAME}: {what}: {error}");
    std::process::exit(2)
}
