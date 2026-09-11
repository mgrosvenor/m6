//! Re-export shim. `Response` lives in `m6-core` as of Phase 5; this keeps
//! `m6_render::response::*` working until Phase 6 moves consumers onto
//! `m6-core` directly.
pub use m6_core::response::{error_to_response, Response};
