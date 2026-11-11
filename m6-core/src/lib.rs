pub mod client;
pub mod compress;
pub mod conditional;
pub mod config;
pub mod cookie;
pub mod dict;
pub mod dns;
pub mod error;
pub mod event;
pub mod firewall;
pub mod h1;
pub mod headers;
pub mod host;
pub mod http;
pub mod log;
pub mod mime;
/// The HTML, CSS, JavaScript and JSON minifiers. On by default; see the
/// `minify` feature in Cargo.toml.
#[cfg(feature = "minify")]
pub mod minify;
pub mod monitoring;
pub mod ndjson;
pub mod negotiate;
pub mod net;
pub mod parse;
pub mod path;
pub mod random;
pub mod render;
pub mod request;
pub mod resolve;
pub mod response;
pub mod server;
pub mod signal;
pub mod site;
pub mod telemetry;
pub mod time_range;
pub mod tls;
pub mod util;
pub mod wireguard;

// Behind a feature so it is never compiled into a production binary.
// Enable it in `dev-dependencies` only. The module's own `//!` doc describes it.
#[cfg(feature = "testkit")]
pub mod testkit;

#[cfg(feature = "multipart")]
pub mod multipart;

#[cfg(feature = "sqlite")]
pub mod sqlite;

/// Tera templating and the site filters that go with it. On by default;
/// see the `templates` feature in Cargo.toml.
#[cfg(feature = "templates")]
pub mod template;

#[cfg(feature = "templates")]
pub use template::{TeraFactory, TeraRenderer};

/// The reactor: `Poll`, `Events`, `Token`, `Interest` and the non-blocking
/// sockets. Re-exported so a service names `m6_core::mio::...` and links only
/// m6-core.
pub use mio;

pub use compress::{brotli_compress, brotli_decompress, gzip_compress, gzip_decompress};
pub use conditional::{
    evaluate_preconditions, is_not_modified, not_modified_headers, Precondition,
};
pub use config::{RendererConfig, RouteConfig};
pub use cookie::{Cookie, SameSite};
pub use dict::Dict;
pub use error::{Error, Result};
pub use h1::{parse_request, ParseResult};
pub use http::{header, is_same_origin_path, HeaderSource, RawRequest, RawResponse};
pub use mime::{mime_from_path, should_compress_default};
pub use negotiate::{canonical_coding, coding_quality, preferred_coding};
pub use path::validate_path_param;
pub use random::random_hex_token;
pub use render::{NoTemplates, RenderError, Renderer, RendererFactory};
pub use request::Request;
pub use response::{Body, Response};
pub use server::socket_path_from_config;
pub use signal::ShutdownHandle;
pub use site::{CodeRoute, Handled, Site};

