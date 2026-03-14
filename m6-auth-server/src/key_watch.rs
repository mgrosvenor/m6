/// Key rotation watcher.
///
/// Monitors the private and public key files for changes and reloads
/// the JwtEngine when either file is modified.  On Linux this uses
/// inotify for prompt notification; on other platforms (macOS, etc.)
/// it falls back to polling every 5 seconds.
///
/// Rotation semantics (per spec):
///   m6-auth begins signing with the new key immediately.  Previously
///   issued access tokens expire naturally.  Refresh tokens are verified
///   against the *database* (hash match), not re-validated by JWT
///   signature, so old refresh tokens remain usable until they expire.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use tracing::{error, info};

#[cfg(target_os = "linux")]
use tracing::warn;

use crate::jwt::JwtEngine;

/// The hot-swappable key material.  Wrapped in Arc<RwLock<>> so the
/// watcher thread can replace it while request threads hold read locks.
pub struct KeyMaterial {
    pub jwt:            JwtEngine,
    pub public_key_pem: String,
}

impl KeyMaterial {
    pub fn load(private_key_path: &std::path::Path, public_key_path: &std::path::Path, issuer: String) -> anyhow::Result<Self> {
        let private_pem = std::fs::read_to_string(private_key_path)
            .map_err(|e| anyhow::anyhow!("cannot read private key {}: {}", private_key_path.display(), e))?;
        let public_pem = std::fs::read_to_string(public_key_path)
            .map_err(|e| anyhow::anyhow!("cannot read public key {}: {}", public_key_path.display(), e))?;

        let jwt = JwtEngine::new(&private_pem, &public_pem, issuer)?;
        Ok(KeyMaterial { jwt, public_key_pem: public_pem })
    }
}

/// Spawn a background thread that watches key files for changes and
/// reloads them into `keys` when a change is detected.
///
/// The thread runs until the process exits.
pub fn spawn_key_watcher(
    private_key_path: PathBuf,
    public_key_path: PathBuf,
    issuer: String,
    keys: Arc<RwLock<KeyMaterial>>,
) {
    std::thread::Builder::new()
        .name("key-watcher".into())
        .spawn(move || {
            watch_loop(&private_key_path, &public_key_path, &issuer, &keys);
        })
        .expect("spawn key-watcher thread");
}

fn watch_loop(
    private_key_path: &std::path::Path,
    public_key_path: &std::path::Path,
    issuer: &str,
    keys: &Arc<RwLock<KeyMaterial>>,
) {
    // On Linux use inotify (which falls back to polling if init fails).
    // On all other platforms use polling directly.
    #[cfg(target_os = "linux")]
    watch_loop_inotify(private_key_path, public_key_path, issuer, keys);

    #[cfg(not(target_os = "linux"))]
    watch_loop_poll(private_key_path, public_key_path, issuer, keys);
}

// ── Polling fallback (all platforms) ─────────────────────────────────────────
// On non-Linux platforms this is the primary watcher.
// On Linux it is used as a fallback when inotify initialisation fails.

