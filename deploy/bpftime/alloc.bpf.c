// Malloc counter for the userspace eBPF runtime (bpftime).
//
// The same question the kernel uprobe reader answered, asked without a kernel
// uprobe: `malloc` is hooked inside the process by the injected bpftime agent,
// so a call costs a few hundred nanoseconds instead of the 79 microseconds a
// kernel uprobe trap measured on this cluster.
//
// Keys are `(pid, stack)`, so the loader can attribute bytes to a stack and the
// agent can label them by pod. Only `alloc_space` is produced: `free` is not
// hooked, so "in use" would be a guess. See `loader.c`.
#define BPF_NO_GLOBAL_DATA
#include <vmlinux.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

// Enough to see a C allocation site below its framework; deeper stacks are
// truncated by the kernel and by the loader.
#define MAX_STACK_DEPTH 32

// One in SAMPLING_RATE allocations is recorded, and each recorded allocation
// is scaled back up by SAMPLING_RATE so the totals stay unbiased.
//
// The decision is a power-of-two mask on a random number: that is sampling
// "with replacement" at a fixed probability, i.e. a geometric (Poisson)
// count of skips between samples, and it costs one random draw. Doing the
// arithmetic properly (`-ln(u) / p`) would need floating point inside the
// hook, which costs more than the stack unwind it is trying to save.
//
// Why sample at all: the unwind is the expensive part (`bpf_get_stackid`),
// measured at roughly 520 ns against a 50 ns hook. At 1/64 the probe costs
// about 58 ns per allocation on average.
//
// The rate lives in a map so one build can be benchmarked at several rates
// without recompiling: `cfg[0] = 1` is "instrument every call", a huge value is
// "hook only, never unwind". SAMPLING_RATE is the default when the loader does
// not set it.
#define SAMPLING_RATE 64

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, __u64);
} cfg SEC(".maps");

struct alloc_key {
	__u32 pid;
	__s32 stack;
};

struct alloc_val {
	__u64 bytes;
	__u64 count;
};

struct {
	__uint(type, BPF_MAP_TYPE_STACK_TRACE);
	__uint(key_size, sizeof(__u32));
	__uint(value_size, MAX_STACK_DEPTH * sizeof(__u64));
	__uint(max_entries, 8192);
} stacks SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 16384);
	__type(key, struct alloc_key);
	__type(value, struct alloc_val);
} allocs SEC(".maps");

// Every invocation of the program increments this, whatever else happens. The
// agent uses it to tell "the probe never ran" apart from "the probe ran but the
// map or the shared memory lost the data" - two very different bugs.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 16384);
	__type(key, __u32);
	__type(value, __u64);
} hits SEC(".maps");

// How many samples this process has produced, so the loader can show the
// sampling ratio next to the raw call count.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 16384);
	__type(key, __u32);
	__type(value, __u64);
} sampled SEC(".maps");

SEC("uprobe/libc.so.6:malloc")
int count_malloc(struct pt_regs *ctx)
{
	__u32 pid = bpf_get_current_pid_tgid() >> 32;
	__u64 *hit = bpf_map_lookup_elem(&hits, &pid);
	if (hit) {
		__sync_fetch_and_add(hit, 1ULL);
	} else {
		__u64 one = 1;
		bpf_map_update_elem(&hits, &pid, &one, BPF_NOEXIST);
	}

	// The rate is read per call rather than cached: one array lookup is a few
	// nanoseconds, and it keeps a single build usable for every benchmark.
	__u32 zero = 0;
	__u64 *configured = bpf_map_lookup_elem(&cfg, &zero);
	__u64 rate = (configured && *configured > 0) ? *configured : SAMPLING_RATE;
	if (rate > (1u << 20)) {
		rate = 1u << 20;
	}
	__u64 mask = rate - 1;

	// The cheap part ends here for all but one call in `rate`: no unwind, no
	// map lookup on the hot key.
	if ((bpf_get_prandom_u32() & (__u32)mask) != 0)
		return 0;

	__u64 *taken = bpf_map_lookup_elem(&sampled, &pid);
	if (taken) {
		__sync_fetch_and_add(taken, 1ULL);
	} else {
		__u64 one = 1;
		bpf_map_update_elem(&sampled, &pid, &one, BPF_NOEXIST);
	}

	// Every recorded allocation stands for `rate` of them.
	__u64 size = PT_REGS_PARM1(ctx) * rate;

	struct alloc_key key = {};
	key.pid = pid;
	// Negative ids mean "no stack" (map full, or the unwind failed); they are
	// still counted, because the bytes are real even when the site is unknown.
	key.stack = bpf_get_stackid(ctx, &stacks, BPF_F_USER_STACK);

	struct alloc_val *val = bpf_map_lookup_elem(&allocs, &key);
	if (val) {
		__sync_fetch_and_add(&val->bytes, size);
		__sync_fetch_and_add(&val->count, rate);
		return 0;
	}

	struct alloc_val fresh = { .bytes = size, .count = rate };
	bpf_map_update_elem(&allocs, &key, &fresh, BPF_NOEXIST);
	return 0;
}

char LICENSE[] SEC("license") = "GPL";
