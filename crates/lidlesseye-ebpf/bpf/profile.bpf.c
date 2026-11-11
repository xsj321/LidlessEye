// LidlessEye eBPF programs.
//
// Written against the raw eBPF helper ABI with no kernel headers and no
// `vmlinux.h`, for three reasons:
//
//   1. it compiles with a stock `clang -target bpf`, so building LidlessEye
//      does not depend on a kernel source tree or on `bpftool`,
//   2. there is no CO-RE relocation to go stale: the programs only call
//      helpers and never poke at kernel struct layouts,
//   3. the same object file loads on every kernel >= 4.17, whatever the
//      distro compiled in.
//
// The two programs implement:
//
//   * `lidlesseye_cpu`     - on-CPU sampling, driven by a per-CPU software
//                            perf event (PERF_COUNT_SW_CPU_CLOCK).
//   * `lidlesseye_offcpu_*` - off-CPU (blocked) time accounting. The
//                            "blocked" interval is bracketed by
//                            kprobe:schedule (task is about to sleep, still
//                            `current`) and kprobe:finish_task_switch (task
//                            just resumed, now `current`), so both programs
//                            only need `bpf_get_current_pid_tgid()` and never
//                            have to read `task_struct`.

typedef unsigned char __u8;
typedef unsigned int __u32;
typedef unsigned long long __u64;
typedef long long __s64;

#define SEC(name) __attribute__((section(name), used))
#define __always_inline inline __attribute__((always_inline))

#define BPF_MAP_TYPE_HASH 1
#define BPF_MAP_TYPE_STACK_TRACE 7

#define BPF_ANY 0
#define BPF_F_USER_STACK (1ULL << 8)
#define BPF_F_FAST_STACK_CMP (1ULL << 9)

#define PERF_MAX_STACK_DEPTH 127
#define INVALID_STACK_ID 0xffffffffULL

/* ---- helper ABI ------------------------------------------------------- */
static void *(*bpf_map_lookup_elem)(void *map, const void *key) = (void *)1;
static long (*bpf_map_update_elem)(void *map, const void *key, const void *value,
                                   __u64 flags) = (void *)2;
static long (*bpf_map_delete_elem)(void *map, const void *key) = (void *)3;
static __u64 (*bpf_ktime_get_ns)(void) = (void *)5;
static __u64 (*bpf_get_current_pid_tgid)(void) = (void *)14;
static long (*bpf_get_stackid)(void *ctx, void *map, __u64 flags) = (void *)27;

/* ---- BTF map definition macros (libbpf compatible) -------------------- */
#define __uint(name, val) int (*name)[val]
#define __type(name, val) typeof(val) *name

#define MAP_DEF(name)                                                          \
    struct {                                                                   \
        __uint(type, BPF_MAP_TYPE_HASH);                                        \
        __uint(max_entries, name##_MAX);                                        \
        __type(key, struct stack_key);                                          \
        __type(value, __u64);                                                   \
    }

/* Aggregation key: one thread group + one user stack + one kernel stack. */
struct stack_key {
    __u32 tgid;
    __u32 user_stack;
    __u32 kernel_stack;
    __u32 pad;
};

struct stack_entry {
    __u64 ips[PERF_MAX_STACK_DEPTH];
};

/* on-CPU sample counts, weighted by the perf event period in user space */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 32768);
    __type(key, struct stack_key);
    __type(value, __u64);
} cpu_counts SEC(".maps");

/* off-CPU nanoseconds */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 32768);
    __type(key, struct stack_key);
    __type(value, __u64);
} offcpu_totals SEC(".maps");

/* stack id -> instruction pointers, shared by both programs */
struct {
    __uint(type, BPF_MAP_TYPE_STACK_TRACE);
    __uint(max_entries, 16384);
    __type(key, __u32);
    __type(value, struct stack_entry);
} stacks SEC(".maps");

struct offcpu_state {
    __u64 started_ns;
    __u32 stack_id;
    __u32 pad;
};

/* tgid -> timestamp captured at kprobe:schedule */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u32);
    __type(value, struct offcpu_state);
} offcpu_inflight SEC(".maps");

