/// m6-render: framework library for building renderer binaries.
///
/// # Quick start
///
/// ```rust,no_run
/// use m6_render::prelude::*;
///
/// fn main() -> Result<()> {
///     App::new()
///         .route("/blog/{stem}", |req| Response::render("templates/post.html", req))
///         .run()
/// }
/// ```

pub mod app;
pub mod compress;
pub mod config;
pub mod error;
pub mod minify;
pub mod request;
pub mod response;
pub mod server;
pub mod template;
pub mod util;

#[cfg(feature = "multipart")]
pub mod multipart;

pub use app::App;
pub use error::{Error, Result};
pub use request::Request;
pub use response::Response;

/// Prelude: bring the most commonly needed types into scope.
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
}
