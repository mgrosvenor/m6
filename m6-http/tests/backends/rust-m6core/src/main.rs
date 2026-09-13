//! An m6 backend in Rust **on `m6-core`**, written from
//! `docs/m6-backend-protocol.md`.
//!
//! Usage: `m6-example-rust-m6core <site-dir> <config-path>`
//!
//! Note the invocation: it is not the `<socket-path> <status-json-path>` of the
//! other five. A core service is configured rather than argument-driven, which
//! is `parse_invocation`'s contract, and the socket path comes from the config
//! file or `M6_SOCKET_OVERRIDE`. That difference is itself part of what this
//! example shows.
//!
//! # Read this beside `rust-plain/src/main.rs`
//!
//! That file and this one answer the same five routes with the same bytes. The
//! difference between them is the whole point of the pair, and it is the
//! measurement the phase exists for: same language, same compiler, same payload,
//! so the delta between the two binaries is precisely what linking `m6-core`
//! costs. Per `docs/m6-backend-examples.md` §5.3 that is the genuinely
//! informative comparison in the set, and if it is not close to zero then core
//! has a problem worth knowing about.
//!
//! What the pair file writes by hand and this one does not write at all:
//!
//! - the binding sequence of spec §1.2, including the `chmod 0666` that is the
//!   single most common cause of a backend that starts and is never contacted
//! - `signal(2)` handling, the shutdown flag, and the unlink on the way out
//! - accepting connections and a thread per connection
//! - parsing a request line and header section by hand, and draining exactly
//!   `Content-Length` bytes
//! - writing a status line, `Content-Type` and an accurate `Content-Length`
//! - suppressing the body on HEAD, and on 1xx, 204 and 304
//!
//! What is left is the part that is actually this service: four routes and their
//! bodies. That is the shape the architectural rule asks for, and the reason
//! the rule is phrased as "a service being nearly a no-op on top of core is the
//! goal, not a smell".

use m6_core::prelude::*;

const LANGUAGE: &str = "Rust (m6-core)";

fn main() -> Result<()> {
    // The payload comes from the one copy in the repository, the same file the
    // other five examples read, so byte-identity across all six is structural
    // rather than asserted. The site directory is where a core service's own
    // files live, and `parse_invocation` has already established it as argv[1].
    let site_dir = std::env::args().nth(1).unwrap_or_default();
    let status_path = std::path::Path::new(&site_dir).join("status.json");
    let status_body = match std::fs::read(&status_path) {
        Ok(b) => b,
        Err(e) => {
            // Exit 2 means "failed before binding", so a supervisor can tell a
            // misconfiguration from a crash (spec §8.3). Core exits 2 for a bad
            // invocation; this is the same contract for a missing payload.
            eprintln!("cannot read status payload {}: {e}", status_path.display());
            std::process::exit(2);
        }
    };

    App::new()
        .route_get("/", |_req| {
            Ok(Response::html(html_page(
                &format!("m6 backend example: {LANGUAGE}"),
                &format!("A minimal m6 backend written in {LANGUAGE}."),
            )))
        })
        .route_get("/status", move |_req| {
            // `.verbatim()` rather than letting the pipeline have it, and not
            // `Response::json`, for two separate reasons:
            //
            // - `Response::json` would re-serialise a parsed value, and the
            //   whole point of this route is that all six examples emit the
            //   same bytes. Serde would be free to order or space them
            //   differently.
            // - verbatim stops core minifying and compressing the body. Spec
            //   §3.6 says a backend SHOULD NOT compress, because the proxy
            //   negotiates and caches each representation itself. It also keeps
            //   the measurement honest: compressing here and not in the pair
            //   would make the delta a story about brotli rather than about
            //   core.
            Ok(Response::status(200)
                .body(status_body.clone())
                .header("content-type", "application/json; charset=utf-8")
                .verbatim())
        })
        .route_get("/health", |_req| Ok(Response::text("ok")))
        .route_get("/boom", |_req| {
            // Fails on purpose. The proxy counts 5xx as a backend error and may
            // replace it with a styled error page (spec §4).
            // `.verbatim()` here too. Without it core minifies the HTML, which
            // is core doing its job correctly but makes this example's body 167
            // bytes where the other five send 178. The examples are meant to be
            // readable side by side, so the one that is allowed to differ is
            // /status, and that one differs in nothing at all.
            Ok(Response::status(500)
                .body(html_page(
                    "500 Internal Server Error",
                    "This endpoint fails on purpose.",
                ))
                .header("content-type", "text/html; charset=utf-8")
                .verbatim())
        })
        // Core answers an unmatched path with a bare 404 and no body, which
        // satisfies the protocol but not docs/m6-backend-examples.md §3, where
        // every example serves "a small not-found page". A catch-all wildcard
        // gives the same 404 body as the other five. It is registered last so
        // the four specific routes win; `{*any}` captures a segment and every
        // one after it.
        .route_get("/{*any}", |_req| {
            Ok(Response::status(404)
                .body(html_page(
                    "404 Not Found",
                    &format!("This {LANGUAGE} backend does not serve that path."),
                ))
                .header("content-type", "text/html; charset=utf-8")
                .verbatim())
        })
        .run()
}

fn html_page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\">\n<title>{title}</title>\n<h1>{title}</h1>\n<p>{detail}</p>\n</html>\n"
    )
}
