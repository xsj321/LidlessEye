//! Small time helpers.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// Nanoseconds since the Unix epoch.
pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Parse durations such as `30s`, `5m`, `2h`, `7d`, `1w`, `500ms`.
///
/// Returns nanoseconds. A bare number is interpreted as seconds.
pub fn parse_duration_ns(input: &str) -> Result<u64> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::InvalidArgument("empty duration".into()));
    }
    let (digits, unit) = input
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map(|idx| input.split_at(idx))
        .unwrap_or((input, "s"));
    let value: f64 = digits
        .parse()
        .map_err(|_| Error::InvalidArgument(format!("invalid duration `{input}`")))?;
    if value < 0.0 || !value.is_finite() {
        return Err(Error::InvalidArgument(format!(
            "invalid duration `{input}`"
        )));
    }
    let multiplier: f64 = match unit.trim() {
        "ns" => 1.0,
        "us" | "µs" => 1_000.0,
        "ms" => 1_000_000.0,
        "s" | "" => 1_000_000_000.0,
        "m" => 60.0 * 1_000_000_000.0,
        "h" => 3_600.0 * 1_000_000_000.0,
        "d" => 86_400.0 * 1_000_000_000.0,
        "w" => 7.0 * 86_400.0 * 1_000_000_000.0,
        other => {
            return Err(Error::InvalidArgument(format!(
                "unknown duration unit `{other}`"
            )));
        }
    };
    Ok((value * multiplier) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration_ns("30s").unwrap(), 30_000_000_000);
        assert_eq!(parse_duration_ns("5m").unwrap(), 300_000_000_000);
        assert_eq!(parse_duration_ns("2h").unwrap(), 7_200_000_000_000);
        assert_eq!(parse_duration_ns("1d").unwrap(), 86_400_000_000_000);
        assert_eq!(parse_duration_ns("500ms").unwrap(), 500_000_000);
        assert_eq!(parse_duration_ns("10").unwrap(), 10_000_000_000);
        assert!(parse_duration_ns("nope").is_err());
        assert!(parse_duration_ns("5x").is_err());
    }
}
