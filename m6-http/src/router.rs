/// Route table and request matching.
use std::collections::HashMap;
use std::path::Path;

use matchit::Router as MatchitRouter;

use crate::config::Config;

/// Expand glob template variables for a matched file.
///
/// Supported variables (spec §`[[route_group]]`):
///   `{stem}`     — filename without extension
///   `{filename}` — filename with extension
///   `{relpath}`  — path relative to the non-wildcard prefix of the glob
///   `{dir}`      — directory of the matched file (relative to glob prefix)
fn expand_glob_vars(template: &str, file_path: &Path, glob_pattern: &str) -> String {
    let stem = file_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let filename = file_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let dir = file_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Compute the non-wildcard prefix of the glob pattern (up to first `*` or `?`).
    let glob_prefix = {
        let star = glob_pattern.find('*').unwrap_or(glob_pattern.len());
        let question = glob_pattern.find('?').unwrap_or(glob_pattern.len());
        let first_wild = star.min(question);
        // Go back to the last path separator before the first wildcard.
        let prefix_end = glob_pattern[..first_wild]
            .rfind('/')
            .map(|p| p + 1) // include the trailing slash
            .unwrap_or(0);
        &glob_pattern[..prefix_end]
    };

    // `{relpath}`: file path relative to glob prefix directory.
    let relpath = {
        let fp = file_path.to_string_lossy();
        if fp.starts_with(glob_prefix) {
            fp[glob_prefix.len()..].to_string()
        } else {
            fp.into_owned()
        }
    };

    // `{dir}`: directory of matched file, relative to glob prefix.
    let rel_dir = {
        let d = Path::new(&dir).to_string_lossy();
        if d.starts_with(glob_prefix) {
            d[glob_prefix.len()..].to_string()
        } else {
            d.into_owned()
        }
    };

    template
        .replace("{stem}", &stem)
        .replace("{filename}", &filename)
        .replace("{relpath}", &relpath)
        .replace("{dir}", &rel_dir)
}

#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub path: String,
    pub backend: String,
    pub require: Option<String>,
}

/// Built route table.
pub struct RouteTable {
    router: MatchitRouter<RouteEntry>,
    /// Ordered entries for specificity-based selection.
    entries: Vec<RouteEntry>,
}

impl RouteTable {
    /// Build a route table from config. Also expands route_groups.
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let mut router = MatchitRouter::new();
        let mut entries = Vec::new();

        // Add explicit routes
        for route in &config.routes {
            let entry = RouteEntry {
                path: route.path.clone(),
                backend: route.backend.clone(),
                require: route.require.clone(),
            };
            // matchit returns error on duplicate; we already validate in config
            if let Err(e) = router.insert(route.path.clone(), entry.clone()) {
                return Err(anyhow::anyhow!("router insert error for {}: {}", route.path, e));
            }
            entries.push(entry);
        }

        // Expand route_groups
        for rg in &config.route_groups {
            let glob_pattern = if std::path::Path::new(&rg.glob).is_absolute() {
                rg.glob.clone()
            } else {
                config.site_dir.join(&rg.glob).to_string_lossy().into_owned()
            };

            let paths = match glob::glob(&glob_pattern) {
                Ok(paths) => paths,
                Err(e) => {
                    tracing::warn!(pattern = %glob_pattern, error = %e, "route_group glob error");
                    continue;
                }
            };

            for path_result in paths {
                let file_path = match path_result {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, "route_group glob entry error");
                        continue;
                    }
                };

                let url_path = expand_glob_vars(&rg.path, &file_path, &glob_pattern);

                let entry = RouteEntry {
                    path: url_path.clone(),
                    backend: rg.backend.clone(),
                    require: rg.require.clone(),
                };

                // Skip if already added (explicit route takes precedence)
                let already_exists = router.at(&url_path).is_ok();
                if already_exists {
                    continue;
                }

                if let Err(e) = router.insert(url_path.clone(), entry.clone()) {
                    tracing::warn!(path = %url_path, error = %e, "route_group insert error");
                    continue;
                }
                entries.push(entry);
            }
        }

        Ok(RouteTable { router, entries })
    }

    /// Match a request path. Returns the matched RouteEntry if found.
    pub fn at<'a>(&'a self, path: &str) -> Option<&'a RouteEntry> {
        self.router.at(path).ok().map(|m| m.value)
    }

    /// Number of routes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Build invalidation map from two sources:
