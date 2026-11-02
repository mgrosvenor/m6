/// Filesystem watcher for backend sockets.
///
/// Watches the directory the backend sockets live in and emits
/// `SocketCreated` and `SocketDeleted` events, so a worker that starts or stops
/// joins or leaves its pool at once.
///
/// This is all it watches. `site.toml`, the system config and the certificate
/// and key are not watched here. `m6_core::watcher::ConfigReloader` watches
/// both config files, and new TLS material is read when the system config is
/// touched, after the certificate and the key are both written.
///
/// On Linux: uses inotify on the socket directory.
/// On every other platform: `new()` returns an error and the 2-second rescan of
/// the backend socket globs finds new workers.
use std::os::unix::io::RawFd;
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone)]
pub enum FsEventKind {
    SocketCreated,
    SocketDeleted,
}

#[derive(Debug, Clone)]
pub struct FsEvent {
    pub path: PathBuf,
    pub kind: FsEventKind,
}

/// Filesystem watcher. Linux only; see the module documentation.
pub struct FsWatcher {
    #[cfg(target_os = "linux")]
    inotify: inotify::Inotify,
    #[cfg(target_os = "linux")]
    socket_dir: PathBuf,
}

impl FsWatcher {
    #[cfg(target_os = "linux")]
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        use inotify::{Inotify, WatchMask};

        let inotify = Inotify::init()?;

        // Determine socket directory from backend configs.
        let socket_dir = config
            .backends
            .iter()
            .filter_map(|b| b.sockets.as_ref())
            .filter_map(|g| std::path::Path::new(g).parent().map(|p| p.to_path_buf()))
            .next()
            .unwrap_or_else(|| PathBuf::from("/run/m6"));

        if socket_dir.exists() {
            inotify.watches().add(
                &socket_dir,
                WatchMask::CREATE | WatchMask::DELETE | WatchMask::MOVED_TO | WatchMask::MOVED_FROM,
            )?;
        }

        Ok(FsWatcher {
            inotify,
            socket_dir,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new(_config: &Config) -> anyhow::Result<Self> {
        Err(anyhow::anyhow!(
            "backend sockets are found by the 2-second rescan on this platform"
        ))
    }

    /// Return the raw file descriptor for polling, if available.
    pub fn raw_fd(&self) -> Option<RawFd> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            Some(self.inotify.as_raw_fd())
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// Read and return pending events.
    pub fn read_events(&mut self) -> Vec<FsEvent> {
        #[cfg(target_os = "linux")]
        {
            use inotify::EventMask;

            let mut buf = [0u8; 4096];
            let events = match self.inotify.read_events(&mut buf) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "inotify read error");
                    return vec![];
                }
            };

            let mut result = Vec::new();
            for event in events {
                let name = event
                    .name
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !name.ends_with(".sock") {
                    continue;
                }
                if event.mask.contains(EventMask::CREATE)
                    || event.mask.contains(EventMask::MOVED_TO)
                {
                    result.push(FsEvent {
                        path: self.socket_dir.join(&name),
                        kind: FsEventKind::SocketCreated,
                    });
                }
                if event.mask.contains(EventMask::DELETE)
                    || event.mask.contains(EventMask::MOVED_FROM)
                {
                    result.push(FsEvent {
                        path: self.socket_dir.join(&name),
                        kind: FsEventKind::SocketDeleted,
                    });
                }
            }
            result
        }
        #[cfg(not(target_os = "linux"))]
        {
            vec![]
        }
    }
}