fn watch_loop_poll(
    private_key_path: &std::path::Path,
    public_key_path: &std::path::Path,
    issuer: &str,
    keys: &Arc<RwLock<KeyMaterial>>,
) {
    let poll_interval = Duration::from_secs(5);

    let mut last_private = mtime(private_key_path);
    let mut last_public  = mtime(public_key_path);

    loop {
        std::thread::sleep(poll_interval);

        let cur_private = mtime(private_key_path);
        let cur_public  = mtime(public_key_path);

        let changed = cur_private != last_private || cur_public != last_public;

        if changed {
            reload(private_key_path, public_key_path, issuer, keys);
            last_private = mtime(private_key_path);
            last_public  = mtime(public_key_path);
        }
    }
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

// ── inotify (Linux) ──────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn watch_loop_inotify(
    private_key_path: &std::path::Path,
    public_key_path: &std::path::Path,
    issuer: &str,
    keys: &Arc<RwLock<KeyMaterial>>,
) {
    use std::os::unix::io::RawFd;

    // inotify_init1(IN_CLOEXEC)
    let inotify_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if inotify_fd < 0 {
        warn!("inotify_init1 failed; falling back to polling");
        watch_loop_poll(private_key_path, public_key_path, issuer, keys);
        return;
    }

    // Watch both key file *directories* for IN_CREATE | IN_MOVED_TO | IN_CLOSE_WRITE
    // This catches atomic rotation (write to temp, rename into place) as well as
    // direct overwrites.
    let mask = libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE;

    let watch_dir = |path: &std::path::Path| -> Result<RawFd, ()> {
        let dir = path.parent().unwrap_or(std::path::Path::new("/"));
        let c_path = match std::ffi::CString::new(dir.to_string_lossy().as_bytes()) {
            Ok(s) => s,
            Err(_) => return Err(()),
        };
        let wd = unsafe { libc::inotify_add_watch(inotify_fd, c_path.as_ptr(), mask) };
        if wd < 0 { Err(()) } else { Ok(wd) }
    };

    let _wd1 = watch_dir(private_key_path).unwrap_or_else(|_| {
        warn!(path = %private_key_path.display(), "inotify_add_watch failed for private key dir");
        -1
    });
    let _wd2 = watch_dir(public_key_path).unwrap_or_else(|_| {
        warn!(path = %public_key_path.display(), "inotify_add_watch failed for public key dir");
        -1
    });

    // Event buffer — large enough for several events
    let mut buf = vec![0u8; 4096];

    loop {
        // Blocking read; returns when at least one event is available.
        let n = unsafe {
            libc::read(inotify_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n < 0 {
            let errno = unsafe { *libc::__errno_location() };
            if errno == libc::EINTR {
                continue;
            }
            warn!(errno = errno, "inotify read error; falling back to polling");
            unsafe { libc::close(inotify_fd) };
            watch_loop_poll(private_key_path, public_key_path, issuer, keys);
            return;
        }

        // Parse events; check if either key file was affected.
        let mut offset = 0usize;
        let mut relevant = false;
        while offset + std::mem::size_of::<libc::inotify_event>() <= n as usize {
            let event = unsafe {
                &*(buf.as_ptr().add(offset) as *const libc::inotify_event)
            };
            let name_len = event.len as usize;
            let name_bytes = &buf[offset + std::mem::size_of::<libc::inotify_event>()
                               ..offset + std::mem::size_of::<libc::inotify_event>() + name_len];
            // Trim trailing NULs
            let name = std::ffi::CStr::from_bytes_until_nul(name_bytes)
                .ok()
                .and_then(|s| s.to_str().ok())
                .unwrap_or("");

            // Check whether the changed file matches our key filenames
            let priv_name = private_key_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let pub_name  = public_key_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name == priv_name || name == pub_name {
                relevant = true;
            }

            offset += std::mem::size_of::<libc::inotify_event>() + name_len;
        }

        if relevant {
            // Brief delay — if multiple files are being rotated in sequence,
            // wait for both to settle before reloading.
            std::thread::sleep(Duration::from_millis(200));
            reload(private_key_path, public_key_path, issuer, keys);
        }
    }
}

// ── Common reload logic ───────────────────────────────────────────────────────

fn reload(
    private_key_path: &std::path::Path,
    public_key_path: &std::path::Path,
    issuer: &str,
    keys: &Arc<RwLock<KeyMaterial>>,
) {
    match KeyMaterial::load(private_key_path, public_key_path, issuer.to_string()) {
        Ok(new_keys) => {
            match keys.write() {
                Ok(mut guard) => {
                    *guard = new_keys;
                    info!(
                        private_key = %private_key_path.display(),
                        public_key  = %public_key_path.display(),
                        "key rotation: new keys loaded"
                    );
                }
                Err(e) => {
                    error!(error = %e, "key rotation: RwLock poisoned; cannot update keys");
                }
            }
        }
        Err(e) => {
            error!(error = %e, "key rotation: failed to load new keys; continuing with current keys");
        }
    }
}