static __always_inline __u32 capture_user_stack(void *ctx) {
    long id = bpf_get_stackid(ctx, (void *)&stacks, BPF_F_USER_STACK | BPF_F_FAST_STACK_CMP);
    if (id < 0) {
        return (__u32)INVALID_STACK_ID;
    }
    return (__u32)id;
}

static __always_inline __u32 capture_kernel_stack(void *ctx) {
    long id = bpf_get_stackid(ctx, (void *)&stacks, BPF_F_FAST_STACK_CMP);
    if (id < 0) {
        return (__u32)INVALID_STACK_ID;
    }
    return (__u32)id;
}

/* Increment `count` for `key` in `map`, creating the entry if needed. */
static __always_inline void add_value(void *map, struct stack_key *key, __u64 value) {
    __u64 *slot = bpf_map_lookup_elem(map, key);
    if (slot) {
        __sync_fetch_and_add(slot, value);
        return;
    }
    __u64 initial = value;
    bpf_map_update_elem(map, key, &initial, BPF_ANY);
}

/* --------------------------------------------------------------------- */
/* On-CPU sampling                                                        */
/* --------------------------------------------------------------------- */
SEC("perf_event")
int lidlesseye_cpu(void *ctx) {
    __u64 pid_tgid = bpf_get_current_pid_tgid();
    __u32 tgid = pid_tgid >> 32;
    if (tgid == 0) {
        return 0;
    }

    struct stack_key key = {};
    key.tgid = tgid;
    key.user_stack = capture_user_stack(ctx);
    key.kernel_stack = capture_kernel_stack(ctx);
    if (key.user_stack == (__u32)INVALID_STACK_ID && key.kernel_stack == (__u32)INVALID_STACK_ID) {
        return 0;
    }
    add_value((void *)&cpu_counts, &key, 1);
    return 0;
}

/* --------------------------------------------------------------------- */
/* Off-CPU accounting: mark the moment a task leaves the CPU              */
/* --------------------------------------------------------------------- */
SEC("kprobe/schedule")
int lidlesseye_offcpu_start(void *ctx) {
    __u64 pid_tgid = bpf_get_current_pid_tgid();
    __u32 tgid = pid_tgid >> 32;
    if (tgid == 0) {
        return 0;
    }

    struct offcpu_state state = {};
    state.started_ns = bpf_ktime_get_ns();
    state.stack_id = capture_user_stack(ctx);
    if (state.stack_id == (__u32)INVALID_STACK_ID) {
        state.stack_id = capture_kernel_stack(ctx);
    }
    bpf_map_update_elem((void *)&offcpu_inflight, &tgid, &state, BPF_ANY);
    return 0;
}

/* --------------------------------------------------------------------- */
/* Off-CPU accounting: account the blocked interval when the task resumes */
/* --------------------------------------------------------------------- */
SEC("kprobe/finish_task_switch")
int lidlesseye_offcpu_end(void *ctx) {
    __u64 pid_tgid = bpf_get_current_pid_tgid();
    __u32 tgid = pid_tgid >> 32;
    if (tgid == 0) {
        return 0;
    }

    struct offcpu_state *state = bpf_map_lookup_elem((void *)&offcpu_inflight, &tgid);
    if (!state) {
        return 0;
    }

    __u64 now = bpf_ktime_get_ns();
    __u64 delta = now - state->started_ns;
    __u32 stack_id = state->stack_id;
    bpf_map_delete_elem((void *)&offcpu_inflight, &tgid);

    /* Ignore sub-millisecond wakeups: they are scheduler noise, and counting
     * them makes the flamegraph dominated by spin-then-sleep loops. */
    if (delta < 1000000ULL) {
        return 0;
    }
    /* A task blocked for over an hour is almost certainly a stuck process;
     * clamp so a single sample cannot dominate the whole profile. */
    if (delta > 3600000000000ULL) {
        delta = 3600000000000ULL;
    }

    struct stack_key key = {};
    key.tgid = tgid;
    key.user_stack = stack_id;
    key.kernel_stack = (__u32)INVALID_STACK_ID;
    add_value((void *)&offcpu_totals, &key, delta);
    return 0;
}

char LICENSE[] SEC("license") = "Apache-2.0";
