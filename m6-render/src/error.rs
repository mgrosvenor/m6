//! Re-export shim. The error type lives in `m6-core` as of Phase 5; this keeps
//! `m6_render::error::{Error, Result}` working until Phase 6 moves consumers.
pub use m6_core::error::{Error, Result};
