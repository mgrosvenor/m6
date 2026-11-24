use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

// ── Config ────────────────────────────────────────────────────────────────────

/// Read site.toml and return all fields as a JSON blob:
/// { "path": "/path/to/site.toml", "config": { "server": {...}, "route": [...], ... } }
pub fn config_read(site_toml: &Path) -> Value {
    let text = match fs::read_to_string(site_toml) {
        Ok(t) => t,
        Err(e) => return json!({ "error": format!("cannot read site.toml: {e}") }),
    };
    let toml_val: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("cannot parse site.toml: {e}") }),
    };
    let config = match serde_json::to_value(&toml_val) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("cannot convert to JSON: {e}") }),
    };
    let mtime = fs::metadata(site_toml)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    json!({ "path": site_toml.display().to_string(), "mtime": mtime, "config": config })
}

/// Accept a JSON object representing the full site.toml, convert back to TOML, validate,
/// and write atomically.  Returns Ok(()) or Err(message).
pub fn config_write(site_toml: &Path, json: &Value) -> Result<(), String> {
    let json = &strip_empty_strings(json.clone());
    let toml_val = json_to_toml_value(json)?;
    let content =
        toml::to_string_pretty(&toml_val).map_err(|e| format!("cannot serialise to TOML: {e}"))?;

    // Re-parse as a sanity check before writing.
    toml::from_str::<toml::Value>(&content).map_err(|e| format!("TOML round-trip failed: {e}"))?;

    // Atomic write via temp file + rename.
    let tmp = site_toml.with_extension("toml.tmp");
    fs::write(&tmp, &content).map_err(|e| format!("write failed: {e}"))?;
    fs::rename(&tmp, site_toml).map_err(|e| format!("rename failed: {e}"))?;
    Ok(())
}

/// Recursively remove empty-string values from objects.
/// This prevents optional TOML keys like `require = ""` from being written,
/// which m6-http would misinterpret as a set (but empty) auth requirement.
fn strip_empty_strings(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| v.as_str() != Some(""))
                .map(|(k, v)| (k, strip_empty_strings(v)))
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.into_iter().map(strip_empty_strings).collect()),
        other => other,
    }
}

/// Recursively convert a `serde_json::Value` to a `toml::Value`.
/// Returns Err if the JSON contains null (TOML has no null type).
fn json_to_toml_value(v: &Value) -> Result<toml::Value, String> {
    match v {
        Value::Null => Err("TOML does not support null values".into()),
        Value::Bool(b) => Ok(toml::Value::Boolean(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(toml::Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(toml::Value::Float(f))
            } else {
                Err(format!("unsupported number: {n}"))
            }
        }
        Value::String(s) => Ok(toml::Value::String(s.clone())),
        Value::Array(arr) => {
            let vals: Result<Vec<toml::Value>, _> = arr.iter().map(json_to_toml_value).collect();
            Ok(toml::Value::Array(vals?))
        }
        Value::Object(map) => {
            let mut table = toml::map::Map::new();
            for (k, val) in map {
                table.insert(k.clone(), json_to_toml_value(val)?);
            }
            Ok(toml::Value::Table(table))
        }
    }
}

// ── Service restart ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ServiceConfig {
    pub name: String,
    pub pid_file: String,
}

/// Send SIGTERM to a service using its PID file.
/// Returns { "ok": true, "pid": 1234 } or { "error": "..." }.
pub fn restart_service(svc: &ServiceConfig) -> Value {
    let pid_text = match fs::read_to_string(&svc.pid_file) {
        Ok(t) => t,
        Err(e) => return json!({ "error": format!("cannot read pid file {}: {e}", svc.pid_file) }),
    };

    let pid: u32 = match pid_text.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            return json!({ "error": format!("invalid pid in {}: '{}'", svc.pid_file, pid_text.trim()) })
        }
    };

    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();

    match status {
        Ok(s) if s.success() => json!({ "ok": true, "pid": pid }),
        Ok(s) => json!({ "error": format!("kill -TERM {pid} exited with {s}") }),
        Err(e) => json!({ "error": format!("kill -TERM {pid} failed: {e}") }),
    }
}
