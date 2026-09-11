pub mod app;
pub mod compress;
pub mod conditional;
pub mod config;
pub mod error;
pub mod h1;
pub mod http;
pub mod log;
pub mod mime;
pub mod minify;
pub mod negotiate;
pub mod parse;
pub mod path;
pub mod random;
pub mod render;
pub mod request;
pub mod response;
pub mod server;
pub mod signal;
pub mod util;
pub mod watcher;

/// Shared integration-test harness. Behind a feature so it is never compiled
/// into a production binary; enable it in `dev-dependencies` only.
#[cfg(feature = "testkit")]
pub mod testkit;

#[cfg(feature = "multipart")]
pub mod multipart;

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
