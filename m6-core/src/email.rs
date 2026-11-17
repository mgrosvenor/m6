//! An SMTP client a service's own event loop drives, without blocking.
//!
//! A [`Relay`] is made once at start: it checks the plain-SMTP rule, resolves
//! the host once with [`crate::net::resolve`] and reads the EHLO name. One
//! [`Smtp`] slot is made at start per outgoing table entry, with its buffers.
//! [`Smtp::start`] begins a non-blocking connect for one message, the service
//! registers [`Smtp::fd`] with [`Smtp::interest`] and calls [`Smtp::drive`]
//! each time it fires. `drive` reports `Ok(Some(()))` when the relay accepted
//! the message. A relay that takes too long is the service's deadline to
//! enforce: the slot does not read the clock.
//!
//! ```rust,no_run
//! use std::time::Duration;
//!
//! use m6_core::email::{Relay, RelayConfig, Smtp};
//!
//! # fn f() -> std::io::Result<()> {
//! let relay = Relay::new(&RelayConfig {
//!     host: "127.0.0.1".into(),
//!     port: 2525,
//!     starttls: None,
//!     credentials: None,
//!     timeout: Duration::from_secs(20),
//! })?;
//! let mut smtp = Smtp::new(4096, 8192);
//! let _ = (&relay, &mut smtp);
//! # Ok(())
//! # }
//! ```
//!
//! lettre builds the message and its envelope, formats the bytes with
//! `Message::formatted()`, and turns each step into its command: `Mail`,
//! `Rcpt`, `Data`, `Starttls`, `Auth`, `Ehlo` and `Quit`. It parses the
//! replies and supplies the extension and authentication types. This loop
//! owns the socket, the descriptor and the deadline.

use std::fmt::Display;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lettre::transport::smtp::authentication::{Credentials, Mechanism, DEFAULT_MECHANISMS};
use lettre::transport::smtp::commands::{Auth, Data, Ehlo, Mail, Quit, Rcpt, Starttls};
use lettre::transport::smtp::extension::{
    ClientId, Extension, MailBodyParameter, MailParameter, ServerInfo,
};
use lettre::transport::smtp::response::Response;
use rustls::ClientConnection;

/// Re-exported so a service builds its message with `m6_core::email::lettre`
/// and needs no `lettre` dependency of its own.
pub use lettre;

/// Where and how to send. Built by the service at start from its own config.
pub struct RelayConfig {
    pub host: String,
    pub port: u16,
    /// `Some`: STARTTLS is required and the relay's certificate is checked with
    /// this config. `None`: plain SMTP, accepted only when `host` is a loopback
    /// name.
    pub starttls: Option<Arc<rustls::ClientConfig>>,
    pub credentials: Option<(String, String)>,
    /// The longest one message may take, from connect to the reply to its data.
    pub timeout: Duration,
}

/// The relay, resolved and checked. Start only.
pub struct Relay {
    addr: SocketAddr,
    /// The EHLO command bytes, made once so a message writes them unchanged.
    ehlo: Vec<u8>,
    tls: Option<Arc<rustls::ClientConfig>>,
    name: Option<rustls::pki_types::ServerName<'static>>,
    credentials: Option<(String, String)>,
    timeout: Duration,
}

impl Relay {
    /// Checks the plain-SMTP rule, resolves `host` once and reads the EHLO
    /// name. Any failure is an error, so the service exits at start.
    pub fn new(cfg: &RelayConfig) -> io::Result<Relay> {
        if cfg.starttls.is_none()
            && !matches!(
                cfg.host.as_str(),
                "127.0.0.1" | "::1" | "[::1]" | "localhost"
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "plain SMTP to {:?} is refused: configure STARTTLS, or use 127.0.0.1, ::1 or localhost",
                    cfg.host
                ),
            ));
        }
        let addr = crate::net::resolve(&cfg.host, cfg.port)?;
        let mut ehlo = Vec::with_capacity(512);
        write!(ehlo, "{}", Ehlo::new(ClientId::default()))
            .expect("writing EHLO into a Vec cannot fail");
        let name = match &cfg.starttls {
            Some(_) => Some(
                rustls::pki_types::ServerName::try_from(cfg.host.clone()).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidInput, format!("{}: {e}", cfg.host))
                })?,
            ),
            None => None,
        };
        Ok(Relay {
            addr,
            ehlo,
            tls: cfg.starttls.clone(),
            name,
            credentials: cfg.credentials.clone(),
            timeout: cfg.timeout,
        })
    }
}

/// The state of the dot-stuffing codec, as lettre's `ClientCodec` keeps it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stuffing {
    Middle,
    AfterCr,
    LineStart,
}

/// The SMTP step the slot is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Idle,
    Connecting,
    Greeting,
    Ehlo,
    Starttls,
    Handshake,
    EhloAgain,
    Auth,
    AuthReply,
    Mail,
    Rcpt,
    Data,
    Body,
    BodyEnd,
    Done,
}

impl Step {
    /// The name an error reports for this step.
    fn name(self) -> &'static str {
        match self {
            Step::Idle => "idle",
            Step::Connecting => "connect",
            Step::Greeting => "greeting",
            Step::Ehlo | Step::EhloAgain => "EHLO",
            Step::Starttls => "STARTTLS",
            Step::Handshake => "handshake",
            Step::Auth | Step::AuthReply => "AUTH",
            Step::Mail => "MAIL",
            Step::Rcpt => "RCPT",
            Step::Data => "DATA",
            Step::Body | Step::BodyEnd => "end of data",
            Step::Done => "done",
        }
    }

    /// Whether the step is waiting for one complete reply.
    fn awaits_reply(self) -> bool {
        !matches!(
            self,
            Step::Idle | Step::Connecting | Step::Handshake | Step::Body | Step::Done
        )
    }
}

