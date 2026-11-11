//! Query-string parsing: time ranges in several formats and label selectors.

use axum::extract::Query;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use lidlesseye_core::selector::Matcher;
use lidlesseye_core::{ProfileType, Selector, parse_duration_ns};

use crate::error::ApiError;

/// Raw query parameters, preserving repeated keys (`match[]=...`).
#[derive(Debug, Default, Clone)]
pub struct Params(pub Vec<(String, String)>);

impl Params {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.0
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn usize_param(&self, key: &str, default: usize) -> usize {
        self.get(key)
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(default)
    }

    /// `from` / `until` as nanoseconds since the epoch.
    ///
    /// Accepted formats: unix seconds (default), `ms`/`us`/`ns` suffixes,
    /// RFC3339 timestamps, `now`, `now-15m`, `now-2h`.
    pub fn range(&self, now_ns: u64) -> Result<(u64, u64), ApiError> {
        let until = match self.get("until") {
            Some(value) => parse_time_ns(value, now_ns)?,
            None => now_ns,
        };
        let from = match self.get("from") {
            Some(value) => parse_time_ns(value, now_ns)?,
            None => until.saturating_sub(3_600_000_000_000),
        };
        if until <= from {
            return Err(ApiError::bad_request(format!(
                "`until` ({until}) must be greater than `from` ({from})"
            )));
        }
        Ok((from, until))
    }

    pub fn profile_type(&self) -> ProfileType {
        self.get("profile_type")
            .or_else(|| self.get("type"))
            .filter(|s| !s.trim().is_empty())
            .map(ProfileType::new)
            .unwrap_or_else(ProfileType::cpu)
    }

    /// Accepts `selector={service_name="x"}` as well as `service_name=x` and
    /// repeated `match[]` parameters.
    pub fn selector(&self) -> Result<Selector, ApiError> {
        if let Some(raw) = self.get("selector") {
            return Selector::parse(raw).map_err(ApiError::from);
        }
        let mut matchers: Vec<Matcher> = Vec::new();
        for raw in self.get_all("match[]").chain(self.get_all("match")) {
            let parsed = Selector::parse(raw).map_err(ApiError::from)?;
            matchers.extend(parsed.matchers);
        }
        if let Some(service) = self.get("service_name") {
            matchers.push(Matcher::eq("service_name", service));
        }
        if let Some(label) = self.get("label")
            && let Some(value) = self.get("label_value")
        {
            matchers.push(Matcher::eq(label, value));
        }
        Ok(Selector::new(matchers))
    }
}

impl<S> axum::extract::FromRequestParts<S> for Params
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let Query(pairs) = Query::<Vec<(String, String)>>::from_request_parts(parts, state)
            .await
            .map_err(|err| ApiError::bad_request(format!("invalid query string: {err}")))?;
        Ok(Params(pairs))
    }
}

/// Parse a point in time into nanoseconds since the epoch.
pub fn parse_time_ns(input: &str, now_ns: u64) -> Result<u64, ApiError> {
    let value = input.trim();
    if value.is_empty() {
        return Err(ApiError::bad_request("empty timestamp"));
    }
    if value.eq_ignore_ascii_case("now") {
        return Ok(now_ns);
    }
    if let Some(rest) = value.strip_prefix("now-") {
        let delta = parse_duration_ns(rest)
            .map_err(|err| ApiError::bad_request(format!("invalid relative time: {err}")))?;
        return Ok(now_ns.saturating_sub(delta));
    }
    if let Ok(dt) = OffsetDateTime::parse(value, &Rfc3339) {
        let nanos = dt.unix_timestamp_nanos();
        if nanos < 0 {
            return Err(ApiError::bad_request("timestamps before 1970 are not supported"));
        }
        return Ok(nanos as u64);
    }

    // Numeric value, optionally with an explicit unit.
    let (number, unit) = match value.find(|c: char| c.is_ascii_alphabetic()) {
        Some(idx) => (&value[..idx], &value[idx..]),
        None => (value, ""),
    };
    let number: f64 = number
        .parse()
        .map_err(|_| ApiError::bad_request(format!("invalid timestamp `{input}`")))?;
    if number < 0.0 {
        return Err(ApiError::bad_request("timestamps before 1970 are not supported"));
    }
    let multiplier = match unit {
        "ns" => 1.0,
        "us" | "µs" => 1_000.0,
        "ms" => 1_000_000.0,
        "s" | "" => {
            if unit.is_empty() {
                // Guess from magnitude: seconds, milliseconds, microseconds.
                if number >= 1e17 {
                    1.0
                } else if number >= 1e14 {
                    1_000.0
                } else if number >= 1e11 {
                    1_000_000.0
                } else {
                    1_000_000_000.0
                }
            } else {
                1_000_000_000.0
            }
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "unsupported time unit `{other}`"
            )));
        }
    };
    Ok((number * multiplier) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000_000_000;

    #[test]
    fn parses_unix_seconds() {
        assert_eq!(parse_time_ns("1700000000", NOW).unwrap(), 1_700_000_000_000_000_000);
    }

    #[test]
    fn parses_milliseconds() {
        assert_eq!(
            parse_time_ns("1700000000000", NOW).unwrap(),
            1_700_000_000_000_000_000
        );
        assert_eq!(parse_time_ns("1700000000000ms", NOW).unwrap(), 1_700_000_000_000_000_000);
    }

    #[test]
    fn parses_relative() {
        assert_eq!(parse_time_ns("now", NOW).unwrap(), NOW);
        assert_eq!(
            parse_time_ns("now-1h", NOW).unwrap(),
            NOW - 3_600_000_000_000
        );
    }

    #[test]
    fn parses_rfc3339() {
        assert_eq!(
            parse_time_ns("2023-11-14T22:13:20Z", NOW).unwrap(),
            1_700_000_000_000_000_000
        );
    }

    #[test]
    fn range_defaults_to_one_hour() {
        let p = Params(vec![]);
        let (from, until) = p.range(NOW).unwrap();
        assert_eq!(until, NOW);
        assert_eq!(until - from, 3_600_000_000_000);
        assert!(p.range(NOW).is_ok());
    }

    #[test]
    fn selector_from_params() {
        let p = Params(vec![
            ("service_name".into(), "checkout".into()),
            ("match[]".into(), r#"{pod=~"checkout-*"}"#.into()),
        ]);
        let s = p.selector().unwrap();
        assert_eq!(s.matchers.len(), 2);
        assert_eq!(p.profile_type(), ProfileType::cpu());
    }
}
