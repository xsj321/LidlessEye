//! Label selectors, in a PromQL-flavoured syntax.
//!
//! ```text
//! {service_name="checkout"}
//! {service_name="checkout", namespace="shop", pod=~"checkout-*"}
//! service_name="checkout"          # braces are optional
//! ```
//!
//! Supported operators: `=` (equality), `!=` (inequality), `=~` / `!~`
//! (glob matching with `*` and `?`). Glob matching is used instead of full
//! regexes on purpose: it covers the label matching people actually write and
//! keeps the query path allocation-free.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// How a label value is compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchOp {
    Eq,
    NotEq,
    Glob,
    NotGlob,
}

impl MatchOp {
    fn as_str(self) -> &'static str {
        match self {
            MatchOp::Eq => "=",
            MatchOp::NotEq => "!=",
            MatchOp::Glob => "=~",
            MatchOp::NotGlob => "!~",
        }
    }

    fn matches(self, pattern: &str, value: Option<&str>) -> bool {
        match self {
            MatchOp::Eq => value == Some(pattern),
            MatchOp::NotEq => value != Some(pattern),
            MatchOp::Glob => value.is_some_and(|v| glob_match(pattern, v)),
            MatchOp::NotGlob => !value.is_some_and(|v| glob_match(pattern, v)),
        }
    }
}

/// A single `label op value` matcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Matcher {
    pub label: String,
    pub op: MatchOp,
    pub value: String,
}

impl Matcher {
    pub fn new(label: impl Into<String>, op: MatchOp, value: impl Into<String>) -> Self {
        Matcher {
            label: label.into(),
            op,
            value: value.into(),
        }
    }

    pub fn eq(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(label, MatchOp::Eq, value)
    }
}

/// A conjunction of matchers. An empty selector matches everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    pub matchers: Vec<Matcher>,
}

impl Selector {
    pub fn new(matchers: Vec<Matcher>) -> Self {
        Selector { matchers }
    }

    pub fn service(name: impl Into<String>) -> Self {
        Selector::new(vec![Matcher::eq(
            crate::model::SERVICE_NAME_LABEL,
            name,
        )])
    }

    pub fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }

    /// Does a label set satisfy every matcher?
    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        self.matchers.iter().all(|m| {
            let value = labels
                .get(&m.label)
                .map(|s| s.as_str())
                .or_else(|| {
                    // `service_name` is stored as its own field in segments;
                    // callers inject it, but be forgiving.
                    None
                });
            m.op.matches(&m.value, value)
        })
    }

    /// Same as [`Selector::matches`] but with an explicit service name.
    pub fn matches_meta(&self, service_name: &str, labels: &BTreeMap<String, String>) -> bool {
        self.matchers.iter().all(|m| {
            let value = if m.label == crate::model::SERVICE_NAME_LABEL {
                Some(service_name)
            } else {
                labels.get(&m.label).map(|s| s.as_str())
            };
            m.op.matches(&m.value, value)
        })
    }

    /// Parse a selector expression.
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed == "*" || trimmed == "{}" {
            return Ok(Selector::default());
        }

        let body = if trimmed.starts_with('{') {
            if !trimmed.ends_with('}') {
                return Err(Error::Selector("missing closing `}`".into()));
            }
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        };

        let mut matchers = Vec::new();
        for part in split_matchers(body) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            matchers.push(parse_matcher(part)?);
        }
        Ok(Selector { matchers })
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (i, m) in self.matchers.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}{}\"{}\"", m.label, m.op.as_str(), m.value)?;
        }
        f.write_str("}")
    }
}

/// Split on commas that are not inside quotes.
fn split_matchers(body: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == '\\' {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    current.push(c);
                } else if c == ',' {
                    parts.push(current.clone());
                    current.clear();
                } else {
                    current.push(c);
                }
            }
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

