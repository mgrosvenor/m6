/// Configuration utilities: TOML loading and JSON Map operations.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};

/// Load a TOML file and deserialize into a JSON `Map`.
///
/// All TOML types are converted to equivalent JSON types.
pub fn load_toml(path: &Path) -> Result<Map<String, Value>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file: {}", path.display()))?;

    let toml_value: toml::Value = text
        .parse()
        .with_context(|| format!("parsing TOML in: {}", path.display()))?;

    let json_value = toml_to_json(toml_value);

    match json_value {
        Value::Object(map) => Ok(map),
        _ => Err(anyhow!(
            "config file did not contain a TOML table at top level: {}",
            path.display()
        )),
    }
}

/// Recursively convert a `toml::Value` to a `serde_json::Value`.
fn toml_to_json(val: toml::Value) -> Value {
    match val {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::Number(i.into()),
        toml::Value::Float(f) => {
            serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::Null)
        }
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Array(arr) => {
            Value::Array(arr.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => {
            let map = table
                .into_iter()
                .map(|(k, v)| (k, toml_to_json(v)))
                .collect();
            Value::Object(map)
        }
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
    }
}

/// Merge two JSON `Map`s.  Values from `overlay` overwrite `base`.
/// Nested objects are merged recursively.
pub fn merge_maps(
    mut base: Map<String, Value>,
    overlay: Map<String, Value>,
) -> Map<String, Value> {
    for (key, overlay_val) in overlay {
        match (base.get_mut(&key), overlay_val) {
            (Some(Value::Object(base_map)), Value::Object(overlay_map)) => {
                // Recursive merge for nested maps.
                let merged = merge_maps(
                    std::mem::take(base_map),
                    overlay_map,
                );
                *base_map = merged;
            }
            (_, val) => {
                base.insert(key, val);
            }
        }
    }
    base
}

/// Return a `&str` for a required key, or a clear error.
pub fn require_str<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    match map.get(key) {
        None => Err(anyhow!("missing required config key: {}", key)),
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(other) => Err(anyhow!(
            "config key '{}' must be a string, got {}",
            key,
            other
        )),
    }
}

/// Return an `i64` for a required key, or a clear error.
pub fn require_i64(map: &Map<String, Value>, key: &str) -> Result<i64> {
    match map.get(key) {
        None => Err(anyhow!("missing required config key: {}", key)),
        Some(Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| anyhow!("config key '{}' could not be represented as i64", key)),
        Some(other) => Err(anyhow!(
            "config key '{}' must be a number, got {}",
            key,
            other
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("not an object"),
        }
    }

    #[test]
    fn test_merge_maps_overlay_wins() {
        let base = map(json!({"a": 1, "b": 2}));
        let overlay = map(json!({"b": 99, "c": 3}));
        let result = merge_maps(base, overlay);
        assert_eq!(result["a"], json!(1));
        assert_eq!(result["b"], json!(99));
        assert_eq!(result["c"], json!(3));
    }

    #[test]
    fn test_merge_maps_recursive() {
        let base = map(json!({"server": {"host": "localhost", "port": 8080}}));
        let overlay = map(json!({"server": {"port": 9090}}));
        let result = merge_maps(base, overlay);
        assert_eq!(result["server"]["host"], json!("localhost"));
        assert_eq!(result["server"]["port"], json!(9090));
    }

    #[test]
    fn test_require_str_present() {
        let m = map(json!({"key": "value"}));
        assert_eq!(require_str(&m, "key").unwrap(), "value");
    }

    #[test]
    fn test_require_str_missing() {
        let m = map(json!({}));
        assert!(require_str(&m, "key").is_err());
    }

    #[test]
    fn test_require_str_wrong_type() {
        let m = map(json!({"key": 42}));
        assert!(require_str(&m, "key").is_err());
    }

    #[test]
    fn test_require_i64_present() {
        let m = map(json!({"port": 8080}));
        assert_eq!(require_i64(&m, "port").unwrap(), 8080);
    }

    #[test]
    fn test_require_i64_missing() {
        let m = map(json!({}));
        assert!(require_i64(&m, "port").is_err());
    }

    #[test]
    fn test_load_toml() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"name = "test"\nport = 1234"#).unwrap();
        // Write valid TOML.
        let mut f2 = tempfile::NamedTempFile::new().unwrap();
        writeln!(f2, "name = \"test\"\nport = 1234").unwrap();
        let map = load_toml(f2.path()).unwrap();
        assert_eq!(map["name"], Value::String("test".into()));
    }
}