/// Why a delivery ended. `step` names the step that failed, `code` is the
/// relay's reply code when it sent one, and `text` is its first line.
#[derive(Debug)]
pub struct SmtpError {
    pub step: &'static str,
    pub code: Option<u16>,
    pub text: String,
}

impl std::fmt::Display for SmtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(f, "smtp {} failed: {code} {}", self.step, self.text),
            None => write!(f, "smtp {} failed: {}", self.step, self.text),
        }
    }
}

impl std::error::Error for SmtpError {}

/// One outgoing message's slot. Made at start with its buffers, reused.
pub struct Smtp {
    stream: Option<TcpStream>,
    tls: Option<ClientConnection>,
    /// The peer closed. The socket stays open until [`Smtp::close`] so the
    /// service can deregister the descriptor it registered.
    eof: bool,
    step: Step,
    in_buf: Vec<u8>,
    in_len: usize,
    out_buf: Vec<u8>,
    out_pos: usize,
    out_len: usize,
    /// The message bytes from `Message::formatted()`, kept for the data step.
    body: Vec<u8>,
    body_pos: usize,
    stuffing: Stuffing,
    /// The message's MAIL command: lettre's envelope sender and parameters.
    mail: Option<Mail>,
    /// The message's RCPT commands, one per envelope recipient.
    rcpts: Vec<Rcpt>,
    /// The recipient the RCPT step is on.
    rcpt_now: usize,
    /// The envelope carries a non-ASCII address, so MAIL needs SMTPUTF8.
    utf8_addresses: bool,
    /// The formatted body is not ASCII, so MAIL needs 8BITMIME.
    eight_bit: bool,
    /// The EHLO command bytes, copied from the relay at start.
    ehlo: Vec<u8>,
    /// The `smtp_reply` limit the input buffer grows under.
    reply_limit: crate::limits::Limit,
    credentials: Option<(String, String)>,
    mechanism: Option<Mechanism>,
    server: ServerInfo,
    tls_config: Option<Arc<rustls::ClientConfig>>,
    tls_name: Option<rustls::pki_types::ServerName<'static>>,
    deadline: Instant,
}

impl Smtp {
    /// At start: the buffers every message reuses, with the built-in maximum of
    /// `smtp_reply`.
    pub fn new(read_cap: usize, write_cap: usize) -> Smtp {
        use crate::limits::{Limit, SMTP_REPLY};
        let reply = Limit::custom(&SMTP_REPLY, read_cap.max(1), SMTP_REPLY.max.max(read_cap));
        Smtp::with_limit(&reply, write_cap)
    }

    /// At start: the buffers every message reuses, the input buffer following
    /// the `smtp_reply` limit.
    pub fn with_limit(reply: &crate::limits::Limit, write_cap: usize) -> Smtp {
        let read_cap = reply.start();
        assert!(
            read_cap > 0 && write_cap >= 8,
            "an SMTP slot needs both buffers"
        );
        Smtp {
            reply_limit: reply.clone(),
            stream: None,
            tls: None,
            eof: false,
            step: Step::Idle,
            in_buf: vec![0; read_cap],
            in_len: 0,
            out_buf: vec![0; write_cap],
            out_pos: 0,
            out_len: 0,
            body: Vec::new(),
            body_pos: 0,
            stuffing: Stuffing::LineStart,
            mail: None,
            rcpts: Vec::new(),
            rcpt_now: 0,
            utf8_addresses: false,
            eight_bit: false,
            ehlo: Vec::with_capacity(512),
            credentials: None,
            mechanism: None,
            server: ServerInfo::default(),
            tls_config: None,
            tls_name: None,
            deadline: crate::platform::now(),
        }
    }