///
/// 1. `[[route_group]]` globs: `file_path → [url_path, ...]`
/// 2. Renderer config `params` declarations: `params_file → [url_path, ...]`
///
/// The map key is the filesystem path (or glob pattern for templated params).
/// The map value is the list of URL paths that should be evicted when that
/// file changes.
pub fn build_invalidation_map(config: &Config) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    // ── Source 1: [[route_group]] globs ──────────────────────────────────────
    for rg in &config.route_groups {
        let glob_pattern = if std::path::Path::new(&rg.glob).is_absolute() {
            rg.glob.clone()
        } else {
            config.site_dir.join(&rg.glob).to_string_lossy().into_owned()
        };

        let paths = match glob::glob(&glob_pattern) {
            Ok(paths) => paths,
            Err(_) => continue,
        };

        for path_result in paths.flatten() {
            let url_path = expand_glob_vars(&rg.path, &path_result, &glob_pattern);
            map.entry(path_result.to_string_lossy().into_owned())
                .or_default()
                .push(url_path);
        }
    }

    // ── Source 2: renderer config `params` declarations ───────────────────────
    // For each socket-based backend, look for a renderer config at
    // `<site_dir>/configs/<backend_name>.conf` and parse its [[route]] entries.
    for backend in &config.backends {
        if backend.sockets.is_none() {
            continue; // URL backends don't have renderer configs
        }
        let conf_path = config.site_dir.join("configs").join(format!("{}.conf", backend.name));
        if !conf_path.exists() {
            continue;
        }
        add_renderer_params_to_map(&conf_path, &config.site_dir, &mut map);
    }

    map
}

