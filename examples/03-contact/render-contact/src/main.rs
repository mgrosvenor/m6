//! The contact-form renderer: one page, one POST, and the mail it sends.
//!
//! The service owns its event loop, built from m6-core's parts (see
//! docs/m6-app-anatomy.md in m6). Sending mail is the part worth reading: the
//! SMTP exchange runs in the same loop as the HTTP connections, one `Smtp` slot
//! per connection slot, so a slow relay holds one client and never the loop.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use m6_core::client;
use m6_core::email::{lettre, Relay, RelayConfig, Smtp};
use m6_core::platform::process::raise_fd_limit;
use m6_core::prelude::*;
use m6_core::rustls::ClientConfig;
use m6_core::signal::{Action, Lifecycle};
use m6_core::site::finish_reload;
use m6_core::{genid_failed, genid_ok, Secret};

/// The name the service passes to `Site::load`. It is also the configuration file's
/// stem, the socket's name and the `[[backend]]` name in site.toml.
const NAME: &str = "render-contact";

/// The code routes. `GET /contact` is a template route in the configuration, so
/// only the POST is code. `Site::load` hands a match back as `Handled::Code(0, req)`.
const ROUTES: &[CodeRoute] = &[CodeRoute {
    pattern: "/contact",
    method: RouteMethod::Post,
}];

/// A client's token is twice its slot's number, and the slot's mail connection
/// is the odd token after it.
const LISTENER: Token = Token(usize::MAX);
const SIGNALS: Token = Token(usize::MAX - 1);
/// The log's stdout, registered only while it holds bytes a full pipe refused.
const LOG: Token = Token(usize::MAX - 2);
/// The longest the loop waits when nothing happens, and the log's flush tick.
const TICK: Duration = Duration::from_secs(1);
/// The longest one message may take, from connect to the relay's reply.
const SMTP_TIMEOUT: Duration = Duration::from_secs(20);
const SMTP_WRITE_CAP: usize = 16 * 1024;

/// Settings read once at start: the relay, resolved, and the two addresses.
struct Global {
    relay: Relay,
    from: String,
    to: String,
}

fn load_state(site: &Site) -> Result<Global> {
    let config = site.user_config();
    let host = config["smtp"]["host"]
        .as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.host missing")))?;
    let port = config["smtp"]["port"]
        .as_u64()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.port missing")))? as u16;
    let username = config["smtp"]["username"]
        .as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.username missing")))?;
    let password = config["smtp"]["password"]
        .as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.password missing")))?;
    let starttls = config["smtp"]["starttls"].as_bool().unwrap_or(true);
    let from = config["smtp"]["from"]
        .as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.from missing")))?
        .to_string();
    let to = config["smtp"]["to"]
        .as_str()
        .ok_or_else(|| Error::Other(anyhow::anyhow!("smtp.to missing")))?
        .to_string();

    // m6-core refuses plain SMTP to any host but localhost, 127.0.0.1 and ::1,
    // so a real relay needs STARTTLS and the system's trusted certificates.
    let tls = if starttls {
        // The provider is named, because a build that links two of them has no
        // process default for `ClientConfig::builder()` to pick.
        let provider = Arc::new(m6_core::rustls::crypto::aws_lc_rs::default_provider());
        Some(Arc::new(
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| Error::Other(anyhow::anyhow!("{e}")))?
                .with_root_certificates(
                    client::system_roots().map_err(|e| Error::Other(anyhow::anyhow!("{e}")))?,
                )
                .with_no_client_auth(),
        ))
    } else {
        None
    };
    let credentials =
        (!username.is_empty()).then(|| Secret::new((username.to_string(), password.to_string())));
    let relay = Relay::new(&RelayConfig {
        host: host.to_string(),
        port,
        starttls: tls,
        credentials,
        timeout: SMTP_TIMEOUT,
    })
    .map_err(|e| Error::Other(anyhow::anyhow!("{e}")))?;

    Ok(Global { relay, from, to })
}

