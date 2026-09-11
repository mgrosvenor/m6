//! Re-export shim. `Request` and request parsing live in `m6-core` as of
//! Phase 5; this keeps `m6_render::request::*` working until Phase 6 moves
//! consumers onto `m6-core` directly.
pub use m6_core::request::{
    parse_auth_claims, parse_cookies, parse_form_body, parse_query_string, validate_path_param,
    RawRequest, Request,
};
