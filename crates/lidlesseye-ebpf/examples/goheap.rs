// Read a Go process's heap profile the way the runtime would print it.
//
//   cargo run --example goheap -- 1234 5678    # specific pids
//   cargo run --example goheap                 # every Go process visible
//
// Output is a one-line summary plus folded text (stack;bytes) for the biggest
// stacks, which makes it diffable against `go tool pprof -top` for the same
// process and usable as input to `flamegraph.pl`.

use std::collections::HashMap;

use lidlesseye_ebpf::goheap::GoHeap;

fn main() {
    let wanted: Vec<u32> = std::env::args()
        .skip(1)
        .filter_map(|arg| arg.parse().ok())
        .collect();

    // `scan` takes an exclusion list, and this tool normally runs beside the
    // processes it reads rather than above them: with explicit pids everything
    // else is excluded, without them only this process is.
    let exclude = if wanted.is_empty() {
        vec![std::process::id()]
    } else {
        pids()
            .into_iter()
            .filter(|pid| !wanted.contains(pid))
            .collect()
    };

    let mut heap = GoHeap::new();
    let mut alive = Vec::new();
    let samples = heap.scan(&exclude, &mut alive);

    let mut inuse: HashMap<Vec<u64>, u64> = HashMap::new();
    let mut alloc: HashMap<Vec<u64>, u64> = HashMap::new();
    let (mut alloc_total, mut inuse_total) = (0u64, 0u64);
    for sample in &samples {
        *alloc.entry(sample.stack.clone()).or_default() += sample.alloc_bytes;
        *inuse.entry(sample.stack.clone()).or_default() += sample.inuse_bytes;
        alloc_total += sample.alloc_bytes;
        inuse_total += sample.inuse_bytes;
    }

    println!(
        "processes={} samples={} alloc_bytes={} inuse_bytes={} unreadable={} disabled={} invalid_buckets={}",
        alive.len(),
        samples.len(),
        alloc_total,
        inuse_total,
        heap.unreadable,
        heap.disabled,
        heap.invalid_buckets
    );
    if !heap.skipped.is_empty() {
        println!("skipped: {:?}", heap.skipped);
    }

    let mut stacks: Vec<(&Vec<u64>, &u64)> = inuse.iter().collect();
    stacks.sort_by(|a, b| b.1.cmp(a.1));
    for (stack, bytes) in stacks.iter().take(40) {
        let line: Vec<String> = stack.iter().map(|pc| format!("{pc:x}")).collect();
        println!("{};{}", line.join(";"), bytes);
    }
}

fn pids() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        if let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
            out.push(pid);
        }
    }
    out
}
