use serde_json::{json, Value};
use sysinfo::{Disks, System};

/// Returns a JSON blob with current system metrics:
/// {
///   "uptime_secs": 12345,
///   "cpu_usage_pct": 14.2,
///   "memory": { "used_bytes": ..., "total_bytes": ..., "used_pct": 42.1 },
///   "disks": [ { "mount": "/", "used_bytes": ..., "total_bytes": ..., "used_pct": 55.0 } ]
/// }
pub fn system_blob() -> Value {
    let mut sys = System::new();
    sys.refresh_cpu_usage();
    sys.refresh_memory();

    let uptime = System::uptime();

    // Average across all logical CPUs
    let cpus = sys.cpus();
    let cpu_pct = if cpus.is_empty() {
        0.0
    } else {
        cpus.iter().map(|c| c.cpu_usage() as f64).sum::<f64>() / cpus.len() as f64
    };

    let mem_used = sys.used_memory();
    let mem_total = sys.total_memory();
    let mem_pct = if mem_total > 0 {
        mem_used as f64 / mem_total as f64 * 100.0
    } else {
        0.0
    };

    let disks: Vec<Value> = Disks::new_with_refreshed_list()
        .iter()
        .map(|d| {
            let total = d.total_space();
            let avail = d.available_space();
            let used = total.saturating_sub(avail);
            let pct = if total > 0 {
                used as f64 / total as f64 * 100.0
            } else {
                0.0
            };
            json!({
                "mount":       d.mount_point().to_string_lossy(),
                "used_bytes":  used,
                "total_bytes": total,
                "used_pct":    (pct * 10.0).round() / 10.0,
            })
        })
        .collect();

    json!({
        "uptime_secs":   uptime,
        "cpu_usage_pct": (cpu_pct * 10.0).round() / 10.0,
        "memory": {
            "used_bytes":  mem_used,
            "total_bytes": mem_total,
            "used_pct":    (mem_pct * 10.0).round() / 10.0,
        },
        "disks": disks,
    })
}
