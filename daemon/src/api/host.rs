//! Host-level metrics for the daemon dashboard, read from /proc so they work
//! even when no guest management call is involved.
//!
//! CPU is reported as raw jiffy counters (`cpu_total`, `cpu_idle`): a single
//! reading cannot express "CPU %", so the client keeps the previous sample
//! and divides the delta (same contract as the per-domain stats endpoint).

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde_json::{Value, json};

use super::ApiError;

pub fn routes() -> Router {
    Router::new().route("/host/stats", get(stats))
}

async fn stats() -> Result<Json<Value>, ApiError> {
    let stat = std::fs::read_to_string("/proc/stat")
        .map_err(|e| ApiError::internal(format!("read /proc/stat: {e}")))?;
    let meminfo = std::fs::read_to_string("/proc/meminfo")
        .map_err(|e| ApiError::internal(format!("read /proc/meminfo: {e}")))?;

    let (cpu_total, cpu_idle) = cpu_counters(&stat).unwrap_or((0, 0));
    let mem_total_kib = meminfo_kib(&meminfo, "MemTotal").unwrap_or(0);
    let mem_available_kib = meminfo_kib(&meminfo, "MemAvailable").unwrap_or(0);
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);

    Ok(Json(json!({
        "cpu_total": cpu_total,
        "cpu_idle": cpu_idle,
        "mem_total_kib": mem_total_kib,
        "mem_available_kib": mem_available_kib,
        "cpus": cpus,
        "sampled_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    })))
}

/// Host CPU count, model and frequency, and total memory, for the guest stats
/// endpoint.
pub(super) fn node_info() -> (u32, u64, u32, String) {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let cpus = cpuinfo_count(&cpuinfo);
    let memory_kib = meminfo_kib(&meminfo, "MemTotal").unwrap_or(0);
    let mhz = cpuinfo_mhz(&cpuinfo);
    let model = cpuinfo_model(&cpuinfo);
    (cpus, memory_kib, mhz, model)
}

/// The number of `processor` entries; `available_parallelism` respects a
/// container's affinity mask, which would under-report the host.
pub(super) fn cpuinfo_count(cpuinfo: &str) -> u32 {
    let n = cpuinfo
        .lines()
        .filter(|l| {
            l.split_once(':')
                .is_some_and(|(k, _)| k.trim() == "processor")
        })
        .count();
    (n.max(1)) as u32
}

pub(super) fn cpuinfo_model(cpuinfo: &str) -> String {
    cpuinfo
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == "model name").then(|| v.trim().to_string())
        })
        .unwrap_or_default()
}

pub(super) fn cpuinfo_mhz(cpuinfo: &str) -> u32 {
    cpuinfo
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim() != "cpu MHz" {
                return None;
            }
            v.trim().parse::<f64>().ok().map(|m| m.round() as u32)
        })
        .unwrap_or(0)
}

/// Total and idle jiffies from the first `cpu` line of /proc/stat.
///
/// `idle` includes iowait: from the host's perspective that time is not
/// spendable by anyone else, so it belongs in the "not busy" bucket.
pub(super) fn cpu_counters(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|n| n.parse().ok())
        .collect();
    let total: u64 = vals.iter().sum();
    let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
    Some((total, idle))
}

/// A `MemTotal:`-style line's value in kibibytes, matched case-insensitively.
pub(super) fn meminfo_kib(meminfo: &str, key: &str) -> Option<u64> {
    meminfo.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if !k.trim().eq_ignore_ascii_case(key) {
            return None;
        }
        v.split_whitespace().next()?.parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_counters_sum_and_separate_idle_plus_iowait() {
        //           user nice system idle iowait irq softirq steal
        let stat = "cpu  100 20 50 800 30 10 5 0 0 0\ncpu0 1 2 3 4 5 6 7 8 9 0\n";
        let (total, idle) = cpu_counters(stat).unwrap();
        assert_eq!(total, 1015, "sum of every counter on the cpu line");
        assert_eq!(idle, 830, "idle + iowait");
    }

    #[test]
    fn cpu_counters_takes_the_aggregate_line_not_per_cpu() {
        let stat = "cpu0 1 2 3 4 5 6 7 8\ncpu  1 1 1 1 1 1 1 1\n";
        let (total, idle) = cpu_counters(stat).unwrap();
        assert_eq!(total, 8);
        assert_eq!(idle, 2);
    }

    #[test]
    fn meminfo_parses_kib_case_insensitively() {
        let mem = "MemTotal:       16000000 kB\nMemAvailable:   8000000 kB\n";
        assert_eq!(meminfo_kib(mem, "MemTotal"), Some(16_000_000));
        assert_eq!(meminfo_kib(mem, "memavailable"), Some(8_000_000));
        assert_eq!(meminfo_kib(mem, "SwapTotal"), None);
    }

    #[test]
    fn node_info_is_read_from_cpuinfo() {
        let cpuinfo = "\
processor\t: 0
model name\t: Example CPU
cpu MHz\t\t: 2400.000

processor\t: 1
model name\t: Example CPU
cpu MHz\t\t: 2400.000
";
        assert_eq!(cpuinfo_count(cpuinfo), 2, "one per processor entry");
        assert_eq!(cpuinfo_model(cpuinfo), "Example CPU");
        assert_eq!(cpuinfo_mhz(cpuinfo), 2400);
        assert_eq!(cpuinfo_count(""), 1, "never reports zero processors");
    }
}
