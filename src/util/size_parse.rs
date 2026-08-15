//! Human-friendly byte size parsing (`500M`, `1G`, raw integers).

use crate::error::{Error, Result};

/// Default nested parallel size budget: 500 MiB.
pub const DEFAULT_NESTED_SIZE_BUDGET: u64 = 500 * 1024 * 1024;

/// Parse a size string into bytes.
///
/// Accepts plain integers (bytes) or a number with optional suffix:
/// `K`/`KB`/`KiB`, `M`/`MB`/`MiB`, `G`/`GB`/`GiB` (binary, 1024-based).
pub fn parse_byte_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::Other("empty size string".into()));
    }
    let lower = s.to_ascii_lowercase();
    let (num_str, mult) = if let Some(n) = lower.strip_suffix("kib") {
        (n, 1024u64)
    } else if let Some(n) = lower.strip_suffix("mib") {
        (n, 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix("gib") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix("kb") {
        (n, 1024)
    } else if let Some(n) = lower.strip_suffix("mb") {
        (n, 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix("gb") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix('k') {
        (n, 1024)
    } else if let Some(n) = lower.strip_suffix('m') {
        (n, 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix('g') {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = lower.strip_suffix('b') {
        (n, 1)
    } else {
        (lower.as_str(), 1)
    };
    let num_str = num_str.trim();
    if num_str.is_empty() {
        return Err(Error::Other(format!("missing number in size '{s}'")));
    }
    let n: f64 = num_str
        .parse()
        .map_err(|_| Error::Other(format!("invalid size number in '{s}'")))?;
    if n < 0.0 || !n.is_finite() {
        return Err(Error::Other(format!("invalid size '{s}'")));
    }
    let bytes = n * mult as f64;
    if bytes > u64::MAX as f64 {
        return Err(Error::Other(format!("size too large: '{s}'")));
    }
    Ok(bytes.round() as u64)
}

/// Whether a job of `job_size` may start given current in-flight state.
///
/// - Never exceeds `max_workers`.
/// - If nothing is running, always admit (a single oversized nest runs alone).
/// - If `budget == 0`, size is unlimited (workers only).
/// - Else require `running_sum + job_size <= budget`.
pub fn can_admit_nested(
    running_sum: u64,
    running_count: usize,
    job_size: u64,
    budget: u64,
    max_workers: usize,
) -> bool {
    if max_workers == 0 || running_count >= max_workers {
        return false;
    }
    if running_count == 0 {
        return true;
    }
    if budget == 0 {
        return true;
    }
    running_sum.saturating_add(job_size) <= budget
}

/// Resolve max nested workers: explicit concurrency, else threads, else CPU count.
pub fn resolve_nested_workers(nested_concurrency: usize, threads: Option<u32>) -> usize {
    if nested_concurrency > 0 {
        return nested_concurrency;
    }
    if let Some(t) = threads {
        return (t as usize).max(1);
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_suffixes() {
        assert_eq!(parse_byte_size("100").unwrap(), 100);
        assert_eq!(parse_byte_size("500M").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_byte_size("500MB").unwrap(), 500 * 1024 * 1024);
        assert_eq!(parse_byte_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("1.5K").unwrap(), 1536);
    }

    #[test]
    fn admit_rules() {
        let budget = 500 * 1024 * 1024;
        let m100 = 100 * 1024 * 1024;
        let m400 = 400 * 1024 * 1024;
        // empty → admit even oversized
        assert!(can_admit_nested(0, 0, budget * 2, budget, 4));
        // two 100MiB in flight, 400MiB next does not fit (200+400 > 500)
        assert!(can_admit_nested(0, 0, m100, budget, 5));
        assert!(can_admit_nested(m100, 1, m100, budget, 5));
        assert!(!can_admit_nested(2 * m100, 2, m400, budget, 5));
        // five 100MiB with 5 workers fills the budget
        let mut sum = 0u64;
        for i in 0..5 {
            assert!(can_admit_nested(sum, i, m100, budget, 5), "i={i}");
            sum += m100;
        }
        assert!(!can_admit_nested(sum, 5, m100, budget, 5));
    }

    #[test]
    fn resolve_workers() {
        assert_eq!(resolve_nested_workers(3, Some(8)), 3);
        assert_eq!(resolve_nested_workers(0, Some(4)), 4);
        assert!(resolve_nested_workers(0, None) >= 1);
    }
}