fn message(global: &Global, name: &str, email: &str, text: &str) -> Result<Message> {
    let from_addr: lettre::Address = global
        .from
        .parse()
        .map_err(|e: lettre::address::AddressError| Error::Other(anyhow::anyhow!("{e}")))?;
    let to_addr: lettre::Address = global
        .to
        .parse()
        .map_err(|e: lettre::address::AddressError| Error::Other(anyhow::anyhow!("{e}")))?;

    Message::builder()
        .from(lettre::message::Mailbox::new(None, from_addr))
        .to(lettre::message::Mailbox::new(None, to_addr))
        .subject(format!("Contact from {}", name))
        .body(format!("From: {} <{}>\n\n{}", name, email, text))
        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")))
}

/// `POST /contact`: read the form, start sending the message, and return the
/// page to show once the relay has accepted it. The page is held, not sent: the
/// reader sees "sent" only after the mail was.
fn handle_post(req: &Request, global: &Global, smtp: &mut Smtp) -> Result<Response> {
    let name = req.field("name")?;
    let email = req.field("email")?;
    let text = req.field("message")?;

    let msg = message(global, &name, &email, &text)?;
    smtp.start(&global.relay, &msg)
        .map_err(|e| Error::Other(e.into()))?;

    Response::render_with(
        "templates/contact.html",
        req,
        json!({"sent": true, "name": name}),
    )
}

// ── the service's own event loop ─────────────────────────────────────────────
//
// m6-core supplies the parts (the reactor, the signal descriptor, the
// connection slots, the read deadlines, routing, the shutdown lines). The
// service writes the loop that ties them together, so nothing here is hidden
// in a framework.
//
// start: blocking and allocation are fine. run: nothing blocks. stop: blocking
// is fine again.

/// What one client's turn asks of the loop.
enum Step {
    /// Nothing more to do now.
    Keep,
    /// Close the connection.
    Close,
    /// A message was started: watch its socket and start its deadline.
    StartOutgoing,
}

/// One connection slot: the client's connection, its mail slot, and the page
/// held while the message is on its way.
struct Conn {
    conn: ServerConn,
    smtp: Smtp,
    held: Option<Response>,
    /// What the client's socket and the mail socket are registered for, so only
    /// a change costs a reregister.
    interest: Interest,
    smtp_interest: Interest,
}

impl Conn {
    fn sending(&self) -> bool {
        self.held.is_some()
    }
}

