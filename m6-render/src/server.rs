//! Re-export shim. These live in `m6-core` as of Phase 5, next to the one
//! parser and the one response writer they delegate to. This keeps
//! `m6_render::server::*` working until Phase 6 moves consumers onto
//! `m6-core` directly.
pub use m6_core::server::{parse_request, write_error_response, write_response};