/// Parse a renderer config file and add `params_file → url_paths` entries to `map`.
///
/// For static params files (no `{stem}` template in either the params path or
/// the route path) the exact resolved file path is mapped to the exact URL path.
///
/// For templated params files (path contains `{stem}`) the glob pattern
/// (e.g. `content/posts/*.json`) is used as the map key and the route path
/// template (e.g. `/blog/{stem}`) is stored as the value with the note that
/// the caller must expand it on eviction.  For simplicity we store the pattern
/// as-is and the eviction code uses glob-prefix matching.
fn add_renderer_params_to_map(
    conf_path: &Path,
    site_dir: &Path,
    map: &mut HashMap<String, Vec<String>>,
) {
    let raw = match std::fs::read_to_string(conf_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                config = %conf_path.display(),
                error = %e,
                "invalidation map: could not read renderer config"
            );
            return;
        }
    };

    let tv: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                config = %conf_path.display(),
                error = %e,
                "invalidation map: could not parse renderer config"
            );
            return;
        }
    };

    let routes = match tv.get("route").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return,
    };

    for route_val in routes {
        let url_path = match route_val.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => continue,
        };

        let params: Vec<String> = route_val
            .get("params")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        for params_path_tmpl in &params {
            // Is the params path templated (contains `{stem}`)?
            if params_path_tmpl.contains('{') {
                // Replace `{stem}` (and similar) with `*` to get a glob pattern.
                let params_glob = params_path_tmpl
                    .replace("{stem}", "*")
                    .replace("{filename}", "*")
                    .replace("{relpath}", "**/*")
                    .replace("{dir}", "*");
                // Resolve relative to site_dir.
                let resolved_glob = if std::path::Path::new(&params_glob).is_absolute() {
                    params_glob.clone()
                } else {
                    site_dir.join(&params_glob).to_string_lossy().into_owned()
                };
                map.entry(resolved_glob)
                    .or_default()
                    .push(url_path.clone());
            } else {
                // Static params file — resolve relative to site_dir.
                let resolved = if std::path::Path::new(params_path_tmpl).is_absolute() {
                    params_path_tmpl.clone()
                } else {
                    site_dir.join(params_path_tmpl).to_string_lossy().into_owned()
                };
                map.entry(resolved)
                    .or_default()
                    .push(url_path.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendConfig, Config, ErrorsConfig, LogConfig, RouteConfig, ServerConfig, SiteConfig};
    use std::path::PathBuf;

    fn make_config(routes: Vec<RouteConfig>, backends: Vec<BackendConfig>) -> Config {
        Config {
            site: SiteConfig { name: "Test".to_string(), domain: "test.example.com".to_string() },
            server: ServerConfig {
                bind: "127.0.0.1:8443".to_string(),
                tls_cert: "/tmp/cert.pem".to_string(),
                tls_key: "/tmp/key.pem".to_string(),
                backend_timeout_secs: 30,
            },
            log: LogConfig::default(),
            errors: ErrorsConfig::default(),
            auth: None,
            backends,
            routes,
            route_groups: vec![],
            site_dir: PathBuf::from("/tmp"),
        }
    }

    fn make_backend(name: &str) -> BackendConfig {
        BackendConfig {
            name: name.to_string(),
            sockets: Some("/run/m6/*.sock".to_string()),
            url: None,
        }
    }

    fn make_route(path: &str, backend: &str) -> RouteConfig {
        RouteConfig { path: path.to_string(), backend: backend.to_string(), require: None }
    }

    #[test]
    fn test_exact_route_matched() {
        let config = make_config(
            vec![make_route("/", "b"), make_route("/about", "b")],
            vec![make_backend("b")],
        );
        let table = RouteTable::from_config(&config).unwrap();
        let entry = table.at("/about").unwrap();
        assert_eq!(entry.backend, "b");
        assert_eq!(entry.path, "/about");
    }

    #[test]
    fn test_parameterized_route() {
        let config = make_config(
            vec![make_route("/admin/{page}", "b")],
            vec![make_backend("b")],
        );
        let table = RouteTable::from_config(&config).unwrap();
        assert!(table.at("/admin/dashboard").is_some());
        assert!(table.at("/admin/settings").is_some());
        assert!(table.at("/other").is_none());
    }

    #[test]
    fn test_no_match_returns_none() {
        let config = make_config(vec![make_route("/", "b")], vec![make_backend("b")]);
        let table = RouteTable::from_config(&config).unwrap();
        assert!(table.at("/nonexistent").is_none());
    }

    #[test]
    fn test_exact_beats_parameterized() {
        let config = make_config(
            vec![make_route("/admin/special", "exact-backend"), make_route("/admin/{page}", "param-backend")],
            vec![make_backend("exact-backend"), make_backend("param-backend")],
        );
        let table = RouteTable::from_config(&config).unwrap();
        // matchit handles this: exact routes beat parameterized
        let entry = table.at("/admin/special").unwrap();
        assert_eq!(entry.backend, "exact-backend");
    }

    #[test]
    fn test_expand_glob_vars_stem() {
        let path = std::path::Path::new("/site/content/posts/hello-world.json");
        let result = expand_glob_vars("/blog/{stem}", path, "/site/content/posts/*.json");
        assert_eq!(result, "/blog/hello-world");
    }

    #[test]
    fn test_expand_glob_vars_filename() {
        let path = std::path::Path::new("/site/assets/style.css");
        let result = expand_glob_vars("/assets/{filename}", path, "/site/assets/*");
        assert_eq!(result, "/assets/style.css");
    }

    #[test]
    fn test_expand_glob_vars_relpath() {
        let path = std::path::Path::new("/site/assets/css/main.css");
        let result = expand_glob_vars("/assets/{relpath}", path, "/site/assets/**/*");
        assert_eq!(result, "/assets/css/main.css");
    }

    #[test]
    fn test_expand_glob_vars_dir() {
        let path = std::path::Path::new("/site/assets/css/main.css");
        let result = expand_glob_vars("/files/{dir}/{filename}", path, "/site/assets/**/*");
        assert_eq!(result, "/files/css/main.css");
    }

    // ── Invalidation map second source (renderer config params) ──────────────

    #[test]
    fn test_invalidation_map_renderer_config_static_params() {
        use tempfile::TempDir;
        use std::io::Write;

        let dir = TempDir::new().unwrap();
        let site_dir = dir.path();

        // Create configs/ directory and renderer config
        std::fs::create_dir_all(site_dir.join("configs")).unwrap();
        let conf_content = r#"
[[route]]
path = "/about"
template = "templates/about.html"
params = ["data/about.json"]

[[route]]
path = "/home"
template = "templates/home.html"
params = ["data/home.json", "data/shared.json"]
"#;
        std::fs::write(site_dir.join("configs").join("m6-html.conf"), conf_content).unwrap();

        // Create dummy TLS files
        std::fs::write(site_dir.join("cert.pem"), "dummy").unwrap();
        std::fs::write(site_dir.join("key.pem"), "dummy").unwrap();

        let config = Config {
            site: SiteConfig { name: "Test".to_string(), domain: "test.example.com".to_string() },
            server: ServerConfig {
                bind: "127.0.0.1:8443".to_string(),
                tls_cert: site_dir.join("cert.pem").to_string_lossy().into_owned(),
                tls_key: site_dir.join("key.pem").to_string_lossy().into_owned(),
                backend_timeout_secs: 30,
            },
            log: LogConfig::default(),
            errors: ErrorsConfig::default(),
            auth: None,
            backends: vec![BackendConfig {
                name: "m6-html".to_string(),
                sockets: Some("/run/m6/m6-html-*.sock".to_string()),
                url: None,
            }],
            routes: vec![],
            route_groups: vec![],
            site_dir: site_dir.to_path_buf(),
        };

        let map = build_invalidation_map(&config);

        // data/about.json → ["/about"]
        let about_key = site_dir.join("data/about.json").to_string_lossy().into_owned();
        assert!(map.contains_key(&about_key), "about.json key missing");
        assert!(map[&about_key].contains(&"/about".to_string()));

        // data/home.json → ["/home"]
        let home_key = site_dir.join("data/home.json").to_string_lossy().into_owned();
        assert!(map.contains_key(&home_key), "home.json key missing");
        assert!(map[&home_key].contains(&"/home".to_string()));

        // data/shared.json → ["/home"]
        let shared_key = site_dir.join("data/shared.json").to_string_lossy().into_owned();
        assert!(map.contains_key(&shared_key), "shared.json key missing");
        assert!(map[&shared_key].contains(&"/home".to_string()));
    }

    #[test]
    fn test_invalidation_map_renderer_config_templated_params() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let site_dir = dir.path();

        std::fs::create_dir_all(site_dir.join("configs")).unwrap();
        let conf_content = r#"
[[route]]
path = "/blog/{stem}"
template = "templates/post.html"
params = ["content/posts/{stem}.json"]
"#;
        std::fs::write(site_dir.join("configs").join("m6-html.conf"), conf_content).unwrap();
        std::fs::write(site_dir.join("cert.pem"), "dummy").unwrap();
        std::fs::write(site_dir.join("key.pem"), "dummy").unwrap();

        let config = Config {
            site: SiteConfig { name: "Test".to_string(), domain: "test.example.com".to_string() },
            server: ServerConfig {
                bind: "127.0.0.1:8443".to_string(),
                tls_cert: site_dir.join("cert.pem").to_string_lossy().into_owned(),
                tls_key: site_dir.join("key.pem").to_string_lossy().into_owned(),
                backend_timeout_secs: 30,
            },
            log: LogConfig::default(),
            errors: ErrorsConfig::default(),
            auth: None,
            backends: vec![BackendConfig {
                name: "m6-html".to_string(),
                sockets: Some("/run/m6/m6-html-*.sock".to_string()),
                url: None,
            }],
            routes: vec![],
            route_groups: vec![],
            site_dir: site_dir.to_path_buf(),
        };

        let map = build_invalidation_map(&config);

        // Templated params: key is a glob pattern with `*` substituted for `{stem}`
        let glob_key = site_dir.join("content/posts/*.json").to_string_lossy().into_owned();
        assert!(map.contains_key(&glob_key), "templated glob key missing: {}", glob_key);
        assert!(map[&glob_key].contains(&"/blog/{stem}".to_string()));
    }

    #[test]
    fn test_invalidation_map_url_backend_skipped() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let site_dir = dir.path();
        // No configs/ directory created — URL backends should be silently skipped
        std::fs::write(site_dir.join("cert.pem"), "dummy").unwrap();
        std::fs::write(site_dir.join("key.pem"), "dummy").unwrap();

        let config = Config {
            site: SiteConfig { name: "Test".to_string(), domain: "test.example.com".to_string() },
            server: ServerConfig {
                bind: "127.0.0.1:8443".to_string(),
                tls_cert: site_dir.join("cert.pem").to_string_lossy().into_owned(),
                tls_key: site_dir.join("key.pem").to_string_lossy().into_owned(),
                backend_timeout_secs: 30,
            },
            log: LogConfig::default(),
            errors: ErrorsConfig::default(),
            auth: None,
            backends: vec![BackendConfig {
                name: "external".to_string(),
                sockets: None,
                url: Some("https://api.example.com".to_string()),
            }],
            routes: vec![],
            route_groups: vec![],
            site_dir: site_dir.to_path_buf(),
        };

        // Should not panic or error — URL backends are simply skipped
        let map = build_invalidation_map(&config);
        assert!(map.is_empty());
    }
}
