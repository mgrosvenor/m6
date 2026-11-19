pub mod bench;
pub mod logs;
pub mod ops;
pub mod perf;
pub mod system;

/// The `[server].bind` m6-http listens on. It lives in the system config, not
/// site.toml, so the dashboard reads it from there; `None` when the file or key is absent.
pub fn server_bind(system_toml: &std::path::Path) -> Option<String> {
    let parsed: toml::Value = std::fs::read_to_string(system_toml).ok()?.parse().ok()?;
    parsed
        .get("server")?
        .get("bind")?
        .as_str()
        .map(str::to_owned)
}
