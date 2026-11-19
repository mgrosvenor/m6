use m6_core::client::Upstream;
use m6_core::platform::process::raise_fd_limit;
use m6_core::prelude::*;
use m6_core::signal::{Action, Lifecycle};
use m6_core::site::finish_reload;
use m6_core::{genid_failed, genid_ok};
use serde_json::json;
use std::fs;
use std::io;
use std::time::{Duration, Instant};

use genid_publisher::Publisher;

/// The name the service passes to `Site::load`. It is also the configuration file's
/// stem, the socket's name and the `[[backend]]` name in site.toml.
const NAME: &str = "render-cms";

/// The code routes, in the order `route` below matches them. `Site::load` compiles
/// them at start and hands each request back as `Handled::Code(index, req)`.
const ROUTES: &[CodeRoute] = &[
    CodeRoute {
        pattern: "/cms",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/cms/new",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/cms/edit/{stem}",
        method: RouteMethod::Get,
    },
    CodeRoute {
        pattern: "/api/drafts",
        method: RouteMethod::Post,
    },
    CodeRoute {
        pattern: "/api/drafts/{stem}",
        method: RouteMethod::Patch,
    },
    CodeRoute {
        pattern: "/api/publish/{stem}",
        method: RouteMethod::Post,
    },
    CodeRoute {
        pattern: "/api/unpublish/{stem}",
        method: RouteMethod::Post,
    },
];

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
const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The log's stdout, registered only while it holds bytes a full pipe refused.
const LOG: Token = Token(usize::MAX - 2);
/// The longest the loop waits when nothing happens, and the log's flush tick.
const TICK: Duration = Duration::from_secs(1);

