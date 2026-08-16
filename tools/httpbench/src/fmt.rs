//! Human-readable formatting shared by the console report and the charts.

/// Byte counts as binary units: `1.0 KiB`, `8.0 MiB`.
pub fn bytes(value: f64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if value >= MIB {
        format!("{:.0} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.0} KiB", value / KIB)
    } else {
        format!("{value:.0} B")
    }
}

/// Milliseconds with a precision that suits the magnitude.
pub fn millis(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0} ms")
    } else if value >= 10.0 {
        format!("{value:.1} ms")
    } else {
        format!("{value:.2} ms")
    }
}

/// Request rates, abbreviated once they get long.
pub fn rate(value: f64) -> String {
    if value >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.0}k", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

/// A plain count, for the concurrency axis.
pub fn count(value: f64) -> String {
    format!("{value:.0}")
}

/// Fractional CPU cores.
pub fn cores(value: f64) -> String {
    format!("{value:.2}")
}

/// Throughput in MiB/s.
pub fn mib_per_second(value: f64) -> String {
    format!("{value:.0}")
}

/// Parse `4k`, `2MiB`, `1048576` into a byte count.
pub fn parse_bytes(text: &str) -> Result<usize, String> {
    let trimmed = text.trim();
    let (digits, multiplier) = match trimmed.to_ascii_lowercase() {
        t if t.ends_with("gib") || t.ends_with('g') => (strip_suffix(trimmed), 1024 * 1024 * 1024),
        t if t.ends_with("mib") || t.ends_with('m') => (strip_suffix(trimmed), 1024 * 1024),
        t if t.ends_with("kib") || t.ends_with('k') => (strip_suffix(trimmed), 1024),
        t if t.ends_with('b') => (strip_suffix(trimmed), 1),
        _ => (trimmed, 1),
    };
    digits
        .trim()
        .parse::<usize>()
        .map(|n| n * multiplier)
        .map_err(|_| format!("'{text}' is not a byte size"))
}

fn strip_suffix(text: &str) -> &str {
    text.trim_end_matches(|c: char| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes_use_binary_units() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1024.0), "1 KiB");
        assert_eq!(bytes(8.0 * 1024.0 * 1024.0), "8 MiB");
    }

    #[test]
    fn latency_precision_tracks_magnitude() {
        assert_eq!(millis(0.123), "0.12 ms");
        assert_eq!(millis(12.34), "12.3 ms");
        assert_eq!(millis(1234.0), "1234 ms");
    }

    #[test]
    fn rates_are_abbreviated() {
        assert_eq!(rate(950.0), "950");
        assert_eq!(rate(12_600.0), "13k");
        assert_eq!(rate(2_400_000.0), "2.4M");
    }

    #[test]
    fn byte_sizes_parse_with_and_without_units() {
        assert_eq!(parse_bytes("1024"), Ok(1024));
        assert_eq!(parse_bytes("4k"), Ok(4096));
        assert_eq!(parse_bytes("4KiB"), Ok(4096));
        assert_eq!(parse_bytes("8MiB"), Ok(8 * 1024 * 1024));
        assert_eq!(parse_bytes(" 2 m "), Ok(2 * 1024 * 1024));
        assert!(parse_bytes("banana").is_err());
    }
}
