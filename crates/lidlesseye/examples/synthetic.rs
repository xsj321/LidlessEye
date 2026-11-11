//! Produce profiles without eBPF and query them back.
//!
//! Two things are demonstrated here:
//!
//! 1. `ReportBuilder` is a public API, so anything that can produce stack
//!    samples (a language runtime, a JIT, a fuzzer, a batch job) can push them
//!    into LidlessEye — either through [`LidlessEye::ingest`] or over HTTP.
//! 2. The query engine is usable without the server: `flamegraph`, `top` and
//!    `timeseries` take a `&Store` and return plain structs.
//!
//! ```text
//! cargo run --example synthetic
//! ```

use lidlesseye_core::{
    Frame, ProfileType, QueryOptions, ReportBuilder, Selector, Store, StoreConfig, flamegraph,
    now_ns, top,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join("lidlesseye-synthetic-example");
    let _ = std::fs::remove_dir_all(&dir);

    let store = Store::open(StoreConfig::new(&dir))?;
    let now = now_ns();

    // A tiny synthetic call tree: main -> handle_request -> {parse, render}.
    for minute in 0..5 {
        let end = now - (4 - minute) * 60_000_000_000;
        let start = end - 60_000_000_000;
        let mut builder = ReportBuilder::new("examples", ProfileType::cpu(), start, end)
            .label("region", "eu-west")
            .sample_rate_hz(99);

        let frames = |leaf: &str| {
            vec![
                Frame::new("/app/server", leaf),
                Frame::new("/app/server", "handle_request"),
                Frame::new("/app/server", "main"),
            ]
        };
        // Values are nanoseconds of CPU time, not sample counts.
        builder.add_stack(frames("parse_json"), 120_000_000 + minute * 10_000_000);
        builder.add_stack(frames("render_html"), 340_000_000);
        builder.add_stack(frames("await_postgres"), 90_000_000);

        let report = builder.finish();
        report.validate()?;
        store.append(&report)?;
    }

    println!(
        "stored {} segments in {}",
        store.stats().segment_count,
        dir.display()
    );

    let opts = QueryOptions::new(ProfileType::cpu(), 0, u64::MAX, Selector::default());
    let fg = flamegraph(&store, &opts)?;
    println!(
        "\nflamegraph: total {} in {} frames ({} unit)",
        format_ns(fg.total),
        fg.names.len(),
        fg.unit
    );
    print_tree(&fg.to_tree()[0], 0, fg.total);

    let result = top(&store, &opts, 5)?;
    println!(
        "\ntop functions ({} total):\n{:<24}{:>12}{:>12}{:>9}",
        format_ns(result.total_value),
        "FUNCTION",
        "SELF",
        "TOTAL",
        "SHARE"
    );
    for row in &result.items {
        println!(
            "{:<24}{:>12}{:>12}{:>8.1}%",
            row.name,
            format_ns(row.self_value),
            format_ns(row.total_value),
            row.total_value as f64 / result.total_value as f64 * 100.0
        );
    }

    // Selectors work on labels, so one binary can serve several tenants.
    let only_eu = QueryOptions::new(
        ProfileType::cpu(),
        0,
        u64::MAX,
        Selector::parse(r#"{region="eu-west"}"#)?,
    );
    let filtered = flamegraph(&store, &only_eu)?;
    println!(
        "\n{} matches {{region=\"eu-west\"}}",
        format_ns(filtered.total)
    );

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

fn print_tree(node: &lidlesseye_core::TreeNode, depth: usize, total: i64) {
    let share = node.value as f64 / total.max(1) as f64 * 100.0;
    if share < 1.0 {
        return;
    }
    println!(
        "{:indent$}{} — {} ({share:.1}%)",
        "",
        node.name,
        format_ns(node.value),
        indent = depth * 2
    );
    for child in &node.children {
        print_tree(child, depth + 1, total);
    }
}

fn format_ns(ns: i64) -> String {
    let ns = ns.max(0) as f64;
    if ns >= 1e9 {
        format!("{:.2}s", ns / 1e9)
    } else if ns >= 1e6 {
        format!("{:.0}ms", ns / 1e6)
    } else if ns >= 1e3 {
        format!("{:.0}µs", ns / 1e3)
    } else {
        format!("{ns:.0}ns")
    }
}
