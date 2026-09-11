//! Re-export shim. The service loop lives in `m6-core` as of Phase 5.
//!
//! Only the constructors are defined here, and only to keep supplying Tera.
//! `App::new()` in core takes a renderer, because core does not have one;
//! `m6_render::App::new()` is documented to render Tera templates and the
//! three site renderers call it that way, so this hands core `TeraFactory`
//! and returns core's builder. Every builder method after that point, and
//! `run()` itself, is core's.
//!
//! Phase 6 deletes this file and the site renderers construct core's `App`
//! with the renderer they actually want, which for most of them is
//! `NoTemplates`.

use std::any::Any;

use serde_json::{Map, Value};

use m6_core::app::{AppWithGlobal, AppWithState, AppWithThreadState};
use m6_core::{Request, Response, Result};

pub use m6_core::app::{
    compile_pattern, find_route, is_shutdown, match_route, route_specificity, run_app,
    CompiledRoute, RouteMethod, Segment, ThreadPool,
};

/// Constructs `m6_core::app::App` preloaded with Tera.
///
/// A unit struct rather than a re-export: the whole job is to supply the
/// renderer argument that core's constructors now take.
pub struct App;

impl App {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> m6_core::app::App {
        m6_core::app::App::new(crate::template::TeraFactory)
    }

    pub fn with_global<G: Send + Sync + 'static>(
        init_global: impl Fn(&Map<String, Value>) -> Result<G> + Send + Sync + 'static,
    ) -> AppWithGlobal<G> {
        m6_core::app::App::with_global(crate::template::TeraFactory, init_global)
    }

    pub fn with_thread_state<T: Any + Send + 'static>(
        init_thread: impl Fn(&Map<String, Value>, &()) -> Result<T> + Send + Sync + 'static,
    ) -> AppWithThreadState<T> {
        m6_core::app::App::with_thread_state(crate::template::TeraFactory, init_thread)
    }

    pub fn with_state<G: Send + Sync + 'static, T: Any + Send + 'static>(
        init_global: impl Fn(&Map<String, Value>) -> Result<G> + Send + Sync + 'static,
        init_thread: impl Fn(&Map<String, Value>, &G) -> Result<T> + Send + Sync + 'static,
    ) -> AppWithState<G, T> {
        m6_core::app::App::with_state(crate::template::TeraFactory, init_global, init_thread)
    }
}

// Named so a handler signature written against the shim still resolves.
pub type Req = Request;
pub type Resp = Response;
