/// Unix socket server for m6 inter-process communication.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::io::Write;

use anyhow::Result;
use tracing::{debug, error, warn};

use crate::http::{RawRequest, RawResponse};
use crate::parse::parse_request;

/// Derive the Unix socket path from a config file path.
///
/// Rule: basename, strip last extension, prepend /run/m6/
///
/// Examples:
///   "configs/m6-html.conf"   → "/run/m6/m6-html.sock"
///   "configs/m6-html-2.conf" → "/run/m6/m6-html-2.sock"
///   "/abs/path/to/foo.bar"   → "/run/m6/foo.sock"
pub fn socket_path_from_config(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("m6-default");
    PathBuf::from(format!("/run/m6/{}.sock", stem))
}

/// A running Unix socket server.
///
/// Accepts connections, calls handler for each, sends response.
pub struct UnixServer {
    path: PathBuf,
    listener: UnixListener,
}

impl UnixServer {
    /// Bind to the given socket path.  Removes a stale socket file first.
    pub fn bind(socket_path: PathBuf) -> Result<Self> {
        // Remove stale socket file if it exists.
        if socket_path.exists() {
            warn!(path = %socket_path.display(), "removing stale socket file");
            std::fs::remove_file(&socket_path)?;
        }

        // Ensure the directory exists.
        if let Some(parent) = socket_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let listener = UnixListener::bind(&socket_path)?;
        debug!(path = %socket_path.display(), "listening on Unix socket");

        Ok(UnixServer {
            path: socket_path,
            listener,
        })
    }

    /// Returns the socket path for logging.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept one connection, read request, call handler, write response.
    pub fn accept_one<F>(&self, handler: F) -> Result<()>
    where
        F: FnOnce(RawRequest) -> RawResponse,
    {
        let (mut stream, _addr) = self.listener.accept()?;
        handle_connection(&mut stream, handler);
        Ok(())
    }

    /// Returns the raw UnixListener for use in poll/select/epoll.
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

fn handle_connection<F>(stream: &mut UnixStream, handler: F)
where
    F: FnOnce(RawRequest) -> RawResponse,
{
    let req = match parse_request(stream) {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "failed to parse request");
            let resp = RawResponse::new(400).body(format!("Bad Request: {}", e));
            let _ = stream.write_all(&resp.to_bytes());
            return;
        }
    };

    let resp = handler(req);
    if let Err(e) = stream.write_all(&resp.to_bytes()) {
        error!(error = %e, "failed to write response");
    }
}

impl Drop for UnixServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_path_from_config_relative() {
        let p = socket_path_from_config(Path::new("configs/m6-html.conf"));
        assert_eq!(p, PathBuf::from("/run/m6/m6-html.sock"));
    }

    #[test]
    fn test_socket_path_from_config_double_extension() {
        let p = socket_path_from_config(Path::new("configs/m6-html-2.conf"));
        assert_eq!(p, PathBuf::from("/run/m6/m6-html-2.sock"));
    }

    #[test]
    fn test_socket_path_from_config_absolute() {
        let p = socket_path_from_config(Path::new("/abs/path/to/foo.bar"));
        assert_eq!(p, PathBuf::from("/run/m6/foo.sock"));
    }

    #[test]
    fn test_socket_path_from_config_no_extension() {
        let p = socket_path_from_config(Path::new("configs/myconfig"));
        assert_eq!(p, PathBuf::from("/run/m6/myconfig.sock"));
    }
}