    /// Starts a non-blocking connect for `msg`. Never waits.
    pub fn start(&mut self, relay: &Relay, msg: &lettre::Message) -> io::Result<()> {
        self.close();
        if relay.ehlo.len() > self.out_buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the write buffer is smaller than the EHLO command",
            ));
        }
        let envelope = msg.envelope();
        self.utf8_addresses = envelope
            .from()
            .is_some_and(|from| !AsRef::<str>::as_ref(from).is_ascii())
            || envelope
                .to()
                .iter()
                .any(|to| !AsRef::<str>::as_ref(to).is_ascii());
        self.body = msg.formatted();
        self.eight_bit = !self.body.is_ascii();
        let mut params = Vec::new();
        if self.utf8_addresses {
            params.push(MailParameter::SmtpUtfEight);
        }
        if self.eight_bit {
            params.push(MailParameter::Body(MailBodyParameter::EightBitMime));
        }
        self.mail = Some(Mail::new(envelope.from().cloned(), params));
        self.rcpts.clear();
        self.rcpts.extend(
            envelope
                .to()
                .iter()
                .map(|to| Rcpt::new(to.clone(), Vec::new())),
        );
        self.rcpt_now = 0;
        self.body_pos = 0;
        self.eof = false;
        self.stuffing = Stuffing::LineStart;
        self.in_len = 0;
        self.out_pos = 0;
        self.out_len = 0;
        self.mechanism = None;
        self.server = ServerInfo::default();
        self.ehlo.clear();
        self.ehlo.extend_from_slice(&relay.ehlo);
        self.tls_config = relay.tls.clone();
        self.tls_name = relay.name.clone();
        match (&mut self.credentials, &relay.credentials) {
            (Some(dst), Some(src)) => {
                dst.0.clone_from(&src.0);
                dst.1.clone_from(&src.1);
            }
            (slot, Some(src)) => *slot = Some(src.clone()),
            (slot, None) => *slot = None,
        }
        self.deadline = crate::platform::now() + relay.timeout;
        let (stream, state) = crate::platform::net::connect_tcp(relay.addr)?;
        self.stream = Some(stream);
        self.step = match state {
            crate::platform::net::Connect::Connected => Step::Greeting,
            crate::platform::net::Connect::InProgress => Step::Connecting,
        };
        Ok(())
    }

    /// The descriptor to register with the poll, or `None` while there is no
    /// connection: before `start`, and after `close` or a finished delivery.
    pub fn fd(&self) -> Option<RawFd> {
        self.stream.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// The interest to register for the current step.
    pub fn interest(&self) -> crate::platform::Interest {
        use crate::platform::Interest;
        match self.step {
            Step::Connecting => Interest::WRITABLE,
            Step::Handshake => Interest::READABLE | Interest::WRITABLE,
            _ if self.wants_write() => Interest::WRITABLE,
            _ => Interest::READABLE,
        }
    }

    /// The deadline the service puts in its outgoing `Timeouts`.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Advance on readiness. `Ok(None)` while unfinished, `Ok(Some(()))` when
    /// the relay accepted the message, `Err` naming the step and its reply.
    pub fn drive(&mut self) -> Result<Option<()>, SmtpError> {
        loop {
            match self.step {
                Step::Done => return Ok(Some(())),
                Step::Idle => return Ok(None),
                Step::Connecting => {
                    if !self.finish_connect()? {
                        return Ok(None);
                    }
                }
                Step::Handshake => {
                    if !self.handshake()? {
                        return Ok(None);
                    }
                    self.step = Step::EhloAgain;
                    self.queue_ehlo();
                }
                _ => {}
            }
            if self.wants_write() && !self.write_out()? {
                return Ok(None);
            }
            if self.step == Step::Body && !self.pump_body() {
                continue;
            }
            if !self.step.awaits_reply() {
                return Ok(None);
            }
            if let Some(reply) = self.take_reply()? {
                self.handle_reply(&reply)?;
                continue;
            }
            let before = self.in_len;
            self.read_in()?;
            match self.take_reply()? {
                Some(reply) => self.handle_reply(&reply)?,
                None => {
                    if self.in_len == before && self.eof {
                        return Err(self.error("the relay closed before its reply"));
                    }
                    return Ok(None);
                }
            }
        }
    }

    /// Drop the connection and make the slot free again.
    pub fn close(&mut self) {
        self.stream = None;
        self.tls = None;
        self.step = Step::Idle;
        self.eof = false;
        self.in_len = 0;
        self.out_pos = 0;
        self.out_len = 0;
        self.body.clear();
        self.mail = None;
        self.rcpts.clear();
    }

    // ── the socket ───────────────────────────────────────────────────────

    fn wants_write(&self) -> bool {
        self.out_pos < self.out_len
            || (self.step == Step::Body && self.body_pos < self.body.len())
            || self.tls.as_ref().is_some_and(|t| t.wants_write())
    }

    fn finish_connect(&mut self) -> Result<bool, SmtpError> {
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| self.error("the relay connection is gone"))?;
        match stream.take_error() {
            Ok(Some(e)) => return Err(self.error(&e.to_string())),
            Ok(None) => {}
            Err(e) => return Err(self.error(&e.to_string())),
        }
        if stream.peer_addr().is_err() {
            return Ok(false);
        }
        self.step = Step::Greeting;
        Ok(true)
    }

    fn handshake(&mut self) -> Result<bool, SmtpError> {
        let config = self
            .tls_config
            .clone()
            .ok_or_else(|| self.error("STARTTLS without a TLS config"))?;
        let name = self
            .tls_name
            .clone()
            .ok_or_else(|| self.error("STARTTLS without a server name"))?;
        if self.tls.is_none() {
            self.tls =
                Some(ClientConnection::new(config, name).map_err(|e| self.error(&e.to_string()))?);
        }
        let (Some(tls), Some(stream)) = (&mut self.tls, &mut self.stream) else {
            return Ok(true);
        };
        match crate::client::drive_tls_handshake(tls, stream) {
            Ok(done) => Ok(done),
            Err(e) => Err(self.error(&e.to_string())),
        }
    }

    /// Write the output buffer to the socket until it is empty or would block.
    fn write_out(&mut self) -> Result<bool, SmtpError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(self.error("the relay connection is gone"));
        };
        loop {
            if self.out_pos == self.out_len {
                self.out_pos = 0;
                self.out_len = 0;
                if let Some(tls) = self.tls.as_mut() {
                    match crate::client::flush_tls_to(tls, stream) {
                        Ok(true) => {}
                        Ok(false) => return Ok(false),
                        Err(e) => return Err(self.error(&e.to_string())),
                    }
                }
                return Ok(true);
            }
            let written = if let Some(tls) = self.tls.as_mut() {
                match tls
                    .writer()
                    .write(&self.out_buf[self.out_pos..self.out_len])
                {
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(self.error(&e.to_string())),
                }
            } else {
                match stream.write(&self.out_buf[self.out_pos..self.out_len]) {
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(self.error(&e.to_string())),
                }
            };
            self.out_pos += written;
            if written == 0 {
                if let Some(tls) = self.tls.as_mut() {
                    match crate::client::flush_tls_to(tls, stream) {
                        Ok(true) => {}
                        Ok(false) => return Ok(false),
                        Err(e) => return Err(self.error(&e.to_string())),
                    }
                    if self.out_pos == self.out_len {
                        continue;
                    }
                }
                return Ok(false);
            }
        }
    }

    /// Read what the socket has, until WouldBlock or EOF.
    fn read_in(&mut self) -> Result<(), SmtpError> {
        self.make_room()?;
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        if let Some(tls) = self.tls.as_mut() {
            return match crate::client::read_tls_into(
                tls,
                stream,
                &mut self.in_buf,
                &mut self.in_len,
            ) {
                Ok(true) => {
                    self.eof = true;
                    Ok(())
                }
                Ok(false) => Ok(()),
                Err(e) => Err(self.error(&e.to_string())),
            };
        }
        loop {
            if self.in_len == self.in_buf.len() {
                // `stream` borrows `self`, so the growth is repeated here
                // from the fields it leaves free.
                let Some(grown) = self
                    .reply_limit
                    .grown(self.in_buf.len(), self.in_buf.len() + 1)
                else {
                    let refusal = self
                        .reply_limit
                        .exceeded(self.in_buf.len() + 1, "the relay's reply");
                    return Err(self.error(&refusal.to_string()));
                };
                let from = self.in_buf.len();
                self.in_buf.resize(grown, 0);
                self.reply_limit
                    .log_growth(from, grown, "the relay's reply");
            }
            match stream.read(&mut self.in_buf[self.in_len..]) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(());
                }
                Ok(n) => self.in_len += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if e.kind() == io::ErrorKind::ConnectionReset
                        || e.kind() == io::ErrorKind::BrokenPipe =>
                {
                    self.eof = true;
                    return Ok(());
                }
                Err(e) => return Err(self.error(&e.to_string())),
            }
        }
    }

    // ── replies and commands ─────────────────────────────────────────────

    /// Grow the input buffer when it is full, or refuse the reply past the
    /// `smtp_reply` maximum.
    fn make_room(&mut self) -> Result<(), SmtpError> {
        if self.in_len < self.in_buf.len() {
            return Ok(());
        }
        let Some(grown) = self
            .reply_limit
            .grown(self.in_buf.len(), self.in_buf.len() + 1)
        else {
            let refusal = self
                .reply_limit
                .exceeded(self.in_buf.len() + 1, "the relay's reply");
            return Err(self.error(&refusal.to_string()));
        };
        let from = self.in_buf.len();
        self.in_buf.resize(grown, 0);
        self.reply_limit
            .log_growth(from, grown, "the relay's reply");
        Ok(())
    }

    /// Remove one complete reply from the input buffer, if there is one.
    fn take_reply(&mut self) -> Result<Option<Response>, SmtpError> {
        let end = match reply_end(&self.in_buf[..self.in_len], self.reply_limit.max()) {
            Ok(Some(end)) => end,
            Ok(None) => return Ok(None),
            Err(why) => return Err(self.error(why)),
        };
        let text = std::str::from_utf8(&self.in_buf[..end])
            .map_err(|_| self.error("the relay reply is not UTF-8"))?;
        let reply = text
            .parse::<Response>()
            .map_err(|e| self.error(&format!("cannot parse the relay reply: {e}")))?;
        self.in_buf.copy_within(end..self.in_len, 0);
        self.in_len -= end;
        Ok(Some(reply))
    }

    fn handle_reply(&mut self, reply: &Response) -> Result<(), SmtpError> {
        let code: u16 = reply.code().into();
        if self.step == Step::Auth || self.step == Step::AuthReply {
            return match code {
                334 => self.auth_challenge(reply),
                235 => {
                    self.step = Step::Mail;
                    self.queue_mail()
                }
                _ => Err(self.reply_error(code, reply.first_line().unwrap_or(""))),
            };
        }
        let expected = match self.step {
            Step::Greeting | Step::Starttls => 220,
            Step::Ehlo | Step::EhloAgain | Step::Mail | Step::Rcpt | Step::BodyEnd => 250,
            Step::Data => 354,
            _ => 0,
        };
        if code != expected {
            return Err(self.reply_error(code, reply.first_line().unwrap_or("")));
        }
        match self.step {
            Step::Greeting => {
                self.step = Step::Ehlo;
                self.queue_ehlo();
            }
            Step::Ehlo | Step::EhloAgain => {
                // The extensions are needed only for STARTTLS, AUTH, or a
                // non-ASCII address or body. A plain ASCII message skips the
                // parse, and with it its allocations.
                if self.tls_config.is_some()
                    || self.credentials.is_some()
                    || self.utf8_addresses
                    || self.eight_bit
                {
                    self.server = ServerInfo::from_response(reply)
                        .map_err(|e| self.reply_error(code, &e.to_string()))?;
                }
                if self.step == Step::Ehlo && self.tls_config.is_some() {
                    if !self.server.supports_feature(Extension::StartTls) {
                        return Err(SmtpError {
                            step: "STARTTLS",
                            code: None,
                            text: "the relay does not offer STARTTLS".into(),
                        });
                    }
                    self.step = Step::Starttls;
                    self.queue(Starttls);
                } else {
                    self.after_ehlo()?;
                }
            }
            Step::Starttls => {
                self.step = Step::Handshake;
            }
            Step::Mail => {
                self.rcpt_now = 0;
                self.next_rcpt();
            }
            Step::Rcpt => {
                self.rcpt_now += 1;
                self.next_rcpt();
            }
            Step::Data => {
                self.step = Step::Body;
                self.body_pos = 0;
            }
            Step::BodyEnd => {
                self.queue(Quit);
                let _ = self.write_out();
                self.step = Step::Done;
                return Ok(());
            }
            _ => {}
        }
        Ok(())
    }

    fn after_ehlo(&mut self) -> Result<(), SmtpError> {
        if self.credentials.is_some() {
            let mechanism = self
                .server
                .get_auth_mechanism(DEFAULT_MECHANISMS)
                .ok_or_else(|| self.error("the relay offers no PLAIN or LOGIN authentication"))?;
            self.mechanism = Some(mechanism);
            self.step = Step::Auth;
            let credentials = self.credentials_now()?;
            let auth = Auth::new(mechanism, credentials, None)
                .map_err(|e| self.reply_error(0, &e.to_string()))?;
            self.queue(auth);
        } else {
            self.queue_mail()?;
        }
        Ok(())
    }

    fn auth_challenge(&mut self, reply: &Response) -> Result<(), SmtpError> {
        let mechanism = self
            .mechanism
            .ok_or_else(|| self.error("the relay challenged an unstarted AUTH"))?;
        let credentials = self.credentials_now()?;
        let auth = Auth::new_from_response(mechanism, credentials, reply)
            .map_err(|e| self.reply_error(334, &e.to_string()))?;
        self.step = Step::AuthReply;
        self.queue(auth);
        Ok(())
    }

    fn credentials_now(&self) -> Result<Credentials, SmtpError> {
        let (user, pass) = self
            .credentials
            .as_ref()
            .ok_or_else(|| self.error("AUTH without credentials"))?;
        Ok(Credentials::new(user.clone(), pass.clone()))
    }

    fn queue_ehlo(&mut self) {
        let len = self.ehlo.len();
        self.out_buf[..len].copy_from_slice(&self.ehlo);
        self.out_pos = 0;
        self.out_len = len;
    }

    /// Write one lettre command into the slot's output buffer. `Display` gives
    /// the bytes and nothing is formatted into a `String` first.
    fn queue(&mut self, command: impl Display) {
        self.out_len = write_command(&mut self.out_buf, &command);
        self.out_pos = 0;
    }

    /// Write the message's MAIL command from lettre's envelope, after checking
    /// the extensions the body and the addresses need.
    fn queue_mail(&mut self) -> Result<(), SmtpError> {
        if self.utf8_addresses && !self.server.supports_feature(Extension::SmtpUtfEight) {
            return Err(self.error("non-ASCII address without SMTPUTF8"));
        }
        if self.eight_bit && !self.server.supports_feature(Extension::EightBitMime) {
            return Err(self.error("non-ASCII message without 8BITMIME"));
        }
        self.step = Step::Mail;
        let mail = self
            .mail
            .as_ref()
            .expect("a started message has its MAIL command");
        self.out_len = write_command(&mut self.out_buf, mail);
        self.out_pos = 0;
        Ok(())
    }

    fn next_rcpt(&mut self) {
        if self.rcpt_now >= self.rcpts.len() {
            self.step = Step::Data;
            self.queue(Data);
            return;
        }
        self.step = Step::Rcpt;
        self.out_len = write_command(&mut self.out_buf, &self.rcpts[self.rcpt_now]);
        self.out_pos = 0;
    }

    /// Move the dot-stuffed message bytes into the output buffer, then the
    /// terminator. Returns whether the terminator is queued.
    fn pump_body(&mut self) -> bool {
        let cap = self.out_buf.len();
        while self.body_pos < self.body.len() && self.out_len + 2 <= cap {
            let b = self.body[self.body_pos];
            self.body_pos += 1;
            self.out_buf[self.out_len] = b;
            self.out_len += 1;
            match (b, self.stuffing) {
                (b'\r', _) => self.stuffing = Stuffing::AfterCr,
                (b'\n', Stuffing::AfterCr) => self.stuffing = Stuffing::LineStart,
                (_, Stuffing::AfterCr) => self.stuffing = Stuffing::Middle,
                (b'.', Stuffing::LineStart) => {
                    self.stuffing = Stuffing::Middle;
                    self.out_buf[self.out_len] = b'.';
                    self.out_len += 1;
                }
                (_, Stuffing::LineStart) => self.stuffing = Stuffing::Middle,
                _ => {}
            }
        }
        if self.body_pos == self.body.len() && self.out_len + 5 <= cap {
            self.out_buf[self.out_len..self.out_len + 5].copy_from_slice(b"\r\n.\r\n");
            self.out_len += 5;
            self.step = Step::BodyEnd;
        }
        self.step == Step::BodyEnd
    }

    // ── errors ───────────────────────────────────────────────────────────

    fn error(&self, text: &str) -> SmtpError {
        SmtpError {
            step: self.step.name(),
            code: None,
            text: text.to_owned(),
        }
    }

    fn reply_error(&self, code: u16, text: &str) -> SmtpError {
        SmtpError {
            step: self.step.name(),
            code: Some(code),
            text: text.to_owned(),
        }
    }
}

