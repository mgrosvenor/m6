pub mod compress;
pub mod http;
pub mod log;
pub mod mime;
pub mod minify;
pub mod negotiate;
pub mod parse;
pub mod path;
pub mod random;
pub mod server;
pub mod signal;
pub mod watcher;

/// Shared integration-test harness. Behind a feature so it is never compiled
/// into a production binary; enable it in `dev-dependencies` only.
#[cfg(feature = "testkit")]
pub mod testkit;

pub use compress::{brotli_compress, brotli_decompress, gzip_compress, gzip_decompress};
pub use http::{header, is_same_origin_path, HeaderSource, RawRequest, RawResponse};
pub use mime::{mime_from_path, should_compress_default};
pub use negotiate::{canonical_coding, coding_quality, preferred_coding};
pub use path::validate_path_param;
pub use random::random_hex_token;
pub use server::{socket_path_from_config, UnixServer};
pub use signal::ShutdownHandle;
pub use watcher::ConfigWatcher;
