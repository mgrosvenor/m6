use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Returns a JSON blob of log lines plus the current end-of-file byte offset.
///
/// - `offset = None` (or 0): return the last `n` lines (tail behaviour) and end offset.
/// - `offset = Some(pos)`: return only new lines written since `pos` and new end offset.
///
/// Response shape:
/// { "lines": ["…","…"], "end_offset": 12345, "total_lines": 1234 }
pub fn logs_blob(log_path: &Path, n: usize, offset: Option<u64>) -> Value {
    let file_len = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);

    if let Some(off) = offset.filter(|&o| o > 0) {
        // Incremental read: only bytes since `off`.
        let mut f = match std::fs::File::open(log_path) {
            Ok(f) => f,
            Err(e) => {
                return json!({ "error": format!("cannot read log: {e}"), "lines": [], "end_offset": 0 })
            }
        };
        if f.seek(SeekFrom::Start(off)).is_err() {
            return json!({ "lines": [], "end_offset": file_len });
        }
        let mut buf = String::new();
        f.read_to_string(&mut buf).ok();
        let lines: Vec<&str> = buf.lines().collect();
        return json!({ "lines": lines, "end_offset": file_len });
    }

    // Initial load: last `n` lines + end offset.
    let text = match std::fs::read_to_string(log_path) {
        Ok(t) => t,
        Err(e) => {
            return json!({ "error": format!("cannot read log: {e}"), "lines": [], "end_offset": 0, "total_lines": 0 })
        }
    };
    let all: Vec<&str> = text.lines().collect();
    let total = all.len();
    let start = total.saturating_sub(n);
    json!({ "lines": all[start..].to_vec(), "end_offset": file_len, "total_lines": total })
}
