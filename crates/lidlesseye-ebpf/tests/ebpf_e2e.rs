//! End-to-end tests for the eBPF programs.
//!
//! These tests load real BPF programs and sample the machine, so they are
//! `#[ignore]`d by default and need root:
//!
//! ```text
//! DEV_PRIVILEGED=1 DEV_ENV="RUSTFLAGS=-C force-frame-pointers=yes" \
//!     ./scripts/dev.sh cargo test -p lidlesseye-ebpf --test ebpf_e2e \
//!     -- --ignored --nocapture
//! ```
//!
//! The frame-pointer flag is not optional for the attribution test: without it
//! a sample that lands inside a monomorphised `std` helper cannot be unwound
//! past its leaf, and the CPU time is attributed to that helper instead of to
//! the function that called it. Measured on this suite: 95 % of a spinning
//! thread's time reaches `busy_loop` with frame pointers, 20 % without.

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

/// Spin until `deadline`, with the sampled work in a tight arithmetic loop.
///
/// The clock is read once per 65k iterations, not every iteration, on purpose:
/// `std` ships precompiled without frame pointers, so a sample that lands
/// inside `Instant::now` cannot be unwound past its leaf. That is a real
/// limitation of stack unwinding (see docs/compatibility.md), but it makes a
/// poor oracle for "did the profiler attribute the time correctly", so the
/// benchmark keeps its hot path in this function.
#[inline(never)]
fn busy_loop(deadline: Instant) -> u64 {
    let mut acc = 0u64;
    loop {
        for step in 0..65_536u64 {
            acc = acc.wrapping_add(step);
        }
        std::hint::black_box(acc);
        if Instant::now() >= deadline {
            return acc;
        }
    }
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

/// The CPU a thread actually burned, straight from the kernel.
///
/// Wall-clock time is the wrong oracle: on a busy host (or a VM with more
/// vCPUs than cores) a spinning thread may only be scheduled for 80 % of the
/// wall time, and the profiler would look like it lost 20 % of the samples.
fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` only writes the `timespec` it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Sampling must account for the *time*, not merely produce records.
///
/// A collector that drops half the samples, or multiplies by the wrong period,
/// still produces a plausible-looking flamegraph — just a wrong one. The
/// profile's value for `busy_loop` is compared against the thread's own CPU
/// time, which is the number the profiler is trying to reproduce.
///
/// Requires a frame-pointer build (`RUSTFLAGS=-C force-frame-pointers=yes`);
/// see the module docs for why, and for the numbers without it.
#[test]
#[ignore = "requires root and an eBPF capable kernel"]
fn cpu_samples_add_up_to_the_time_the_thread_ran() {
    let busy_for = Duration::from_secs(6);
    let worker = std::thread::spawn(move || {
        let started = thread_cpu_ns();
        busy_loop(Instant::now() + busy_for);
        thread_cpu_ns() - started
    });

    let (tx, rx) = std::sync::mpsc::channel();
    let collector = start(
        CollectorConfig {
            service_name: Some("ebpf-e2e-accuracy".to_string()),
            frequency_hz: 199,
            flush_interval: Duration::from_secs(1),
            collect_cpu: true,
            collect_off_cpu: false,
            exclude_self: false,
            ..Default::default()
        },
        move |report| {
            let _ = tx.send(report);
        },
    )
    .expect("collector should start");

    let mut busy_ns: u64 = 0;
    let mut everything_ns: u64 = 0;
    let mut missing_reports: Vec<lidlesseye_core::Report> = Vec::new();
    let deadline = Instant::now() + busy_for + Duration::from_secs(3);
    while Instant::now() < deadline {
        let Ok(report) = rx.recv_timeout(Duration::from_secs(2)) else {
            continue;
        };
        for stack in &report.stacks {
            everything_ns += stack.value;
            let touches_busy_loop = stack.frames.iter().any(|frame| {
                report
                    .strings
                    .get(frame[1] as usize)
                    .is_some_and(|symbol| symbol.contains("busy_loop"))
            });
            if touches_busy_loop {
                busy_ns += stack.value;
            }
        }
        missing_reports.push(report);
    }

    let stats = collector.stats();
    collector.stop();
    let measured_ns = worker.join().expect("the busy thread finished");

    // Diagnostic: what are the samples that did *not* mention busy_loop?
    // A sample lost in the kernel and a sample attributed to the wrong stack
    // look identical in the total, so name the leaves before guessing.
    let mut leaves: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut missing_ns: u64 = 0;
    for report in &missing_reports {
        for stack in &report.stacks {
            let touches = stack.frames.iter().any(|f| {
                report
                    .strings
                    .get(f[1] as usize)
                    .is_some_and(|s| s.contains("busy_loop"))
            });
            if touches {
                continue;
            }
            missing_ns += stack.value;
            let leaf = stack
                .frames
                .first()
                .and_then(|f| report.strings.get(f[1] as usize))
                .map(|symbol| symbol.to_string())
                .unwrap_or_else(|| "<no frames>".to_string());
            *leaves.entry(leaf).or_default() += stack.value;
        }
    }
    if !leaves.is_empty() {
        println!("not busy_loop: {:.2}s", missing_ns as f64 / 1e9);
        let mut ranked: Vec<_> = leaves.into_iter().collect();
        ranked.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
        for (leaf, value) in ranked.iter().take(6) {
            println!("  {:>8.2}s  {leaf}", *value as f64 / 1e9);
        }
    }

    let profiled = busy_ns as f64 / 1e9;
    let measured = measured_ns as f64 / 1e9;
    let total = everything_ns as f64 / 1e9;
    println!(
        "busy_loop: profiled={profiled:.2}s measured={measured:.2}s \
         total-cpu={total:.2}s records={} spans={}",
        stats.cpu_records, stats.reports
    );

    // The tolerance is sampling noise (one period is 5 ms) plus the chance
    // that a sample lands one frame outside `busy_loop`, e.g. inside the
    // inlined `Instant::now` call.
    let tolerance = (measured * 0.15).max(0.25);
    assert!(
        (profiled - measured).abs() <= tolerance,
        "profiler says {profiled:.2}s, the thread burned {measured:.2}s \
         (tolerance ±{tolerance:.2}s)"
    );
    assert!(
        stats.cpu_records > 0,
        "no samples, so the assertion above proves nothing"
    );
}

/// A process in *another* PID namespace must be profiled under its own name.
///
/// This is the whole-cluster case: the agent runs with `hostPID: true`, every
/// other pod lives in its own PID namespace, and the only pid `/proc` can
/// resolve for those tasks is the one the kernel keeps for the *initial*
/// namespace (`task_struct->tgid`). Translating through the agent's own
/// namespace instead, which is what the object used to do unconditionally,
/// answers "not visible" for every one of them: the samples then land in
/// `unattributed` and the cluster looks idle.
///
/// Runs anywhere: the burner is put in a child of *this* PID namespace, which
/// is the same relation a pod has to a `hostPID` agent, so the test does not
/// need to be on a particular node.
#[test]
#[ignore = "requires root, an eBPF capable kernel, and --pid=host"]
fn profiles_a_process_in_another_pid_namespace() {
    use lidlesseye_ebpf::procfs;

    if procfs::pid_namespace_inum().is_none() {
        eprintln!("skipping: no /proc/self/ns/pid to identify this namespace");
        return;
    }

    // A copy of `yes` under a unique name: the report can then only be about
    // this test, and the process saturates a CPU for as long as it runs.
    let source = ["/usr/bin/yes", "/bin/yes"]
        .iter()
        .map(std::path::Path::new)
        .find(|path| path.exists())
        .expect("no `yes` binary to burn CPU with");
    let burner = std::env::temp_dir().join("lidlesseye-ns-burner");
    std::fs::copy(source, &burner).expect("copy the burner");
    // 15 characters, the most the kernel keeps in `comm`.
    let expected = "lidlesseye-ns-b";

    // `timeout` outlives the test run, so an assertion failure cannot leave a
    // process spinning on someone's machine.
    let mut child = std::process::Command::new("unshare")
        .args(["--pid", "--fork", "--mount-proc", "timeout", "30"])
        .arg(&burner)
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn unshare");

    let (tx, rx) = std::sync::mpsc::channel();
    let collector = start(
        CollectorConfig {
            service_name: None,
            frequency_hz: 199,
            flush_interval: Duration::from_secs(1),
            collect_cpu: true,
            collect_off_cpu: false,
            ..Default::default()
        },
        move |report| {
            let _ = tx.send(report);
        },
    )
    .expect("collector should start");

    let mut seen: Vec<String> = Vec::new();
    let mut found = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && !found {
        let Ok(report) = rx.recv_timeout(Duration::from_secs(2)) else {
            continue;
        };
        if report.service_name == expected {
            found = true;
        } else if seen.len() < 8 && !seen.contains(&report.service_name) {
            seen.push(report.service_name.clone());
        }
    }

    let stats = collector.stats();
    collector.stop();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&burner);

    println!(
        "cpu records={} unattributed={} skipped={} namespaced test: {}",
        stats.cpu_records,
        stats.unattributed,
        stats.symbol_skipped,
        if found { "attributed" } else { "missing" }
    );
    assert!(stats.cpu_records > 0, "no samples captured at all");
    // A handful of samples always lands in kernel threads, and those exist only
    // in the *initial* namespace: when the agent itself is one level down (as
    // here) they have no name it could use. Everything else must be named.
    assert!(
        stats.unattributed * 10 < stats.cpu_records,
        "{} of {} samples were dropped as unattributable",
        stats.unattributed,
        stats.cpu_records
    );
    assert!(
        found,
        "`{expected}` never appeared in a report; saw {:?}",
        seen
    );
}