/// Write one lettre command into `buf` and return its length.
fn write_command(buf: &mut [u8], command: &impl Display) -> usize {
    let mut out = &mut buf[..];
    let len = out.len();
    write!(out, "{command}").expect("the slot's buffer holds the command");
    len - out.len()
}

/// The end of one complete reply in `buf`, or `Ok(None)` when more bytes are
/// needed. Every line starts with three digits; a space after them ends the
/// reply and a hyphen continues it.
fn reply_end(buf: &[u8], max: usize) -> Result<Option<usize>, &'static str> {
    if buf.len() > max {
        return Err("the reply is over the smtp_reply maximum");
    }
    let mut start = 0usize;
    loop {
        let Some(rel) = buf[start..].windows(2).position(|w| w == b"\r\n") else {
            if buf.len() - start > 1000 {
                return Err("a reply line is over 1000 bytes");
            }
            return Ok(None);
        };
        let line = &buf[start..start + rel];
        if line.len() > 1000 {
            return Err("a reply line is over 1000 bytes");
        }
        if line.len() < 3 || !line[..3].iter().all(u8::is_ascii_digit) {
            return Err("a reply line does not start with a code");
        }
        if line.len() == 3 || line[3] == b' ' {
            return Ok(Some(start + rel + 2));
        }
        if line[3] != b'-' {
            return Err("a reply line has neither a space nor a hyphen after its code");
        }
        start += rel + 2;
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: the scripted SMTP peer is a blocking thread with a socket timeout"
)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::platform::{Events, Poll, Token};

    /// One in-process SMTP peer: it writes `greeting`, then for each scripted
    /// step reads one command and answers. A step named `BODY` reads the data
    /// up to its terminator and records it unstuffed.
    fn fixture(
        greeting: &'static str,
        script: Vec<(&'static str, &'static str)>,
    ) -> (u16, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            sock.write_all(greeting.as_bytes()).expect("greeting");
            for (expect, reply) in script {
                if expect == "BODY" {
                    let wire = read_until(&mut sock, b"\r\n.\r\n");
                    let typed = &wire[..wire.len() - 5];
                    let body = unstuff(typed);
                    seen.lock()
                        .unwrap()
                        .push(format!("wire:{}", String::from_utf8_lossy(typed)));
                    seen.lock()
                        .unwrap()
                        .push(format!("body:{}", String::from_utf8_lossy(&body)));
                } else {
                    let line = read_until(&mut sock, b"\r\n");
                    let text = String::from_utf8_lossy(&line).into_owned();
                    assert!(
                        text.starts_with(expect),
                        "expected {expect:?}, got {text:?}"
                    );
                    seen.lock().unwrap().push(text.trim_end().to_owned());
                }
                sock.write_all(reply.as_bytes()).expect("reply");
            }
        });
        (port, log, handle)
    }

    fn read_until(sock: &mut impl Read, needle: &[u8]) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while !got.windows(needle.len()).any(|w| w == needle) {
            let n = sock.read(&mut buf).expect("fixture read");
            assert!(n > 0, "the client closed before {needle:?}");
            got.extend_from_slice(&buf[..n]);
        }
        got
    }

    fn unstuff(wire: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(wire.len());
        let mut line_start = true;
        let mut i = 0;
        while i < wire.len() {
            if line_start && wire[i] == b'.' {
                i += 1;
            }
            line_start = false;
            if i < wire.len() {
                out.push(wire[i]);
                if wire[i] == b'\n' {
                    line_start = true;
                }
                i += 1;
            }
        }
        out
    }

    fn message(body: &str) -> lettre::Message {
        lettre::Message::builder()
            .from("Sender <from@example.com>".parse().expect("from"))
            .to("To <to@example.com>".parse().expect("to"))
            .subject("a subject")
            .body(body.to_owned())
            .expect("message")
    }

    fn relay(port: u16) -> Relay {
        Relay::new(&RelayConfig {
            host: "127.0.0.1".into(),
            port,
            starttls: None,
            credentials: None,
            timeout: Duration::from_secs(10),
        })
        .expect("relay")
    }

    /// Start one message and drive it with a poll to its end.
    fn deliver(
        relay: &Relay,
        msg: &lettre::Message,
        read_cap: usize,
        write_cap: usize,
    ) -> (Result<Option<()>, SmtpError>, Smtp) {
        deliver_with(Smtp::new(read_cap, write_cap), relay, msg)
    }

    /// As [`deliver`], with a slot the test built.
    fn deliver_with(
        mut smtp: Smtp,
        relay: &Relay,
        msg: &lettre::Message,
    ) -> (Result<Option<()>, SmtpError>, Smtp) {
        smtp.start(relay, msg).expect("start");
        let mut poll = Poll::new().expect("poll");
        let mut events = Events::with_capacity(8);
        let token = Token(0);
        poll.registry()
            .register_fd(
                smtp.fd().expect("a started delivery has a socket"),
                token,
                smtp.interest(),
            )
            .expect("register");
        let mut interest = smtp.interest();
        let deadline = crate::platform::now() + Duration::from_secs(30);
        loop {
            match smtp.drive() {
                Ok(None) => {}
                other => return (other, smtp),
            }
            if smtp.interest() != interest {
                interest = smtp.interest();
                poll.registry()
                    .reregister_fd(
                        smtp.fd().expect("a started delivery has a socket"),
                        token,
                        interest,
                    )
                    .expect("reregister");
            }
            poll.poll(&mut events, Some(Duration::from_millis(100)))
                .expect("poll");
            assert!(
                crate::platform::now() < deadline,
                "the fixture did not finish in time"
            );
        }
    }

    /// A reply larger than the slot's buffer grows it, and one past the
    /// `smtp_reply` maximum is refused with an error naming the key.
    #[test]
    fn a_reply_grows_the_buffer_and_past_the_maximum_is_refused() {
        use crate::limits::{Limit, SMTP_REPLY};
        // A greeting of about 2 KiB: 20 continuation lines of 100 bytes.
        let line = format!("220-{}\r\n", "x".repeat(96));
        let greeting: &'static str =
            Box::leak(format!("{}220 ready\r\n", line.repeat(20)).into_boxed_str());

        let (port, _log, handle) = fixture(greeting, vec![("EHLO", "250 ok\r\n")]);
        let grows = Smtp::with_limit(&Limit::custom(&SMTP_REPLY, 512, 4096), 4096);
        let (result, smtp) = deliver_with(grows, &relay(port), &message("hi\r\n"));
        // The greeting was read whole: the failure, if any, is not the buffer's.
        let text = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(!text.contains("smtp_reply"), "{text}");
        let size = smtp.in_buf.len();
        assert!(
            (2048..=4096).contains(&size) && size.is_power_of_two(),
            "{size}"
        );
        drop(handle);

        let (port, _log, handle) = fixture(greeting, vec![]);
        let refuses = Smtp::with_limit(&Limit::custom(&SMTP_REPLY, 512, 1024), 4096);
        let (result, _) = deliver_with(refuses, &relay(port), &message("hi\r\n"));
        let text = result.expect_err("a reply past the maximum").to_string();
        assert!(
            text.contains("over [limits.smtp_reply] max = 1 KiB"),
            "{text}"
        );
        drop(handle);
    }

    #[test]
    fn a_plain_message_is_delivered() {
        let (port, log, peer) = fixture(
            "220 test\r\n",
            vec![
                ("EHLO", "250-test\r\n250 8BITMIME\r\n"),
                ("MAIL", "250 ok\r\n"),
                ("RCPT", "250 ok\r\n"),
                ("DATA", "354 go\r\n"),
                ("BODY", "250 queued\r\n"),
            ],
        );
        let msg = message("hello\r\n");
        let (result, mut smtp) = deliver(&relay(port), &msg, 4096, 4096);
        assert!(result.expect("delivery").is_some());
        smtp.close();
        peer.join().expect("peer");
        let seen = log.lock().unwrap();
        assert!(seen[0].starts_with("EHLO "), "{seen:?}");
        assert_eq!(seen[1], "MAIL FROM:<from@example.com>");
        assert_eq!(seen[2], "RCPT TO:<to@example.com>");
        assert_eq!(seen[3], "DATA");
        assert_eq!(
            seen[5],
            format!("body:{}", String::from_utf8_lossy(&msg.formatted()))
        );
    }

    #[test]
    fn a_line_starting_with_a_dot_is_stuffed() {
        let (port, log, peer) = fixture(
            "220 test\r\n",
            vec![
                ("EHLO", "250 test\r\n"),
                ("MAIL", "250 ok\r\n"),
                ("RCPT", "250 ok\r\n"),
                ("DATA", "354 go\r\n"),
                ("BODY", "250 queued\r\n"),
            ],
        );
        let msg = message("hello\r\n.hidden\r\nbye\r\n");
        let (result, mut smtp) = deliver(&relay(port), &msg, 4096, 4096);
        assert!(result.expect("delivery").is_some());
        smtp.close();
        peer.join().expect("peer");
        let seen = log.lock().unwrap();
        let wire = seen[4].strip_prefix("wire:").expect("wire recorded");
        assert!(
            wire.contains("..hidden"),
            "the dot must be doubled: {wire:?}"
        );
        let body = seen[5].strip_prefix("body:").expect("body recorded");
        assert_eq!(body, String::from_utf8_lossy(&msg.formatted()));
    }

    #[test]
    fn a_5xx_at_a_step_ends_the_delivery() {
        let (port, log, peer) = fixture(
            "220 test\r\n",
            vec![("EHLO", "250 test\r\n"), ("MAIL", "554 no\r\n")],
        );
        let (result, mut smtp) = deliver(&relay(port), &message("hello\r\n"), 4096, 4096);
        let err = result.expect_err("the 554 must fail the delivery");
        assert_eq!(err.step, "MAIL", "{err}");
        assert_eq!(err.code, Some(554), "{err}");
        smtp.close();
        peer.join().expect("peer");
        let seen = log.lock().unwrap();
        assert_eq!(seen.len(), 2, "no command after the refused one: {seen:?}");
    }

    #[test]
    fn a_body_larger_than_the_buffer_is_sent_whole() {
        let (port, log, peer) = fixture(
            "220 test\r\n",
            vec![
                ("EHLO", "250 test\r\n"),
                ("MAIL", "250 ok\r\n"),
                ("RCPT", "250 ok\r\n"),
                ("DATA", "354 go\r\n"),
                ("BODY", "250 queued\r\n"),
            ],
        );
        let msg = message(&"x".repeat(1024 * 1024));
        let (result, mut smtp) = deliver(&relay(port), &msg, 8192, 4096);
        assert!(result.expect("delivery").is_some());
        smtp.close();
        peer.join().expect("peer");
        let seen = log.lock().unwrap();
        assert_eq!(
            seen[5],
            format!("body:{}", String::from_utf8_lossy(&msg.formatted()))
        );
    }

    #[test]
    fn plain_smtp_to_a_remote_host_is_refused_at_start() {
        let err = match Relay::new(&RelayConfig {
            host: "192.0.2.1".into(),
            port: 25,
            starttls: None,
            credentials: None,
            timeout: Duration::from_secs(1),
        }) {
            Ok(_) => panic!("plain SMTP to a non-loopback host must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("plain SMTP"), "{err}");
    }

    #[test]
    fn a_relay_host_that_does_not_resolve_is_an_error_at_start() {
        let tls = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        );
        let err = match Relay::new(&RelayConfig {
            host: "does-not-exist.invalid".into(),
            port: 25,
            starttls: Some(tls),
            credentials: None,
            timeout: Duration::from_secs(1),
        }) {
            Ok(_) => panic!("an unresolvable host must fail at start"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("does-not-exist.invalid"), "{err}");
    }

    #[test]
    fn starttls_then_auth_then_delivery() {
        use rustls::pki_types::PrivateKeyDer;

        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("cert");
        let cert = ck.cert.der().clone();
        let key = PrivateKeyDer::try_from(ck.key_pair.serialize_der()).expect("key");
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server_config = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .expect("server config"),
        );
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("trust the run's own cert");
        let client_config = Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&log);
        let peer = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            sock.write_all(b"220 test\r\n").expect("greeting");
            let ehlo = read_until(&mut sock, b"\r\n");
            seen.lock().unwrap().push(format!(
                "plain:{}",
                String::from_utf8_lossy(&ehlo).trim_end()
            ));
            sock.write_all(b"250-test\r\n250-STARTTLS\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n")
                .expect("ehlo reply");
            let starttls = read_until(&mut sock, b"\r\n");
            assert!(starttls.starts_with(b"STARTTLS"), "{starttls:?}");
            sock.write_all(b"220 go\r\n").expect("starttls reply");

            let conn = rustls::ServerConnection::new(server_config).expect("server conn");
            let mut tls = rustls::StreamOwned::new(conn, sock);
            let ehlo = read_until(&mut tls, b"\r\n");
            seen.lock()
                .unwrap()
                .push(format!("tls:{}", String::from_utf8_lossy(&ehlo).trim_end()));
            tls.write_all(b"250-test\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n")
                .expect("second ehlo reply");
            let auth = read_until(&mut tls, b"\r\n");
            seen.lock().unwrap().push(format!(
                "auth:{}",
                String::from_utf8_lossy(&auth).trim_end()
            ));
            assert!(auth.starts_with(b"AUTH PLAIN "), "{auth:?}");
            tls.write_all(b"235 ok\r\n").expect("auth reply");
            for (expect, reply) in [
                (&b"MAIL"[..], &b"250 ok\r\n"[..]),
                (b"RCPT", b"250 ok\r\n"),
                (b"DATA", b"354 go\r\n"),
            ] {
                let line = read_until(&mut tls, b"\r\n");
                assert!(
                    line.starts_with(expect),
                    "{:?}",
                    String::from_utf8_lossy(&line)
                );
                tls.write_all(reply).expect("step reply");
            }
            let wire = read_until(&mut tls, b"\r\n.\r\n");
            let typed = &wire[..wire.len() - 5];
            seen.lock()
                .unwrap()
                .push(format!("body:{}", String::from_utf8_lossy(&unstuff(typed))));
            tls.write_all(b"250 queued\r\n").expect("data reply");
        });

        let relay = Relay::new(&RelayConfig {
            host: "localhost".into(),
            port,
            starttls: Some(client_config),
            credentials: Some(("user".into(), "pass".into())),
            timeout: Duration::from_secs(10),
        })
        .expect("relay");
        let msg = message("hello over tls\r\n");
        let (result, mut smtp) = deliver(&relay, &msg, 8192, 8192);
        assert!(result.expect("delivery").is_some());
        smtp.close();
        peer.join().expect("peer");
        let seen = log.lock().unwrap();
        assert!(
            seen[1].starts_with("tls:EHLO"),
            "the second EHLO must be over TLS: {seen:?}"
        );
        assert!(
            seen[2].starts_with("auth:AUTH PLAIN "),
            "AUTH must follow the handshake: {seen:?}"
        );
        assert_eq!(
            seen[3],
            format!("body:{}", String::from_utf8_lossy(&msg.formatted()))
        );
    }

    #[test]
    fn auth_login_answers_the_challenges() {
        let (port, log, peer) = fixture(
            "220 test\r\n",
            vec![
                ("EHLO", "250-test\r\n250-AUTH LOGIN\r\n250 8BITMIME\r\n"),
                ("AUTH LOGIN", "334 VXNlcm5hbWU6\r\n"),
                ("dXNlcg==", "334 UGFzc3dvcmQ6\r\n"),
                ("cGFzcw==", "235 ok\r\n"),
                ("MAIL", "250 ok\r\n"),
                ("RCPT", "250 ok\r\n"),
                ("DATA", "354 go\r\n"),
                ("BODY", "250 queued\r\n"),
            ],
        );
        let relay = Relay::new(&RelayConfig {
            host: "127.0.0.1".into(),
            port,
            starttls: None,
            credentials: Some(("user".into(), "pass".into())),
            timeout: Duration::from_secs(10),
        })
        .expect("relay");
        let msg = message("hello\r\n");
        let (result, mut smtp) = deliver(&relay, &msg, 4096, 4096);
        assert!(result.expect("delivery").is_some());
        smtp.close();
        peer.join().expect("peer");
        let seen = log.lock().unwrap();
        assert_eq!(seen[1], "AUTH LOGIN");
        assert_eq!(seen[2], "dXNlcg==");
        assert_eq!(seen[3], "cGFzcw==");
        assert_eq!(
            seen[8],
            format!("body:{}", String::from_utf8_lossy(&msg.formatted()))
        );
    }

    #[test]
    fn a_relay_without_starttls_is_refused_before_auth() {
        let (port, log, peer) = fixture("220 test\r\n", vec![("EHLO", "250 no-tls-here\r\n")]);
        let tls = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        );
        let relay = Relay::new(&RelayConfig {
            host: "127.0.0.1".into(),
            port,
            starttls: Some(tls),
            credentials: Some(("user".into(), "pass".into())),
            timeout: Duration::from_secs(5),
        })
        .expect("relay");
        let (result, mut smtp) = deliver(&relay, &message("hello\r\n"), 4096, 4096);
        let err = result.expect_err("a relay without STARTTLS must be refused");
        assert_eq!(err.step, "STARTTLS", "{err}");
        smtp.close();
        peer.join().expect("peer");
        let seen = log.lock().unwrap();
        assert_eq!(seen.len(), 1, "no AUTH or MAIL may be sent: {seen:?}");
        assert!(seen[0].starts_with("EHLO "), "{seen:?}");
    }
}
