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
/// The platform layer, the one crate that knows the operating system. See
/// `docs/m6-platform.md`.
pub use m6_platform as platform;
/// The TLS library `client` and `email` take their configuration from,
/// re-exported so a service names `m6_core::rustls::ClientConfig` with no
/// `rustls` dependency of its own and cannot get a second copy.
pub use rustls;
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

#[cfg(feature = "email")]
pub mod email;

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
pub use random::{random_hex_token, random_hex_token_into};
pub use render::{NoTemplates, RenderError, Renderer, RendererFactory};
pub use request::Request;
pub use response::{Body, Response};
pub use server::socket_path_from_config;
pub use site::{
    finish_reload, genid_failed, genid_ok, CodeRoute, Handled, Reload, Site, GENID_PATH,
};

/// Everything a service normally wants, in one line.
///
/// `use m6_core::prelude::*;` is the intended first line of an m6 service.
/// m6-core is the box of blocks: linking it should be the only thing a new
/// service has to do to get routing, templating, a request dictionary, the
/// event-loop parts and the helpers that go with them. The loop is the
/// service's own, written out in its `main`.
///
/// ```rust,no_run
/// use std::time::Duration;
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
///     // Start: blocking and allocation are fine.
///     let site = Site::load("example", ROUTES, &[], &NoTemplates);
///     let mut poll = Poll::new()?;
///     let mut listener = listen_unix(site.socket_path(), site.socket_mode())?;
///     poll.registry()
///         .register(&mut listener, LISTENER, Interest::READABLE)?;
///     let mut conns = Slots::new(1024, 1024, || ServerConn::new(16 * 1024, 64 * 1024));
///     // `read_timeout_s = 0` means no deadline, which is the zero duration.
///     let read_timeout = site.read_timeout();
///     let mut deadlines = Timeouts::new(1024, read_timeout.unwrap_or(Duration::ZERO));
///     let mut events = Events::with_capacity(256);
///
///     // Run: `docs/m6-app-anatomy.md` section 2 owns the loop (the accept,
///     // the connection turn, the signals and the stop phase). This example
///     // shows the start only.
///     drop((poll, listener, conns, deadlines, events, site));
///     Ok(())
/// }
/// ```
///
/// The loop is one `match` on the event's token and one function per
/// connection. `docs/m6-app-anatomy.md` §2 shows it in full, with the accept,
/// the connection turn, the signals and the stop phase.
pub mod prelude {
    pub use crate::dict::Dict;
    pub use crate::error::{Error, Result};
    pub use crate::event::{LoopTrace, Slots, Timeouts};
    pub use crate::h1::{head_slots, Flush, Read, ServerConn};
    pub use crate::net::listen_unix;
    pub use crate::platform::{Events, Interest, Poll, Token};
    pub use crate::platform::{Signal, Signals};
    pub use crate::render::NoTemplates;
    pub use crate::request::Request;
    pub use crate::response::Response;
    pub use crate::site::{CodeRoute, Handled, RouteMethod, Site};
    pub use crate::util::{now_iso8601, slugify, today_iso8601};
    pub use serde_json::{json, Map, Value};

    #[cfg(feature = "email")]
    pub use crate::email::{Relay, RelayConfig, Smtp};
    #[cfg(feature = "email")]
    pub use lettre::Message;

    #[cfg(feature = "http-client")]
    pub use crate::client::Endpoint;

    #[cfg(feature = "multipart")]
    pub use crate::multipart::Upload;

    #[cfg(feature = "sqlite")]
    pub use rusqlite;

    #[cfg(feature = "templates")]
    pub use crate::template::{TeraFactory, TeraRenderer};
}
