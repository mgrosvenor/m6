//! The one thing the service loop needs from a templating crate.
//!
//! Core accepts the connection, parses the request, routes it, builds the
//! request dictionary, compresses, minifies and writes the response. It does
//! not render, and it does not link a template engine. A route that names a
//! template produces a `Response` carrying `template_name` and
//! `template_dict`, and this is the hand-off that turns that into bytes.
//!
//! `m6-html` supplies the Tera implementation. The three site renderers that
//! have no template files at all supply nothing, and stop linking Tera,
//! comrak, pest and the rest to get a server loop. That trade is the whole
//! reason this migration exists.

use std::path::Path;

use serde_json::{Map, Value};

/// What went wrong rendering a template.
///
/// The distinction is typed rather than sniffed out of an error string. A
/// template that wants to answer 404 (m6-html exposes `{{ not_found() }}` for
/// exactly this) used to signal it by failing with a magic substring,
/// `__M6_NOT_FOUND__`, which the service loop then looked for with
/// `msg.contains(..)`. That coupled the loop to one engine's error formatting
/// and would have turned any template legitimately containing that text into
/// a 404. The engine still needs its own sentinel internally, because Tera
/// errors are strings, but it converts here and core never sees it.
#[derive(Debug)]
pub enum RenderError {
    /// The template asked for a 404.
    NotFound,
    /// Anything else: a syntax error, a missing include, a filter that failed.
    Failed(anyhow::Error),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "template signalled not found"),
            Self::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

/// Turns a template name and a context into a rendered body.
///
/// One virtual call per templated response, on a path that is already doing
/// file I/O and compression.
pub trait Renderer: Send + Sync + 'static {
    fn render(&self, template: &str, ctx: &Map<String, Value>) -> Result<String, RenderError>;
}

/// Builds a `Renderer` for a site.
///
/// A trait rather than a value because the service loop rebuilds one on every
/// config reload, and a reload has to be able to pick up an edited template
/// without restarting the process.
pub trait RendererFactory: Send + Sync + 'static {
    /// `template_paths` is the set of templates named by config routes,
    /// relative to `site_dir`. Empty means "discover them", which is what a
    /// site with only code routes and a `templates/` directory wants.
    fn build(
        &self,
        site_dir: &Path,
        template_paths: &[String],
    ) -> anyhow::Result<Box<dyn Renderer>>;
}

/// The renderer for a service that has no templates.
///
/// It is an error rather than an empty string on purpose. A route configured
/// with a template in a binary that cannot render one is a misconfiguration,
/// and returning 500 with a reason in the log is how the operator finds out.
/// Silently serving an empty body is the failure mode that looks like health.
pub struct NoTemplates;

impl Renderer for NoTemplates {
    fn render(&self, template: &str, _ctx: &Map<String, Value>) -> Result<String, RenderError> {
        Err(RenderError::Failed(anyhow::anyhow!(
            "route asked for template `{template}`, but this binary links no template engine"
        )))
    }
}

impl RendererFactory for NoTemplates {
    fn build(&self, _site_dir: &Path, _paths: &[String]) -> anyhow::Result<Box<dyn Renderer>> {
        Ok(Box::new(NoTemplates))
    }
}
