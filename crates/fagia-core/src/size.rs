//! Parsing and formatting of sizes and durations. Suffixes are binary
//! (K = KiB) because that is what the output shows.

use crate::{Error, Result};
use std::time::Duration;

const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

pub fn parse_size(input: &str) -> Result<u64> {
    let s = input.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num
        .parse()
        .map_err(|_| Error::InvalidSize(input.to_string()))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return Err(Error::InvalidSize(input.to_string())),
    };
    if !value.is_finite() || value < 0.0 {
        return Err(Error::InvalidSize(input.to_string()));
    }
    Ok((value * mult as f64).round() as u64)
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Signed variant for growth reports.
pub fn format_delta(bytes: i64) -> String {
    let sign = if bytes < 0 { "-" } else { "+" };
    format!("{sign}{}", format_size(bytes.unsigned_abs()))
}

pub fn parse_duration(input: &str) -> Result<Duration> {
    let s = input.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: u64 = num
        .parse()
        .map_err(|_| Error::InvalidDuration(input.to_string()))?;
    let secs = match unit {
        "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" | "" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(Error::InvalidDuration(input.to_string())),
    };
    Ok(Duration::from_secs(value * secs))
}

/// Compact age such as `210d`, `5h`, `12m`.
pub fn format_age(secs: u64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3600 => format!("{}h", s / 3600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_suffixes() {
        assert_eq!(parse_size("100M").unwrap(), 100 << 20);
        assert_eq!(parse_size("1.5G").unwrap(), 3 << 29);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("10KiB").unwrap(), 10 << 10);
        assert!(parse_size("ten").is_err());
        assert!(parse_size("5X").is_err());
    }

    #[test]
    fn formats_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(38 * (1 << 30) + (1 << 29)), "38.5 GiB");
        assert_eq!(format_delta(-2048), "-2.0 KiB");
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("90d").unwrap().as_secs(), 90 * 86_400);
        assert_eq!(parse_duration("1h").unwrap().as_secs(), 3600);
        assert_eq!(parse_duration("30").unwrap().as_secs(), 30 * 86_400);
        assert!(parse_duration("1y").is_err());
        assert_eq!(format_age(3 * 86_400 + 5), "3d");
    }
}