fn main() -> io::Result<()> {
    // Block the termination signals before anything can start a thread.
    m6_core::signal::block();

    // ── start ────────────────────────────────────────────────────────────
    let mut site = Site::load(NAME, ROUTES, &[], &TeraFactory);
    // State is a plain value in `main`: one thread owns it, so it needs no lock.
    // A start that cannot build it prints why and exits 2.
    let publisher = Publisher::load(site.user_config())
        .unwrap_or_else(|e| fail("loading the configuration", e));
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
    let mut outgoing = Timeouts::new(initial, publisher.timeout());
    let mut events = Events::with_capacity(256);
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
                // A publish connection: one step of its exchange with m6-http.
                Token(t) if t % 2 == 1 => {
                    publish_event(&mut poll, &mut conns, &mut outgoing, t / 2)?;
                }
                // A client: its turn, then wait for what it still needs, push
                // its deadline back, or close it.
                Token(t) => {
                    let k = t / 2;
                    match client_turn(conns.get_mut(k), &mut site, &publisher, now)? {
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
    publisher: &Publisher,
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
                    Handled::Code(index, req) => match route(index, &req) {
                        Ok(Answer::Now(response)) => site.finish(&req, response),
                        Ok(Answer::AfterPublish(response)) => {
                            let response = site.finish(&req, response);
                            match publisher.start(&mut conn.up, now) {
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

/// A failure at start is printed and exits 2, the code for a bad start.
fn fail(what: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("{NAME}: {what}: {error}");
    std::process::exit(2)
}

/// The handler for one route: the arm's number is the route's place in `ROUTES`.
fn route(index: usize, req: &Request) -> Result<Answer> {
    match index {
        0 => handle_dashboard(req).map(Answer::Now),
        1 => handle_new(req).map(Answer::Now),
        2 => handle_edit(req).map(Answer::Now),
        3 => handle_create_draft(req).map(Answer::Now),
        4 => handle_update_draft(req).map(Answer::Now),
        5 => handle_publish(req),
        6 => handle_unpublish(req),
        _ => unreachable!("route {index} is not in ROUTES"),
    }
}

fn handle_dashboard(req: &Request) -> Result<Response> {
    let author = req["auth_username"].as_str().unwrap_or("");
    // `unwrap_or_default`, not `?`: git does not track empty directories, so a fresh
    // clone has no content/drafts/ and no content/posts/, and `list_json` returns an
    // error for a directory that does not exist. No drafts and no drafts directory
    // are the same thing to a reader, so they render the same page.
    let drafts = req.list_json("content/drafts/").unwrap_or_default();
    let published = req.list_json("content/posts/").unwrap_or_default();
    Response::render_with(
        "templates/cms/dashboard.html",
        req,
        json!({
            "drafts":    drafts,
            "published": published,
            "author":    author,
        }),
    )
}

fn handle_new(req: &Request) -> Result<Response> {
    Response::render_with("templates/cms/editor.html", req, json!({"new": true}))
}

fn handle_edit(req: &Request) -> Result<Response> {
    let stem = req["stem"].as_str().ok_or(Error::NotFound)?;
    let post = req
        .read_json(&format!("content/drafts/{}.json", stem))
        .or_else(|_| req.read_json(&format!("content/posts/{}.json", stem)))?;
    Response::render_with("templates/cms/editor.html", req, post)
}

fn handle_create_draft(req: &Request) -> Result<Response> {
    let body: Value = req.body_json()?;
    let stem = slugify(body["title"].as_str().unwrap_or("untitled"));
    let draft = json!({
        "stem":   stem,
        "title":  body["title"],
        "body":   body["body"],
        "author": req["auth_username"],
        "date":   today_iso8601(),
    });
    req.write_json(&format!("content/drafts/{}.json", stem), &draft)?;
    Ok(Response::json_status(json!({"stem": stem}), 201))
}

fn handle_update_draft(req: &Request) -> Result<Response> {
    let stem = req["stem"].as_str().ok_or(Error::NotFound)?;
    let body: Value = req.body_json()?;
    let path = format!("content/drafts/{}.json", stem);
    let mut draft = req.read_json(&path)?;
    if let Some(t) = body.get("title") {
        draft["title"] = t.clone();
    }
    if let Some(b) = body.get("body") {
        draft["body"] = b.clone();
    }
    req.write_json(&path, &draft)?;
    Ok(Response::json(json!({"ok": true})))
}

fn handle_publish(req: &Request) -> Result<Answer> {
    let stem = req["stem"].as_str().ok_or(Error::NotFound)?;
    let draft_path = format!("content/drafts/{}.json", stem);
    let publish_path = format!("content/posts/{}.json", stem);

    let mut post = req.read_json(&draft_path)?;
    post["stem"] = json!(stem);
    post["path"] = json!(format!("/blog/{}", stem));
    post["published_at"] = json!(now_iso8601());

    req.write_json_atomic(&publish_path, &post)?;
    update_index(req)?;
    // The files are written. m6 services read them once, at start, and a touch of
    // site.toml reloads nothing, so the answer is `AfterPublish`: the loop sends
    // `POST /genid` to m6-http, which reloads every backend and flushes its
    // cache, and only then tells the editor it is published.
    let _ = fs::remove_file(req.site_path(&draft_path));

    Ok(Answer::AfterPublish(Response::json(
        json!({"published": true, "path": post["path"]}),
    )))
}

fn handle_unpublish(req: &Request) -> Result<Answer> {
    let stem = req["stem"].as_str().ok_or(Error::NotFound)?;
    let publish_path = format!("content/posts/{}.json", stem);
    let draft_path = format!("content/drafts/{}.json", stem);

    let mut post = req.read_json(&publish_path)?;
    post["draft"] = json!(true);
    req.write_json(&draft_path, &post)?;
    let _ = fs::remove_file(req.site_path(&publish_path));
    update_index(req)?;
    Ok(Answer::AfterPublish(Response::json(
        json!({"unpublished": true}),
    )))
}

/// Every stem this CMS owns, published or not, taken from the file names in both
/// content directories. A post's owner cannot be deduced from where it currently is:
/// an unpublished post has a draft file, so it is still owned, and its stale index
/// entry is dropped and not re-added.
fn cms_owned_stems(req: &Request) -> std::collections::HashSet<String> {
    let mut stems = std::collections::HashSet::new();
    for dir in ["content/posts/", "content/drafts/"] {
        let Ok(entries) = fs::read_dir(req.site_path(dir)) else {
            continue; // a directory that does not exist owns nothing
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    stems.insert(stem.to_string());
                }
            }
        }
    }
    stems
}

/// Rebuild `data/posts.json` from the CMS's published posts plus the entries m6-md
/// put there from markdown, so a publish does not drop the markdown posts.
fn update_index(req: &Request) -> Result<()> {
    let cms_posts = req.list_json("content/posts/").unwrap_or_default();
    let owned = cms_owned_stems(req);
    let existing = req
        .read_json("data/posts.json")
        .unwrap_or_else(|_| json!({"documents": []}));
    let md_posts: Vec<serde_json::Value> = existing["documents"]
        .as_array()
        .map(|docs| {
            docs.iter()
                .filter(|d| !owned.contains(d["stem"].as_str().unwrap_or("")))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut all: Vec<serde_json::Value> = md_posts.into_iter().chain(cms_posts).collect();
    all.sort_by(|a, b| {
        b["date"]
            .as_str()
            .unwrap_or("")
            .cmp(a["date"].as_str().unwrap_or(""))
    });
    req.write_json_atomic("data/posts.json", &json!({ "documents": all }))
}
