//! Symbol coverage on a live machine.
//!
//! A flamegraph with a `0x7f3c...` leaf is not a rendering bug: the agent
//! sampled an address it could not name. There are exactly four reasons that
//! happens, and they need different fixes, so this test reports them apart
//! instead of lumping everything into "symbol failures":
//!
//! * `missing_syms`      — the binary is stripped; install debug info.
//! * `missing_component` — anonymous mapping / JIT without a perf map.
//! * `unmapped`          — the address is not in the process any more.
//! * `unknown_addr`      — the file has symbols, but none covers the address.
//!
//! Run it with:
//!
//! ```text
//! DEV_PRIVILEGED=1 ./scripts/dev.sh cargo test -p lidlesseye-ebpf \
//!     --test symbol_coverage -- --ignored --nocapture
//! ```
//!
//! It prints a per-service breakdown and asserts a floor, so a change that
//! silently drops the DWARF inline expansion fails the build instead of just
//! making the flamegraph look slightly worse.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lidlesseye_core::Report;
use lidlesseye_ebpf::{CollectorConfig, start};

#[derive(Default)]
struct ServiceCoverage {
    frames: u64,
    kernel_frames: u64,
    unknown: u64,
    unknown_kernel: u64,
    values: u64,
    /// Unknown leaf addresses, so the report can say *which file* is missing
    /// symbols instead of just "some addresses".
    unknown_addrs: Vec<u64>,
}

/// Find the mapping an address of *this* process belongs to.
fn module_of(addr: u64) -> String {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return "<unreadable>".to_string();
    };
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else { continue };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            continue;
        };
        if addr >= start && addr < end {
            let path: String = fields.skip(3).collect::<Vec<_>>().join(" ");
            return if path.is_empty() {
                "<anonymous>".to_string()
            } else {
                path
            };
        }
    }
    "<unmapped>".to_string()
}

/// A stack leaf that is really an address: the resolver writes `0x<addr>`.
fn is_unknown(symbol: &str) -> bool {
    symbol.starts_with("0x")
}

fn collect(
    seconds: u64,
) -> (
    BTreeMap<String, ServiceCoverage>,
    u64,
    u64,
    lidlesseye_ebpf::StatsSnapshot,
) {
    let stop = std::sync::Arc::new(AtomicU64::new(0));
    let (tx, rx) = std::sync::mpsc::channel();

    // Give the machine something to sample besides itself: one spinning
    // thread (user space symbols) and one sleeping thread (kernel symbols).
    let spin_stop = stop.clone();
    let spinner = std::thread::spawn(move || {
        let mut acc = 0u64;
        while spin_stop.load(Ordering::Relaxed) == 0 {
            for step in 0..4096u64 {
                acc = acc.wrapping_add(step);
            }
            std::hint::black_box(acc);
        }
    });
    let sleeper = std::thread::spawn(|| {
        for _ in 0..12 {
            std::thread::sleep(Duration::from_millis(400));
        }
    });

    let collector = start(
        CollectorConfig {
            service_name: None,
            frequency_hz: 199,
            flush_interval: Duration::from_secs(2),
            collect_cpu: true,
            collect_off_cpu: true,
            exclude_self: false,
            ..Default::default()
        },
        move |report| {
            let _ = tx.send(report);
        },
    )
    .expect("collector should start");

    let mut per_service: BTreeMap<String, ServiceCoverage> = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let Ok(report) = rx.recv_timeout(Duration::from_secs(2)) else {
            continue;
        };
        count_report(&report, &mut per_service);
    }

    let stats = collector.stats();
    collector.stop();
    stop.store(1, Ordering::Relaxed);
    let _ = spinner.join();
    let _ = sleeper.join();

    (
        per_service,
        stats.symbol_frames,
        stats.symbol_inlined,
        stats,
    )
}