/// Everything a service normally wants, in one line.
///
/// `use m6_core::prelude::*;` is the intended first line of an m6 service.
/// m6-core is the box of blocks: linking it should be the only thing a new
/// service has to do to get routing, templating, a request dictionary, the
/// event-loop parts and the helpers that go with them. The loop is the
/// service's own, written out in its `main`.
///
/// ```rust,no_run
/// use std::io;
/// use std::time::{Duration, Instant};
///
/// use m6_core::prelude::*;
///
/// const ROUTES: &[CodeRoute] = &[CodeRoute {
///     pattern: "/healthz",
///     method: RouteMethod::Get,
/// }];
/// const LISTENER: Token = Token(usize::MAX);
///
/// fn main() -> anyhow::Result<()> {
///     let site = Site::load("example", ROUTES, &[], &NoTemplates);
///     let mut poll = Poll::new()?;
///     let mut listener = listen_unix(site.socket_path(), site.socket_mode())?;
///     poll.registry()
///         .register(&mut listener, LISTENER, Interest::READABLE)?;
///     let mut conns = Slots::new(1024, 1024, || ServerConn::new(16 * 1024, 64 * 1024));
///     let mut deadlines = Timeouts::new(1024, Duration::from_secs(30));
///     let mut events = Events::with_capacity(256);
///
///     loop {
///         poll.poll(
///             &mut events,
///             Some(deadlines.timeout(Instant::now(), Duration::from_secs(1))),
///         )?;
///         for event in &events {
///             match event.token() {
///                 // Accept until the kernel has no more, and give the slot
///                 // back when there was nothing to accept after all.
///                 LISTENER => {
///                     while let Some(taken) = conns.take() {
///                         let mut stream = match listener.accept() {
///                             Ok((stream, _)) => stream,
///                             Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
///                                 conns.free(taken.key);
///                                 break;
///                             }
///                             Err(e) => return Err(e.into()),
///                         };
///                         poll.registry().register(
///                             &mut stream,
///                             Token(taken.key),
///                             Interest::READABLE,
///                         )?;
///                         deadlines.touch(Token(taken.key), Instant::now());
///                         conns.get_mut(taken.key).attach(stream);
///                     }
///                 }
///                 // One connection: read, dispatch, write, and close when the
///                 // peer or the response is done.
///                 Token(k) => {
///                     let conn = conns.get_mut(k);
///                     let was = conn.interest();
///                     let mut keep = true;
///                     loop {
///                         if conn.interest() == Interest::WRITABLE {
///                             match conn.flush()? {
///                                 Flush::More => break,
///                                 Flush::Close => {
///                                     keep = false;
///                                     break;
///                                 }
///                                 Flush::Done => {}
///                             }
///                         }
///                         let mut heads = head_slots();
///                         let response = match conn.read(&mut heads)? {
///                             Read::Request(head) => match site.handle(&head) {
///                                 Handled::Done(response) => Some(response),
///                                 Handled::Code(0, req) => {
///                                     Some(site.finish(&req, Response::text("ok")))
///                                 }
///                                 Handled::Code(..) | Handled::Named(..) => unreachable!(),
///                             },
///                             Read::More => None,
///                             Read::Closed => {
///                                 keep = false;
///                                 break;
///                             }
///                         };
///                         if let Some(response) = response {
///                             conn.respond_response(response);
///                         }
///                         match conn.flush()? {
///                             Flush::Done => break,
///                             Flush::More => break,
///                             Flush::Close => {
///                                 keep = false;
///                                 break;
///                             }
///                         }
///                     }
///                     if !keep {
///                         m6_core::h1::close_conn(&poll, &mut conns, &mut deadlines, k)?;
///                     } else {
///                         deadlines.touch(Token(k), Instant::now());
///                         if conn.interest() != was {
///                             let interest = conn.interest();
///                             let stream = conn
///                                 .stream()
///                                 .expect("every registered token has a connection");
///                             poll.registry().reregister(stream, Token(k), interest)?;
///                         }
///                     }
///                 }
///             }
///         }
///         // Close a peer that sent nothing before its read deadline.
///         while let Some(Token(k)) = deadlines.expired(Instant::now()) {
///             m6_core::h1::close_conn(&poll, &mut conns, &mut deadlines, k)?;
///         }
///     }
/// }
/// ```
///
/// The signal arm and the stop phase are two more pieces of the same shape:
/// `Signals` is a descriptor under its own token, and `Lifecycle::complete()`
/// runs after the loop. `docs/m6-app-anatomy.md` has them in place.
pub mod prelude {
    pub use crate::dict::Dict;
    pub use crate::error::{Error, Result};
    pub use crate::event::{LoopTrace, Signal, Signals, Slots, Timeouts};
    pub use crate::h1::{head_slots, Flush, Read, ServerConn};
    pub use crate::mio;
    pub use crate::mio::{Events, Interest, Poll, Token};
    pub use crate::net::listen_unix;
    pub use crate::render::NoTemplates;
    pub use crate::request::Request;
    pub use crate::response::Response;
    pub use crate::site::{CodeRoute, Handled, RouteMethod, Site};
    pub use crate::util::{now_iso8601, slugify, today_iso8601};
    pub use serde_json::{json, Map, Value};

    #[cfg(feature = "email")]
    pub use lettre::{Message, SmtpTransport, Transport};

    #[cfg(feature = "http-client")]
    pub use ureq;

    #[cfg(feature = "multipart")]
    pub use crate::multipart::Upload;

    #[cfg(feature = "sqlite")]
    pub use rusqlite;

    #[cfg(feature = "templates")]
    pub use crate::template::{TeraFactory, TeraRenderer};
}