fn main() -> io::Result<()> {
    // Block the termination signals before anything can start a thread.
    m6_core::signal::block();

    // ── start ────────────────────────────────────────────────────────────
    let mut site = Site::load(NAME, ROUTES, &[], &TeraFactory);
    // State is a plain value in `main`: one thread owns it, so it needs no lock.
    // A start that cannot build it prints why and exits 2.
    let global = load_state(&site).unwrap_or_else(|e| fail("loading the configuration", e));
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
    let smtp_reply = site.limits().get(&m6_core::limits::SMTP_REPLY).clone();
    let mut conns = Slots::new(initial, ceiling, move || Conn {
        conn: ServerConn::with_limits(&read_limit, &write_limit),
        smtp: Smtp::with_limit(&smtp_reply, SMTP_WRITE_CAP),
        held: None,
        interest: Interest::READABLE,
        smtp_interest: Interest::READABLE,
    });
    // `read_timeout_s = 0` means no deadline, which is the zero duration.
    let mut deadlines = Timeouts::new(initial, read_timeout.unwrap_or(Duration::ZERO));
    // The deadline of a message in flight, apart from the client's read deadline.
    let mut outgoing = Timeouts::new(initial, SMTP_TIMEOUT);
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
                // A mail connection: one step of its SMTP exchange.
                Token(t) if t % 2 == 1 => {
                    mail_event(&mut poll, &mut conns, &mut outgoing, t / 2)?;
                }
                // A client: its turn, then wait for what it still needs, push
                // its deadline back, or close it.
                Token(t) => {
                    let k = t / 2;
                    match client_turn(conns.get_mut(k), &mut site, &global)? {
                        Step::Close => free(&poll, &mut conns, &mut deadlines, &mut outgoing, k)?,
                        step => {
                            let conn = conns.get_mut(k);
                            if let Step::StartOutgoing = step {
                                let interest = conn.smtp.interest();
                                if let Some(fd) = conn.smtp.fd() {
                                    poll.registry()
                                        .register_fd(fd, Token(2 * k + 1), interest)?;
                                }
                                conn.smtp_interest = interest;
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
        // A message the relay did not take in time: answer 504 and drop it.
        while let Some(Token(k)) = outgoing.expired(m6_core::platform::now()) {
            let conn = conns.get_mut(k);
            if conn.sending() {
                end_delivery(&poll, conn)?;
                conn.conn.respond(504, &[], b"the mail relay took too long");
                let _ = conn.conn.flush()?;
                refresh_client(&poll, conn, k)?;
            }
        }
        // A client that sent nothing before its deadline is closed unanswered,
        // and its message, if any, is dropped with it.
        while let Some(Token(k)) = deadlines.expired(m6_core::platform::now()) {
            if !conns.get_mut(k).sending() {
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
fn client_turn(conn: &mut Conn, site: &mut Site, global: &Global) -> io::Result<Step> {
    // While a message is on its way the client's token stays readable, so a
    // client that leaves is noticed and its delivery dropped.
    if conn.sending() {
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
                    // `POST /contact`: start the message, hold the page.
                    Handled::Code(0, req) => match handle_post(&req, global, &mut conn.smtp) {
                        Ok(page) => {
                            conn.held = Some(site.finish(&req, page));
                            return Ok(Step::StartOutgoing);
                        }
                        Err(e) => site.finish(&req, m6_core::response::error_to_response(&e)),
                    },
                    Handled::Code(..) => unreachable!("route is not in ROUTES"),
                    // `POST /genid`: read config, templates and params again
                    // through the start path, and keep the old site if that
                    // fails.
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

/// One step of a slot's SMTP exchange. The relay accepting the message sends
/// the held page, and a failure answers 500 with the cause.
fn mail_event(
    poll: &mut Poll,
    conns: &mut Slots<Conn>,
    outgoing: &mut Timeouts,
    k: usize,
) -> io::Result<()> {
    let conn = conns.get_mut(k);
    // The client can leave in the same poll batch, so an event for a slot with
    // no message in flight is stale and ignored.
    if !conn.sending() {
        return Ok(());
    }
    match conn.smtp.drive() {
        Ok(None) => {
            let interest = conn.smtp.interest();
            if interest != conn.smtp_interest {
                if let Some(fd) = conn.smtp.fd() {
                    poll.registry()
                        .reregister_fd(fd, Token(2 * k + 1), interest)?;
                }
                conn.smtp_interest = interest;
            }
            return Ok(());
        }
        Ok(Some(())) => {
            let page = conn
                .held
                .take()
                .expect("a message in flight holds its page");
            conn.conn.respond_response(page);
        }
        Err(e) => {
            conn.held = None;
            let error = Error::Other(e.into());
            conn.conn
                .respond_response(m6_core::response::error_to_response(&error));
        }
    }
    outgoing.clear(Token(k));
    end_delivery(poll, conn)?;
    let _ = conn.conn.flush()?;
    refresh_client(poll, conn, k)
}

/// Drop a delivery's socket and make the slot's mail connection free again.
fn end_delivery(poll: &Poll, conn: &mut Conn) -> io::Result<()> {
    if let Some(fd) = conn.smtp.fd() {
        poll.registry().deregister_fd(fd)?;
    }
    conn.smtp.close();
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

/// Close a client: forget both deadlines, drop its delivery, deregister it and
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
    end_delivery(poll, conn)?;
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
