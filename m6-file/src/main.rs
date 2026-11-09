//! The static file service.
//!
//! An `App` service: one handler, registered by name, and every route it
//! answers comes from config.
//!
//! ```toml
//! [[route]]
//! path = "/assets/{*relpath}"
//! handler = "files"
//! root = "assets/"
//! ```
//!
//! This file used to be 400 lines: its own CLI parsing, its own logging setup,
//! its own socket bind and permissions, its own `poll(2)` accept loop, its own
//! worker pool over a channel, and its own graceful drain. Every one of those
//! had an equivalent in `m6_core::app`, and most had already been reconciled
//! from this side: `apply_read_timeout`, `apply_socket_mode`,
//! `socket_path_from_config` and `serve_connection` were each extracted from
//! here or shared with here.
//!
//! What kept it separate to the end was its route table, which comes from
//! config. `App` bound routes at the call site, so migrating would have
//! quietly removed the ability to add an asset tree without a restart. Config
//! routes naming a handler closed that, and this is what is left: routes come
//! from config, read once at start.

mod compress;
mod handler;

use m6_core::prelude::*;

fn main() -> anyhow::Result<()> {
    App::new().handler("files", handler::serve).run()?;
    Ok(())
}
