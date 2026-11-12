// LidlessEye eBPF programs.
//
// Written against the raw eBPF helper ABI, without kernel headers and without
// `vmlinux.h`:
//
//   1. it compiles with a stock `clang -target bpf`, so building LidlessEye
//      does not depend on a kernel source tree, on `bpftool` or on libbpf,
//   2. the kernel structs it reads are declared inline, one field at a time,
//      and marked `preserve_access_index`. Those accesses are CO-RE: aya
//      rewrites the offsets from `/sys/kernel/btf/vmlinux` at load time, so
//      they follow the layout of the running kernel instead of the one this
//      file was written against,
//   3. `vmlinux.h` is deliberately absent. This file reads three fields of one
//      struct; a 100k-line header would bury which ones matter.
//
// The two programs implement:
//
//   * `lidlesseye_cpu`     - on-CPU sampling, driven by a per-CPU software
//                            perf event (PERF_COUNT_SW_CPU_CLOCK).
//   * `lidlesseye_offcpu_*` - off-CPU (blocked) time accounting. The
//                            "blocked" interval is bracketed by
//                            kprobe:schedule (task is about to sleep, still
//                            `current`) and kprobe:finish_task_switch (task
//                            just resumed, now `current`).
//
// ## How a sample is identified
//
// Every record is keyed by a pid that user space can look up in `/proc`. That
// is the whole difficulty: a pid is only meaningful together with the
// namespace it was read in, and `/proc` shows the tasks of exactly one
// namespace — the reader's. The kernel keeps one number per namespace in
// `task_struct->thread_pid->numbers[]`, so the programs walk that array and
// take the entry belonging to the agent's own namespace. A host agent, or a
// pod with `hostPID: true`, is an ancestor of every container's namespace, so
// every container has such an entry and the whole node can be profiled from
// one agent. A sample whose task lives outside the agent's namespace tree has
// no such entry: it is reported as `unattributed` instead of being given a
// number that would name an unrelated process.

typedef unsigned char __u8;
typedef unsigned int __u32;
typedef int __s32;
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
static long (*bpf_get_stackid)(void *ctx, void *map, __u64 flags) = (void *)27;
static void *(*bpf_get_current_task)(void) = (void *)35;
static long (*bpf_probe_read_kernel)(void *dst, __u32 size, const void *src) = (void *)113;

/* ---- CO-RE: the kernel structs this file reads ----------------------- */
/* Only the fields used below, under the names the kernel gives them.
 * `preserve_access_index` makes clang emit a relocation for every member
 * access instead of a constant offset; aya resolves those against the kernel's
 * own BTF when the object is loaded, so the offsets are right even on a kernel
 * whose struct layout differs from the one this file was written against.
 *
 * The structs are deliberately incomplete: fields are matched by name, so the
 * ones that are not declared here cost nothing.
 *
 * `numbers` is a flexible array member and must stay the last field of
 * `struct pid`: the kernel indexes it by PID-namespace level, and clang only
 * emits an in-bounds relocation for it while it is the trailing member. */
struct ns_common {
    __u64 ns_type;
    __u64 stashed;
    __u64 ops;
    __s32 inum;
} __attribute__((preserve_access_index));

struct pid_namespace {
    struct ns_common ns;
} __attribute__((preserve_access_index));

struct upid {
    __s32 nr;
    __u32 pad;
    struct pid_namespace *ns;
} __attribute__((preserve_access_index));

struct pid {
    __u32 count;
    __u32 level;
    struct upid numbers[];
} __attribute__((preserve_access_index));

struct task_struct {
    struct task_struct *group_leader;
    struct pid *thread_pid;
} __attribute__((preserve_access_index));

/* `ns_common.inum` of the PID namespace the agent's `/proc` belongs to.
 *
 * Filled in by the loader from `/proc/self/ns/pid`. Zero means "not
 * configured", in which case the programs report the pid from the initial
 * namespace — the number `bpf_get_current_pid_tgid()` would have produced. */
const volatile __u32 agent_ns_inum = 0;

/* ---- BTF map definition macros (libbpf compatible) -------------------- */
#define __uint(name, val) int (*name)[val]
#define __type(name, val) typeof(val) *name

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

/* Typed context for kprobes.
 *
 * The verifier rejects `void *ctx` when it is handed to a helper that takes
 * `ARG_PTR_TO_CTX` ("reference type size cannot be determined"), because it
 * cannot prove what the pointer points to. Declaring the kernel's `pt_regs`
 * shape gives it a sized type; the programs never dereference it. */
struct pt_regs_opaque {
    unsigned long regs[64];
};

