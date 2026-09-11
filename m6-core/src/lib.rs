pub mod app;
pub mod compress;
pub mod conditional;
pub mod config;
pub mod error;
pub mod h1;
pub mod host;
pub mod http;
pub mod log;
pub mod mime;
pub mod monitoring;
pub mod minify;
pub mod ndjson;
pub mod negotiate;
pub mod parse;
pub mod path;
pub mod random;
pub mod render;
pub mod request;
pub mod response;
pub mod server;
pub mod signal;
pub mod telemetry;
pub mod util;
pub mod watcher;

/// Shared integration-test harness. Behind a feature so it is never compiled
/// into a production binary; enable it in `dev-dependencies` only.
#[cfg(feature = "testkit")]
pub mod testkit;

#[cfg(feature = "multipart")]
pub mod multipart;

/// Tera templating and the site filters that go with it. On by default;
/// see the `templates` feature in Cargo.toml.
#[cfg(feature = "templates")]
pub mod template;

#[cfg(feature = "templates")]
pub use template::{TeraFactory, TeraRenderer};

pub use conditional::{evaluate_preconditions, is_not_modified, not_modified_headers, Precondition};
pub use app::App;
pub use compress::{brotli_compress, brotli_decompress, gzip_compress, gzip_decompress};
pub use config::{RendererConfig, RouteConfig};
pub use error::{Error, Result};
pub use render::{NoTemplates, RenderError, Renderer, RendererFactory};
pub use h1::{parse_request, ParseResult};
pub use http::{header, is_same_origin_path, HeaderSource, RawRequest, RawResponse};
pub use mime::{mime_from_path, should_compress_default};
pub use negotiate::{canonical_coding, coding_quality, preferred_coding};
pub use path::validate_path_param;
pub use random::random_hex_token;
pub use request::Request;
pub use response::Response;
pub use server::{socket_path_from_config, UnixServer};
pub use signal::ShutdownHandle;
pub use watcher::ConfigWatcher;

/// Everything a service normally wants, in one line.
///
/// `use m6_core::prelude::*;` is the intended first line of an m6 service.
/// m6-core is the box of blocks: linking it should be the only thing a new
/// service has to do to get a server loop, routing, templating, a request
/// dictionary and the helpers that go with them.
///
/// ```rust,no_run
/// use m6_core::prelude::*;
///
/// fn main() -> Result<()> {
///     App::new()
///         .route("/blog/{stem}", |req| Response::render("templates/post.html", req))
///         .run()
/// }
/// ```
///
/// A service that renders nothing links the same crate and says so:
///
/// ```rust,no_run
/// use m6_core::prelude::*;
/// use m6_core::render::NoTemplates;
///
/// fn main() -> Result<()> {
///     App::new()
///         .renderer(NoTemplates)
///         .route("/healthz", |_req| Ok(Response::text("ok")))
///         .run()
/// }
/// ```
pub mod prelude {
    pub use crate::app::App;
    pub use crate::error::{Error, Result};
    pub use crate::request::Request;
    pub use crate::response::Response;
    pub use crate::util::{now_iso8601, slugify, today_iso8601};
    pub use serde_json::{json, Map, Value};

    #[cfg(feature = "email")]
    pub use lettre::{Message, SmtpTransport, Transport};

    #[cfg(feature = "http-client")]
    pub use ureq;

    #[cfg(feature = "multipart")]
    pub use crate::multipart::Upload;

    #[cfg(feature = "templates")]
    pub use crate::template::{TeraFactory, TeraRenderer};
}
