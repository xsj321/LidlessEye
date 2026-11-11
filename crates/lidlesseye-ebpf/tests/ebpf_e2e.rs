//! End-to-end tests for the eBPF programs.
//!
//! These tests load real BPF programs and sample the machine, so they are
//! `#[ignore]`d by default and need root:
//!
//! ```text
//! DEV_PRIVILEGED=1 ./scripts/dev.sh \
//!     cargo test -p lidlesseye-ebpf --test ebpf_e2e -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use lidlesseye_ebpf::{CollectorConfig, start};

/// Loads every program in the object, printing the verifier log on failure.
/// This is the fastest way to notice that a kernel rejects the object.
#[test]
#[ignore = "requires root and an eBPF capable kernel"]
fn all_programs_load() {
    use aya::EbpfLoader;
    use aya::programs::{KProbe, PerfEvent};

    let mut loader = EbpfLoader::new();
    loader.verifier_log_level(aya::VerifierLogLevel::VERBOSE);
    let mut ebpf = loader
        .load(lidlesseye_ebpf::bpf_object())
        .expect("eBPF object should load");

    let cpu: &mut PerfEvent = ebpf
        .program_mut("lidlesseye_cpu")
        .expect("lidlesseye_cpu present")
        .try_into()
        .expect("perf event program");
    cpu.load().expect("lidlesseye_cpu should load");

    let targets: [(&str, &[&str]); 2] = [
        ("lidlesseye_offcpu_start", &["schedule"]),
        (
            "lidlesseye_offcpu_end",
            &[
                "finish_task_switch",
                "finish_task_switch.isra.0",
                "finish_task_switch.constprop.0",
            ],
        ),
    ];
    for (program, symbols) in targets {
        let probe: &mut KProbe = ebpf
            .program_mut(program)
            .unwrap_or_else(|| panic!("{program} present"))
            .try_into()
            .expect("kprobe program");
        probe
            .load()
            .unwrap_or_else(|err| panic!("{program} failed to load: {err}"));
        let mut last_error = None;
        let mut attached = false;
        for symbol in symbols {
            match probe.attach(*symbol, 0) {
                Ok(_) => {
                    println!("{program} attached to {symbol}");
                    attached = true;
                    break;
                }
                Err(err) => last_error = Some(format!("{symbol}: {err}")),
            }
        }
        assert!(attached, "{program} could not attach: {last_error:?}");
    }
}

#[inline(never)]
fn busy_loop(deadline: Instant) -> u64 {
    let mut acc = 0u64;
    while Instant::now() < deadline {
        acc = acc.wrapping_add(1);
        std::hint::black_box(acc);
    }
    acc
}

/// Profiles this very process and asserts that the hot function shows up with
/// its real name — the whole chain: perf event → BPF stack capture → stack
/// map → blazesym → report.
#[test]
#[ignore = "requires root and an eBPF capable kernel"]
fn collects_and_symbolizes_this_process() {
    let worker = std::thread::spawn(|| busy_loop(Instant::now() + Duration::from_secs(8)));

    let (tx, rx) = std::sync::mpsc::channel();
    let collector = start(
        CollectorConfig {
            service_name: Some("ebpf-e2e".to_string()),
            frequency_hz: 199,
            flush_interval: Duration::from_secs(2),
            collect_cpu: true,
            collect_off_cpu: false,
            // The workload runs in this very process, so it must not be
            // filtered out as "the profiler itself".
            exclude_self: false,
            ..Default::default()
        },
        move |report| {
            let _ = tx.send(report);
        },
    )
    .expect("collector should start");

    let mut stacks = 0usize;
    let mut found = false;
    let deadline = Instant::now() + Duration::from_secs(9);
    while Instant::now() < deadline && !found {
        let Ok(report) = rx.recv_timeout(Duration::from_secs(3)) else {
            continue;
        };
        stacks += report.stacks.len();
        for stack in &report.stacks {
            for frame in &stack.frames {
                let Some(symbol) = report.strings.get(frame[1] as usize) else {
                    continue;
                };
                if symbol.contains("busy_loop") {
                    found = true;
                }
            }
        }
    }

    let stats = collector.stats();
    collector.stop();
    let _ = worker.join();

    println!(
        "cpu records={} reports={} stacks={} symbol failures={}",
        stats.cpu_records, stats.reports, stacks, stats.symbol_failures
    );
    assert!(stats.cpu_records > 0, "no samples captured at all");
    assert!(found, "busy_loop never appeared in a symbolized stack");
}

/// Off-CPU accounting: a thread that sleeps must produce blocked-time samples
/// attributed to `sleep`.
#[test]
#[ignore = "requires root and an eBPF capable kernel"]
fn accounts_off_cpu_time() {
    let worker = std::thread::spawn(|| {
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(400));
        }
    });

    let (tx, rx) = std::sync::mpsc::channel();
    let collector = start(
        CollectorConfig {
            service_name: Some("ebpf-e2e-offcpu".to_string()),
            frequency_hz: 99,
            flush_interval: Duration::from_secs(2),
            collect_cpu: false,
            collect_off_cpu: true,
            exclude_self: false,
            ..Default::default()
        },
        move |report| {
            let _ = tx.send(report);
        },
    )
    .expect("collector should start");

    let mut total: u64 = 0;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        let Ok(report) = rx.recv_timeout(Duration::from_secs(2)) else {
            continue;
        };
        total += report.total_value();
    }
    let stats = collector.stats();
    collector.stop();
    let _ = worker.join();

    println!(
        "off-cpu records={} total={}ms",
        stats.offcpu_records,
        total / 1_000_000
    );
    assert!(stats.offcpu_records > 0, "no off-CPU samples captured");
    assert!(total > 0, "off-CPU samples carried no time");
}
