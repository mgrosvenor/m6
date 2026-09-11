//! Re-export shim. Multipart body parsing lives in `m6-core` as of Phase 5;
//! it is request body parsing, not templating. This keeps
//! `m6_render::multipart::*` working until Phase 6 moves consumers onto
//! `m6-core` directly.
pub use m6_core::multipart::{parse_upload, Upload};
