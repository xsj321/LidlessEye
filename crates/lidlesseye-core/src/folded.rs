//! The "folded stacks" text format.
//!
//! ```text
//! main;handle_request;encode_json 1200
//! main;handle_request;db_query 300
//! ```
//!
//! This is what `perf script | stackcollapse-perf.pl`, `cargo flamegraph` and
//! most ad-hoc profiling scripts emit, and frames are listed **root first**,
//! the opposite of the order the kernel and [`crate::Report`] use.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::model::{Frame, ProfileType, Report, ReportBuilder};

/// Where the fields the text format does not carry come from.
#[derive(Debug, Clone)]
pub struct FoldedOptions {
    pub service_name: String,
    pub profile_type: ProfileType,
    pub start_ns: u64,
    pub end_ns: u64,
    pub labels: BTreeMap<String, String>,
}

/// Parse folded stacks into a single report.
///
/// Blank lines and `#` comments are ignored; a line that does not end in an
/// integer is skipped, because tools happily mix in warnings and headers.
pub fn decode(text: &str, opts: &FoldedOptions) -> Result<Report> {
    if opts.end_ns <= opts.start_ns {
        return Err(Error::InvalidRange {
            from: opts.start_ns,
            until: opts.end_ns,
        });
    }
    let mut builder = ReportBuilder::new(
        &opts.service_name,
        opts.profile_type.clone(),
        opts.start_ns,
        opts.end_ns,
    );
    for (key, value) in &opts.labels {
        builder = builder.label(key.clone(), value.clone());
    }

    let mut accepted = 0usize;
    let mut skipped = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Split on the last whitespace: frame names may contain spaces.
        let Some((stack, value)) = line.rsplit_once(char::is_whitespace) else {
            skipped += 1;
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            skipped += 1;
            continue;
        };
        if value == 0 {
            continue;
        }
        let mut frames: Vec<Frame> = stack
            .split(';')
            .map(|frame| frame.trim())
            .filter(|frame| !frame.is_empty())
            .map(|frame| Frame::new("", frame))
            .collect();
        if frames.is_empty() {
            skipped += 1;
            continue;
        }
        // Folded stacks are root first; reports are leaf first.
        frames.reverse();
        builder.add_stack(frames, value);
        accepted += 1;
    }

    if accepted == 0 {
        return Err(Error::Malformed(format!(
            "no folded stacks could be parsed ({skipped} lines skipped)"
        )));
    }
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> FoldedOptions {
        FoldedOptions {
            service_name: "api".into(),
            profile_type: ProfileType::cpu(),
            start_ns: 0,
            end_ns: 1_000,
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn parses_and_reverses_stacks() {
        let report = decode("main;serve;handle 10\nmain;serve;idle 5\n", &opts()).unwrap();
        assert_eq!(report.total_value(), 15);
        assert_eq!(report.stacks.len(), 2);
        let leaf = &report.stacks[0];
        assert_eq!(
            report.strings[leaf.frames[0][1] as usize], "handle",
            "leaf frame comes first"
        );
        assert_eq!(report.strings[leaf.frames[2][1] as usize], "main");
    }

    #[test]
    fn tolerates_noise() {
        let text = "# header\n\nnot a stack\nmain;a 7\n";
        let report = decode(text, &opts()).unwrap();
        assert_eq!(report.total_value(), 7);
    }

    #[test]
    fn rejects_empty_input() {
        assert!(decode("", &opts()).is_err());
        assert!(decode("# only comments\n", &opts()).is_err());
    }

    #[test]
    fn handles_frames_with_spaces() {
        let report = decode("main;my function 3\n", &opts()).unwrap();
        let stack = &report.stacks[0];
        assert_eq!(report.strings[stack.frames[0][1] as usize], "my function");
    }
}