/* Thread group id as the agent's `/proc` names it, or 0 when the task is not
 * visible in the agent's PID namespace.
 *
 * A pid does not identify a task on its own: the kernel keeps one number per
 * PID namespace in `task_struct->thread_pid->numbers[]`, indexed by the
 * namespace's level (`0` = the initial namespace). The agent can only read
 * `/proc/<pid>` for a task that lives in its own namespace or in a descendant
 * of it, and the number it must use is the one belonging to *its* namespace —
 * that is the entry whose `numbers[].ns` is the agent's namespace.
 *
 * Hence the walk below. It is what makes one agent able to profile a whole
 * node: on a host, or with `hostPID: true`, the agent's namespace is a
 * (possibly distant) ancestor of every container's, so each of them has an
 * entry that matches. Reading `task_struct->tgid` instead — or calling
 * `bpf_get_current_pid_tgid()`, which returns `numbers[0]` — yields the number
 * from the *initial* namespace, which `/proc` shows only when the agent's
 * namespace happens to be that one; anywhere else it names an unrelated
 * process, or nothing at all in a container-based node such as kind.
 *
 * The levels are unrolled because a CO-RE relocation needs a constant array
 * index. Deeper first: a task is normally one level below the agent (a pod on
 * its node), so it usually matches on the first or second try. */
#define PID_NS_MAX_LEVEL 7

/* If `pid` has a namespace at level `k` and it is the agent's namespace, the
 * comment of `current_tgid` says the rest: return that level's number. */
#define MATCH_LEVEL(k)                                                          \
    do {                                                                        \
        struct pid_namespace *ns = 0;                                        \
        __s32 inum = 0;                                                         \
        __s32 nr = 0;                                                           \
        if ((k) <= level &&                                                     \
            bpf_probe_read_kernel(&ns, sizeof(ns), &pid->numbers[k].ns) == 0 &&  \
            ns != 0 &&                                                       \
            bpf_probe_read_kernel(&inum, sizeof(inum), &ns->ns.inum) == 0 &&     \
            (__u32)inum == agent_ns_inum &&                                     \
            bpf_probe_read_kernel(&nr, sizeof(nr), &pid->numbers[k].nr) == 0 &&  \
            nr > 0) {                                                           \
            return (__u32)nr;                                                   \
        }                                                                       \
    } while (0)

static __always_inline __u32 current_tgid(void) {
    struct task_struct *task = bpf_get_current_task();
    if (!task) {
        return 0;
    }
    /* The *thread group* id, not the calling thread's: `thread_pid` belongs to
     * the individual thread, and keying on it would turn every worker thread
     * into a series of its own. The leader's pid is the tgid. */
    struct task_struct *leader = 0;
    if (bpf_probe_read_kernel(&leader, sizeof(leader), &task->group_leader) != 0 || leader == 0) {
        leader = task;
    }
    struct pid *pid = 0;
    if (bpf_probe_read_kernel(&pid, sizeof(pid), &leader->thread_pid) != 0 || pid == 0) {
        return 0;
    }

    if (agent_ns_inum == 0) {
        /* Not configured: the initial namespace's number, i.e. what
         * `bpf_get_current_pid_tgid()` returns. */
        __s32 nr = 0;
        if (bpf_probe_read_kernel(&nr, sizeof(nr), &pid->numbers[0].nr) != 0 || nr <= 0) {
            return 0;
        }
        return (__u32)nr;
    }

    __u32 level = 0;
    if (bpf_probe_read_kernel(&level, sizeof(level), &pid->level) != 0) {
        return 0;
    }
    if (level > PID_NS_MAX_LEVEL) {
        level = PID_NS_MAX_LEVEL;
    }

    MATCH_LEVEL(7);
    MATCH_LEVEL(6);
    MATCH_LEVEL(5);
    MATCH_LEVEL(4);
    MATCH_LEVEL(3);
    MATCH_LEVEL(2);
    MATCH_LEVEL(1);
    MATCH_LEVEL(0);
    return 0;
}

static __always_inline __u32 capture_user_stack(struct pt_regs_opaque *ctx) {
    long id = bpf_get_stackid(ctx, (void *)&stacks, BPF_F_USER_STACK | BPF_F_FAST_STACK_CMP);
    if (id < 0) {
        return (__u32)INVALID_STACK_ID;
    }
    return (__u32)id;
}

static __always_inline __u32 capture_kernel_stack(struct pt_regs_opaque *ctx) {
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
int lidlesseye_cpu(struct pt_regs_opaque *ctx) {
    __u32 tgid = current_tgid();
    if (tgid == 0) {
        /* The task cannot be named in the agent's `/proc` — in the namespaced
         * view that is every task outside the agent's own container. Account
         * the sample under the reserved tgid 0 so user space can report the
         * loss instead of dropping it silently. */
        struct stack_key lost = {};
        lost.user_stack = (__u32)INVALID_STACK_ID;
        lost.kernel_stack = (__u32)INVALID_STACK_ID;
        add_value((void *)&cpu_counts, &lost, 1);
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
int lidlesseye_offcpu_start(struct pt_regs_opaque *ctx) {
    __u32 tgid = current_tgid();
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
int lidlesseye_offcpu_end(struct pt_regs_opaque *ctx) {
    __u32 tgid = current_tgid();
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

/* The kernel requires an explicit license for GPL-only helpers, and
 * `bpf_get_stackid` is one of them. The eBPF program is therefore licensed
 * GPL-2.0-only (see LICENSE-BPF); every user space crate in this workspace
 * remains Apache-2.0. This split is the same one used by bpftrace,
 * parca-agent and the OpenTelemetry eBPF profiler. */
char LICENSE[] SEC("license") = "GPL";
