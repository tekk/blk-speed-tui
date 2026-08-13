//! Human readable formatting helpers.
//!
//! Transfer rates use SI units (kB/s, MB/s, GB/s) because that is what drive
//! vendors and every other benchmarking tool print. Block and capacity sizes
//! use IEC units (KiB, MiB, GiB) because that is how they are actually
//! allocated.

use std::time::Duration;

const SI: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
const IEC: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

fn scale(value: f64, base: f64, units: &[&'static str; 7]) -> (f64, &'static str) {
    let mut v = value;
    let mut i = 0;
    while v.abs() >= base && i + 1 < units.len() {
        v /= base;
        i += 1;
    }
    (v, units[i])
}

fn digits(v: f64) -> usize {
    if v >= 100.0 {
        0
    } else if v >= 10.0 {
        1
    } else {
        2
    }
}

/// `1234567.0` -> `"1.23 MB/s"`
pub fn rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "—".to_string();
    }
    let (v, unit) = scale(bytes_per_sec, 1000.0, &SI);
    format!("{:.*} {}/s", digits(v), v, unit)
}

/// `1048576` -> `"1.00 MiB"`
pub fn bytes(n: u64) -> String {
    let (v, unit) = scale(n as f64, 1024.0, &IEC);
    format!("{:.*} {}", digits(v), v, unit)
}

/// Compact form for tight columns: `4096` -> `"4K"`, `4194304` -> `"4M"`.
pub fn block_size(n: usize) -> String {
    let n = n as u64;
    if n >= 1 << 30 {
        format!("{}G", n >> 30)
    } else if n >= 1 << 20 {
        format!("{}M", n >> 20)
    } else if n >= 1 << 10 {
        format!("{}K", n >> 10)
    } else {
        format!("{n}B")
    }
}

/// IOPS with thousands separators dropped in favour of scaling.
pub fn iops(v: f64) -> String {
    if !v.is_finite() || v <= 0.0 {
        return "—".to_string();
    }
    if v >= 1_000_000.0 {
        format!("{:.2}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.1}k", v / 1_000.0)
    } else {
        format!("{v:.0}")
    }
}

/// Sub-millisecond latencies matter for NVMe, so scale down to µs.
pub fn latency(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us <= 0.0 {
        return "—".to_string();
    }
    if us < 1000.0 {
        format!("{us:.0} µs")
    } else if us < 1_000_000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{:.2} s", us / 1e6)
    }
}

pub fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_scale_in_si_units() {
        assert_eq!(rate(999.0), "999 B/s");
        assert_eq!(rate(1_000.0), "1.00 kB/s");
        assert_eq!(rate(1_234_567.0), "1.23 MB/s");
        assert_eq!(rate(3_500_000_000.0), "3.50 GB/s");
        assert_eq!(rate(0.0), "—");
    }

    #[test]
    fn sizes_scale_in_iec_units() {
        assert_eq!(bytes(1024), "1.00 KiB");
        assert_eq!(bytes(500_107_862_016), "466 GiB");
    }

    #[test]
    fn block_sizes_are_compact() {
        assert_eq!(block_size(4096), "4K");
        assert_eq!(block_size(4 << 20), "4M");
        assert_eq!(block_size(512), "512B");
    }

    #[test]
    fn latency_picks_a_readable_unit() {
        assert_eq!(latency(Duration::from_micros(120)), "120 µs");
        assert_eq!(latency(Duration::from_micros(2500)), "2.50 ms");
    }
}