fn parse_matcher(input: &str) -> Result<Matcher> {
    // Find the operator, scanning left to right and skipping quoted values.
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut label_end = None;
    while i < bytes.len() {
        match bytes[i] {
            b'=' | b'!' => {
                label_end = Some(i);
                break;
            }
            _ => i += 1,
        }
    }
    let label_end = label_end.ok_or_else(|| {
        Error::Selector(format!("matcher `{input}` is missing an operator"))
    })?;
    let label = input[..label_end].trim();
    if label.is_empty() {
        return Err(Error::Selector(format!(
            "matcher `{input}` has an empty label name"
        )));
    }
    let mut rest = &input[label_end..];
    let op = if rest.starts_with("!=") {
        MatchOp::NotEq
    } else if rest.starts_with("!~") {
        MatchOp::NotGlob
    } else if rest.starts_with("=~") {
        MatchOp::Glob
    } else if rest.starts_with('=') {
        MatchOp::Eq
    } else {
        return Err(Error::Selector(format!(
            "matcher `{input}` has an unsupported operator"
        )));
    };
    // `=` consumes one byte, `!=`, `=~` and `!~` consume two.
    rest = &rest[if op == MatchOp::Eq { 1 } else { 2 }..];
    let value = unquote(rest.trim()).ok_or_else(|| {
        Error::Selector(format!("matcher `{input}` has an unterminated value"))
    })?;
    Ok(Matcher {
        label: label.to_string(),
        op,
        value,
    })
}

fn unquote(input: &str) -> Option<String> {
    let input = input.trim();
    if input.len() >= 2 {
        let first = input.chars().next()?;
        let last = input.chars().last()?;
        if (first == '"' || first == '\'') && first == last {
            let inner = &input[first.len_utf8()..input.len() - last.len_utf8()];
            let mut out = String::with_capacity(inner.len());
            let mut escaped = false;
            for c in inner.chars() {
                if escaped {
                    out.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else {
                    out.push(c);
                }
            }
            return Some(out);
        }
    }
    if input.is_empty() {
        None
    } else {
        Some(input.to_string())
    }
}

/// `*` matches any sequence of characters, `?` matches exactly one.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let (mut pi, mut vi) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_vi = 0usize;

    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_full_selector() {
        let s = Selector::parse(r#"{service_name="checkout", namespace='shop', pod=~"checkout-*"}"#)
            .unwrap();
        assert_eq!(s.matchers.len(), 3);
        assert_eq!(s.matchers[0].label, "service_name");
        assert_eq!(s.matchers[0].op, MatchOp::Eq);
        assert_eq!(s.matchers[2].op, MatchOp::Glob);
    }

    #[test]
    fn parses_without_braces() {
        let s = Selector::parse(r#"service_name="api""#).unwrap();
        assert_eq!(s.matchers.len(), 1);
        assert!(s.matches_meta("api", &BTreeMap::new()));
        assert!(!s.matches_meta("web", &BTreeMap::new()));
    }

    #[test]
    fn empty_and_star_match_everything() {
        for input in ["", "*", "{}"] {
            let s = Selector::parse(input).unwrap();
            assert!(s.is_empty());
            assert!(s.matches_meta("anything", &labels(&[("a", "b")])));
        }
    }

    #[test]
    fn globs() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("checkout-*", "checkout-abc-123"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("*-prod", "api-prod"));
        assert!(!glob_match("prod*", "api-prod"));
    }

    #[test]
    fn combined_matchers() {
        let s = Selector::parse(r#"{namespace="shop",pod=~"checkout-*",env!="dev"}"#).unwrap();
        let l = labels(&[("namespace", "shop"), ("pod", "checkout-1")]);
        assert!(s.matches_meta("svc", &l));
        let l = labels(&[("namespace", "shop"), ("pod", "checkout-1"), ("env", "dev")]);
        assert!(!s.matches_meta("svc", &l));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Selector::parse("{service_name}").is_err());
        assert!(Selector::parse("{service_name=\"x\"").is_err());
        assert!(Selector::parse(r#"{"x"=1}"#).is_ok());
        assert!(Selector::parse(r#"{=1}"#).is_err());
    }
}
