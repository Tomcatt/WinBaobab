//! Human-readable size and count formatting.

const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];

/// Formats a byte count: "0 bytes", "1 byte", "1023 bytes", "1.0 KB", "4.2 GB".
pub fn format_size(bytes: u64) -> String {
    if bytes == 1 {
        return "1 byte".to_string();
    }
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while (value * 10.0).round() / 10.0 >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Formats a count with thousands separators: 1312 -> "1,312".
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// "YYYY-MM-DD HH:MM" from local calendar fields.
pub fn format_datetime(y: u16, mo: u16, d: u16, h: u16, mi: u16) -> String {
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1), "1 byte");
        assert_eq!(format_size(2), "2 bytes");
        assert_eq!(format_size(1023), "1023 bytes");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(4_509_715_660), "4.2 GB");
        assert_eq!(format_size(1_099_511_627_776), "1.0 TB");
        assert_eq!(format_size(1_048_575), "1.0 MB");
        assert_eq!(format_size(u64::MAX), "16384.0 PB");
    }

    #[test]
    fn counts() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1312), "1,312");
        assert_eq!(format_count(1_000_000), "1,000,000");
    }

    #[test]
    fn datetime_formats() {
        assert_eq!(format_datetime(2026, 3, 9, 7, 5), "2026-03-09 07:05");
    }
}
