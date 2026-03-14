/// Platform-abstracted filesystem watcher for hot reload.
///
/// On Linux: uses inotify.
/// On other platforms: returns an error from `new()`, hot reload disabled.

use std::os::unix::io::RawFd;
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone)]
pub enum FsEventKind {
    SocketCreated,
    SocketDeleted,
    SiteTomlChanged,
    /// TLS certificate or key file changed — caller should reload TLS config.
    TlsCertChanged,
}

#[derive(Debug, Clone)]
pub struct FsEvent {
    pub path: PathBuf,
    pub kind: FsEventKind,
}

/// Filesystem watcher. Platform-specific implementation below.
pub struct FsWatcher {
    #[allow(dead_code)]
    inner: FsWatcherInner,
}

#[cfg(target_os = "linux")]
struct FsWatcherInner {
    inotify: inotify::Inotify,
    /// Filenames (not full paths) of TLS cert and key — watched for changes.
    tls_filenames: Vec<String>,
}

#[cfg(not(target_os = "linux"))]
struct FsWatcherInner {
    // No-op on non-Linux
}

impl FsWatcher {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use inotify::{Inotify, WatchMask};
            use std::collections::HashSet;

            let mut inotify = Inotify::init()?;

            // Watch site.toml's parent directory for changes to site.toml
            let site_dir = &config.site_dir;
            if site_dir.exists() {
                inotify.watches().add(
                    site_dir,
                    WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE,
                )?;
            }

            // Watch /run/m6/ if it exists
            let run_m6 = std::path::Path::new("/run/m6");
            if run_m6.exists() {
                inotify.watches().add(
                    run_m6,
                    WatchMask::CREATE
                        | WatchMask::DELETE
                        | WatchMask::MOVED_TO
                        | WatchMask::MOVED_FROM,
                )?;
            }

            // Watch parent directories of TLS cert and key files for changes.
            let mut tls_filenames: Vec<String> = Vec::new();
            let tls_paths = [&config.server.tls_cert, &config.server.tls_key];
            let mut watched_dirs: HashSet<std::path::PathBuf> = HashSet::new();

            for tls_path_str in &tls_paths {
                let tls_path = std::path::Path::new(tls_path_str);
                if let Some(fname) = tls_path.file_name() {
                    tls_filenames.push(fname.to_string_lossy().into_owned());
                }
                if let Some(parent) = tls_path.parent() {
                    if parent.exists() && !watched_dirs.contains(parent) {
                        let _ = inotify.watches().add(
                            parent,
                            WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE,
                        );
                        watched_dirs.insert(parent.to_path_buf());
                    }
                }
            }

            Ok(FsWatcher { inner: FsWatcherInner { inotify, tls_filenames } })
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = config;
            Err(anyhow::anyhow!("inotify not available on this platform"))
        }
    }

    /// Return the raw file descriptor for polling, if available.
    pub fn raw_fd(&self) -> Option<RawFd> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            Some(self.inner.inotify.as_raw_fd())
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
            let events = match self.inner.inotify.read_events(&mut buf) {
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

                if event.mask.contains(EventMask::CREATE)
                    || event.mask.contains(EventMask::MOVED_TO)
                {
                    if name.ends_with(".sock") {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from("/run/m6").join(&name),
                            kind: FsEventKind::SocketCreated,
                        });
                    }
                    if name == "site.toml" {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from("site.toml"),
                            kind: FsEventKind::SiteTomlChanged,
                        });
                    }
                    if self.inner.tls_filenames.iter().any(|f| f == &name) {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from(&name),
                            kind: FsEventKind::TlsCertChanged,
                        });
                    }
                }

                if event.mask.contains(EventMask::DELETE)
                    || event.mask.contains(EventMask::MOVED_FROM)
                {
                    if name.ends_with(".sock") {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from("/run/m6").join(&name),
                            kind: FsEventKind::SocketDeleted,
                        });
                    }
                }

                if event.mask.contains(EventMask::CLOSE_WRITE) {
                    if name == "site.toml" || name.is_empty() {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from("site.toml"),
                            kind: FsEventKind::SiteTomlChanged,
                        });
                    }
                    if self.inner.tls_filenames.iter().any(|f| f == &name) {
                        result.push(FsEvent {
                            path: std::path::PathBuf::from(&name),
                            kind: FsEventKind::TlsCertChanged,
                        });
                    }
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