fn count_report(report: &Report, per_service: &mut BTreeMap<String, ServiceCoverage>) {
    let entry = per_service.entry(report.service_name.clone()).or_default();
    for stack in &report.stacks {
        entry.values += stack.value;
        for frame in &stack.frames {
            let module = report
                .strings
                .get(frame[0] as usize)
                .map(String::as_str)
                .unwrap_or("");
            let symbol = report
                .strings
                .get(frame[1] as usize)
                .map(String::as_str)
                .unwrap_or("");
            entry.frames += 1;
            let kernel = module == "[kernel]";
            if kernel {
                entry.kernel_frames += 1;
            }
            if is_unknown(symbol) {
                entry.unknown += 1;
                if kernel {
                    entry.unknown_kernel += 1;
                } else if let Ok(addr) = u64::from_str_radix(&symbol[2..], 16) {
                    entry.unknown_addrs.push(addr);
                }
            }
        }
    }
}

#[test]
#[ignore = "requires root and an eBPF capable kernel"]
fn reports_symbol_coverage() {
    let seconds = 12;
    let (per_service, frames, inlined, stats) = collect(seconds);

    let mut totals = ServiceCoverage::default();
    for coverage in per_service.values() {
        totals.frames += coverage.frames;
        totals.kernel_frames += coverage.kernel_frames;
        totals.unknown += coverage.unknown;
        totals.unknown_kernel += coverage.unknown_kernel;
        totals.values += coverage.values;
    }

    let pct = |part: u64, whole: u64| {
        if whole == 0 {
            0.0
        } else {
            100.0 * part as f64 / whole as f64
        }
    };

    println!("\n=== symbol coverage over {seconds}s ===");
    println!(
        "frames={} unknown={} ({:.1}%)  kernel frames={} kernel unknown={} ({:.1}%)",
        totals.frames,
        totals.unknown,
        pct(totals.unknown, totals.frames),
        totals.kernel_frames,
        totals.unknown_kernel,
        pct(totals.unknown_kernel, totals.kernel_frames),
    );
    println!(
        "new frames from DWARF inline info: {} ({:.1}% of named frames)",
        inlined,
        pct(inlined, frames.max(1)),
    );
    println!(
        "named without ELF symbols: {} from Go pclntab, {} from JIT perf maps",
        stats.symbol_go, stats.symbol_jit
    );
    println!(
        "samples dropped: {} process gone, {} pid not visible, {} empty stack",
        stats.symbol_skipped, stats.unattributed, stats.empty_stacks
    );
    println!(
        "unresolved addresses, by reason (total {}):",
        stats.symbol_failures
    );
    for (name, count) in stats.symbol_unknown.entries() {
        if count > 0 {
            println!("  {:<18} {count}", name);
        }
    }

    let mut ranked: Vec<(&String, &ServiceCoverage)> = per_service.iter().collect();
    ranked.sort_by_key(|(_, c)| std::cmp::Reverse(c.unknown));
    println!("\nworst services (unknown frames):");
    for (service, c) in ranked.iter().take(12) {
        println!(
            "  {:>8.1}%  {:>6}/{:<6} frames  {}",
            pct(c.unknown, c.frames),
            c.unknown,
            c.frames,
            service
        );
    }

    // Addresses of our own process can be attributed to a file, which is the
    // difference between "5 % unknown" and "5 % unknown, all of it vdso".
    if let Some(own) = per_service.get("symbol_coverage") {
        let mut by_module: BTreeMap<String, u64> = BTreeMap::new();
        for addr in &own.unknown_addrs {
            *by_module.entry(module_of(*addr)).or_default() += 1;
        }
        if !by_module.is_empty() {
            let mut by_module: Vec<_> = by_module.into_iter().collect();
            by_module.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
            println!("\nunresolved frames of this process, by module:");
            for (module, count) in by_module.iter().take(8) {
                println!("  {count:>6}  {module}");
            }
        }
    }

    assert!(totals.frames > 0, "no frames sampled at all");
    // A machine where a quarter of every frame is a bare address is not being
    // profiled, it is being teased. On a healthy host this number sits around
    // 4 % (all of it stripped `libc`); the threshold exists to catch "the
    // inline expansion stopped working" or "kallsyms became unreadable", not to
    // police small regressions.
    assert!(
        pct(totals.unknown, totals.frames) < 25.0,
        "{:.1}% of frames have no symbol",
        pct(totals.unknown, totals.frames)
    );
}
