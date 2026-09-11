//! Re-export shim. Config loading lives in `m6-core` as of Phase 5; this keeps
//! `m6_render::config::*` working until Phase 6 moves consumers onto `m6-core`
//! directly.
//!
//! `socket_path_from_config` is deliberately not re-exported here. There were
//! two of them and they disagreed; `m6_core::socket_path_from_config` is the
//! one that survived.
pub use m6_core::config::{
    load, toml_to_json, CompressionLevel, LogConfig, MinificationConfig, ParamsCacheConfig,
    RendererConfig, RouteConfig, ThreadPoolConfig,
};
