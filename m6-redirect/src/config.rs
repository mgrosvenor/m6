//! The `[redirect]` section of the service configuration.
//!
//! m6-core reserves the top-level keys in `docs/m6-app-anatomy.md` §4 and
//! hands a service everything else. The redirect engine reads its own settings
//! from one `[redirect]` table, all optional, so a config that says nothing
//! still serves the defaults.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The list file, in the unit's StateDirectory. The file is the one fixed
/// point the interface with the site's tool names: the tool POSTs a new list,
/// so nothing copies a file over SSH.
pub const DEFAULT_RULES_FILE: &str = "/var/lib/m6-redirect/rules.json";
/// Three months in seconds: how long a cache node may keep a redirect fresh.
/// The owner's default lifetime for a short link.
pub const DEFAULT_MAX_AGE_SECS: u64 = 7776000;
/// How long past freshness a cache may serve the held redirect while it
/// revalidates in the background (RFC 5861). The same window as `max_age_secs`
/// keeps a held link serving through an origin outage and until the owner's
/// explicit removal.
pub const DEFAULT_STALE_WHILE_REVALIDATE_SECS: u64 = 7776000;

/// The settings a start reads, and a `POST /genid` reload reads again.
pub struct Config {
    pub rules_file: PathBuf,
    pub max_age_secs: u64,
    pub stale_while_revalidate_secs: u64,
}

impl Config {
    /// Read the `[redirect]` table, or take every default when it is absent.
    ///
    /// A bad value stops the start with exit 2, the same way every other
    /// configuration refusal does. A relative `rules_file` resolves against
    /// the site directory, the rule m6-monitor's `history_file` follows.
    pub fn read(redirect: Option<&Value>, site_dir: &Path) -> Result<Config, String> {
        let Some(redirect) = redirect else {
            return Ok(Config::default());
        };
        let obj = redirect.as_object().ok_or("[redirect] must be a table")?;

        let rules_file = match obj.get("rules_file") {
            None => PathBuf::from(DEFAULT_RULES_FILE),
            Some(v) => {
                let s = v.as_str().ok_or("[redirect] rules_file must be a string")?;
                if s.trim().is_empty() {
                    return Err("[redirect] rules_file must be a non-empty path".to_string());
                }
                let path = PathBuf::from(s);
                if path.is_absolute() {
                    path
                } else {
                    site_dir.join(path)
                }
            }
        };

        let max_age_secs = match obj.get("max_age_secs") {
            None => DEFAULT_MAX_AGE_SECS,
            Some(v) => v
                .as_u64()
                .ok_or("[redirect] max_age_secs must be an integer number of seconds")?,
        };

        let stale_while_revalidate_secs = match obj.get("stale_while_revalidate_secs") {
            None => DEFAULT_STALE_WHILE_REVALIDATE_SECS,
            Some(v) => v.as_u64().ok_or(
                "[redirect] stale_while_revalidate_secs must be an integer number of seconds",
            )?,
        };

        Ok(Config {
            rules_file,
            max_age_secs,
            stale_while_revalidate_secs,
        })
    }

    /// The `Cache-Control` every redirect answer carries: `public` marks the
    /// response storable per the backend protocol, `s-maxage` gives the shared
    /// freshness lifetime, and `stale-while-revalidate` lets a cache serve the
    /// held redirect while it refreshes in the background.
    pub fn cache_control(&self) -> String {
        format!(
            "public, s-maxage={}, stale-while-revalidate={}",
            self.max_age_secs, self.stale_while_revalidate_secs
        )
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rules_file: PathBuf::from(DEFAULT_RULES_FILE),
            max_age_secs: DEFAULT_MAX_AGE_SECS,
            stale_while_revalidate_secs: DEFAULT_STALE_WHILE_REVALIDATE_SECS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(value: Value) -> Result<Config, String> {
        Config::read(Some(&value), Path::new("/srv/site"))
    }

    #[test]
    fn defaults_apply_when_the_table_is_absent() {
        let c = Config::read(None, Path::new("/srv/site")).unwrap();
        assert_eq!(c.rules_file, PathBuf::from(DEFAULT_RULES_FILE));
        assert_eq!(c.max_age_secs, DEFAULT_MAX_AGE_SECS);
        assert_eq!(
            c.stale_while_revalidate_secs,
            DEFAULT_STALE_WHILE_REVALIDATE_SECS
        );
    }

    #[test]
    fn an_empty_table_takes_every_default() {
        let c = read(json!({})).unwrap();
        assert_eq!(c.rules_file, PathBuf::from(DEFAULT_RULES_FILE));
    }

    #[test]
    fn a_relative_rules_file_resolves_against_the_site_dir() {
        let c = read(json!({"rules_file": "rules.json"})).unwrap();
        assert_eq!(c.rules_file, PathBuf::from("/srv/site/rules.json"));
    }

    #[test]
    fn an_absolute_rules_file_is_kept() {
        let c = read(json!({"rules_file": "/run/rules.json"})).unwrap();
        assert_eq!(c.rules_file, PathBuf::from("/run/rules.json"));
    }

    #[test]
    fn a_non_integer_max_age_is_refused() {
        assert!(read(json!({"max_age_secs": "forever"})).is_err());
    }

    #[test]
    fn a_non_table_redirect_is_refused() {
        assert!(Config::read(Some(&json!("x")), Path::new("/")).is_err());
    }

    #[test]
    fn the_cache_control_names_all_three_directives() {
        let c = Config::default();
        assert_eq!(
            c.cache_control(),
            "public, s-maxage=7776000, stale-while-revalidate=7776000"
        );
    }
}
