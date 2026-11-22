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
#define BPF_MAP_TYPE_ARRAY 2
#define BPF_MAP_TYPE_STACK_TRACE 7
#define BPF_MAP_TYPE_LRU_HASH 9

#define BPF_ANY 0
#define BPF_NOEXIST 1
#define BPF_F_USER_STACK (1ULL << 8)
#define BPF_F_FAST_STACK_CMP (1ULL << 9)

#define PERF_MAX_STACK_DEPTH 127
#define INVALID_STACK_ID 0xffffffffULL

/* ---- helper ABI ------------------------------------------------------- */
/* `const` is not decoration: without it clang keeps the pointer in .data and
 * emits an indirect call whose target the verifier cannot resolve, which it
 * rejects as `invalid func unknown#<address>`. `net.bpf.c` carries the same
 * note; the sampling programs below happened to constant-fold every call until
 * the allocator programs called a helper twice through one pointer. */
static void *(*const bpf_map_lookup_elem)(void *map, const void *key) = (void *)1;
static long (*const bpf_map_update_elem)(void *map, const void *key, const void *value,
                                         __u64 flags) = (void *)2;
static long (*const bpf_map_delete_elem)(void *map, const void *key) = (void *)3;
static __u64 (*const bpf_ktime_get_ns)(void) = (void *)5;
static long (*const bpf_get_stackid)(void *ctx, void *map, __u64 flags) = (void *)27;
/* Copies frames into a buffer instead of interning them in a stack map, which
 * is what the allocator needs: the map's 32-bit ids are too narrow to key an
 * aggregation table (see `alloc_stacks`). */
static long (*const bpf_get_stack)(void *ctx, void *buf, __u32 size, __u64 flags) = (void *)67;
static void *(*const bpf_get_current_task)(void) = (void *)35;
static __u64 (*const bpf_get_current_pid_tgid)(void) = (void *)14;
static long (*const bpf_probe_read_user)(void *dst, __u32 size, const void *src) = (void *)112;
static long (*const bpf_probe_read_kernel)(void *dst, __u32 size, const void *src) = (void *)113;

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

/* Which of the two stacks `stack_id` indexes.
 *
 * Both kinds share the `stacks` map, so an id alone cannot say whether it walks
 * user space or the kernel. The off-CPU path falls back to a kernel stack when
 * the user walk fails, and user space symbolizes each stack with the resolver
 * for its kind: a kernel stack left in the user slot asks blazesym to name
 * `0xffffffff...` in a process that never executed it, and every frame comes
 * back as an address in no mapping. */
#define STACK_KIND_USER 0
#define STACK_KIND_KERNEL 1

struct offcpu_state {
    __u64 started_ns;
    __u32 stack_id;
    __u32 kind;
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
    state.kind = STACK_KIND_USER;
    if (state.stack_id == (__u32)INVALID_STACK_ID) {
        /* The user walk failed — a blocked task's registers often point
         * somewhere the unwinder cannot follow. Keep the kernel stack, which
         * still says what the task was waiting in, but record what it is. */
        state.stack_id = capture_kernel_stack(ctx);
        state.kind = STACK_KIND_KERNEL;
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
    __u32 kind = state->kind;
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
    /* The id goes in the slot its kind names. User space resolves each slot
     * with the matching resolver, so putting a kernel stack in the user slot
     * does not just mislabel it — it produces a wall of `[unmapped]` frames
     * from a symbolizer being asked about addresses that never ran. */
    if (kind == STACK_KIND_KERNEL) {
        key.user_stack = (__u32)INVALID_STACK_ID;
        key.kernel_stack = stack_id;
    } else {
        key.user_stack = stack_id;
        key.kernel_stack = (__u32)INVALID_STACK_ID;
    }
    add_value((void *)&offcpu_totals, &key, delta);
    return 0;
}

/* --------------------------------------------------------------------- */
/* Allocation profiling: allocator uprobes                                */
/* --------------------------------------------------------------------- */
/*
 * Every native process on this node — C, C++, Rust, and anything else that is
 * not a managed runtime — gets its heap from an allocator, and every allocator
 * on Linux is reached through the same handful of entry points: `malloc`,
 * `calloc`, `realloc`, `free`, `aligned_alloc`, `posix_memalign`,
 * `memalign`. Hooking those is enough to profile memory in all of them without
 * recompiling anything, which is what makes it a *language-independent* answer
 * where the Go reader above is a Go-specific one.
 *
 * The accounting is split in two because the two questions have different
 * shapes:
 *
 *   * **Allocated** ("how much has this call site allocated?") is a rate, so it
 *     is counted as a delta and drained every interval, exactly like a CPU
 *     sample counter.
 *   * **In use** ("how much is this call site still holding?") is a level. It
 *     cannot be derived from per-interval deltas without keeping state in user
 *     space that would go stale the moment a pid is recycled, so it is kept
 *     here as a balance: `+size` when an allocation is returned and `-size`
 *     when it is freed. User space reads it as a snapshot and never clears it.
 *     What a dead process left behind is deleted per tgid on the next scan.
 *
 * `alloc_live` is what makes the `-size` possible: `free` receives a pointer
 * and nothing else, so the size and the allocating stack have to be remembered
 * from the matching `malloc` return. The map is an LRU so that a workload with
 * millions of live blocks degrades into under-counted *frees* instead of
 * exhausting memory — a freed pointer that is no longer in the table simply
 * leaves its allocation on the books. That direction of error is deliberate:
 * over-counting in-use memory is visible, whereas silently dropping
 * allocations is not.
 *
 * ## What this does not see
 *
 * * Allocations made before the probe was attached. `inuse_*` therefore means
 *   "allocated and not yet freed since profiling started", not "the process's
 *   resident heap". A workload that allocates a gigabyte at startup and then
 *   sleeps reads as zero, which is exactly right about what was observed and
 *   exactly wrong about the process — hence the separate `profile_source`
 *   label, so these numbers are never added to a runtime's own numbers.
 * * `mmap`-backed arenas (jemalloc's extents, Go's heap). The allocator's own
 *   bookkeeping is at the `malloc` level, which is the level a developer reads.
 * * Anything that bypasses the public symbols and calls the allocator's
 *   internals (`_int_malloc` and friends). No application does.
 */

/* Registers of the probed function's caller, declared with the names the
 * kernel's BTF uses so that aya can relocate them. The struct has to be called
 * `pt_regs` and the fields `di`/`si`/`ax`: a CO-RE relocation matches by type
 * name *and* member name against `/sys/kernel/btf/vmlinux`, and a struct this
 * file invents a name for matches nothing — aya then poisons the instruction
 * with `call 0x0bad2310` and the verifier reports `invalid func unknown`. Two architectures cover
 * every kernel this profiler is built for; elsewhere the programs compile but
 * record nothing, and the collector refuses to attach them. */
#if defined(LIDLESSEYE_ARCH_X86_64)
struct pt_regs {
    unsigned long r15, r14, r13, r12, bp, bx, r11, r10, r9, r8, ax, cx, dx, si,
        di, orig_ax, ip, cs, flags, sp, ss;
} __attribute__((preserve_access_index));
#define ALLOC_ARG0(ctx) ((ctx)->di)
#define ALLOC_ARG1(ctx) ((ctx)->si)
#define ALLOC_ARG2(ctx) ((ctx)->dx)
#define ALLOC_RET(ctx) ((ctx)->ax)
#define LIDLESSEYE_ALLOC_SUPPORTED 1
#elif defined(LIDLESSEYE_ARCH_AARCH64)
struct user_pt_regs {
    __u64 regs[31];
    __u64 sp;
    __u64 pc;
    __u64 pstate;
} __attribute__((preserve_access_index));

struct pt_regs {
    struct user_pt_regs user_regs;
} __attribute__((preserve_access_index));
#define ALLOC_ARG0(ctx) ((ctx)->user_regs.regs[0])
#define ALLOC_ARG1(ctx) ((ctx)->user_regs.regs[1])
#define ALLOC_ARG2(ctx) ((ctx)->user_regs.regs[2])
#define ALLOC_RET(ctx) ((ctx)->user_regs.regs[0])
#define LIDLESSEYE_ALLOC_SUPPORTED 1
#else
struct pt_regs {
    __u64 unused;
} __attribute__((preserve_access_index));
#define ALLOC_ARG0(ctx) 0
#define ALLOC_ARG1(ctx) 0
#define ALLOC_ARG2(ctx) 0
#define ALLOC_RET(ctx) 0
#endif

/* One aggregation key: a thread group and an allocating stack. */
struct alloc_key {
    __u32 tgid;
    __u32 pad;
    /* Hash of the frames, see `alloc_stack`. Zero means "no stack". */
    __u64 stack;
};

/* Bytes and objects, used for both the drained delta and the in-use balance. */
struct alloc_counts {
    __u64 bytes;
    __u64 objects;
};

/* A live block: what `free` has to look up from a bare pointer. */
struct alloc_ptr_key {
    __u32 tgid;
    __u32 pad;
    __u64 ptr;
};

struct alloc_ptr_val {
    __u64 size;
    __u64 stack;
};

/* The allocation whose return value has not been seen yet, per thread.
 *
 * Keyed by tid rather than tgid: two threads of one process can be inside
 * `malloc` at the same time, and they are about to return two different
 * pointers. */
struct alloc_pending {
    __u64 size;
    /* `posix_memalign` hands the pointer back through an out parameter. */
    __u64 out;
    __u64 stack;
    __u32 kind;
    __u32 pad;
};

#define ALLOC_KIND_MALLOC 1
#define ALLOC_KIND_CALLOC 2
#define ALLOC_KIND_REALLOC 3
#define ALLOC_KIND_ALIGNED 4
#define ALLOC_KIND_POSIX_MEMALIGN 5

/* Frames behind one allocation stack hash. Sixteen is well past the depth of a
 * real allocation site; a deeper one is truncated, not lost. */
#define ALLOC_STACK_FRAMES 16

/* How many frames the hash covers, zero-padded.
 *
 * Not the whole captured stack, because the last frames of a capture are not
 * the caller's: glibc's `malloc` is built without a frame pointer, so the walk
 * that starts inside it sometimes steps off the real chain and reads whatever
 * is left on the stack — `32`, `13`, a stale local. Those bytes differ from
 * call to call, and hashing them turned one allocation site into thousands of
 * distinct stacks, which filled the table and left most of them unresolvable.
 * The first four frames are the site: the allocator, its caller, and two
 * levels of context, which is more than a flame graph needs to attribute a
 * byte. Anything past them still travels with the entry, for the frames shown
 * once the stack is symbolized. */
#define ALLOC_STACK_KEY_FRAMES 4

struct alloc_frames {
    __u32 nr;
    __u32 pad;
    __u64 ips[ALLOC_STACK_FRAMES];
};

/* Stack hash -> the frames themselves.
 *
 * Deliberately *not* a `BPF_MAP_TYPE_STACK_TRACE`. That type hands out 32-bit
 * ids, but they hold only `log2(max_entries)` bits of entropy: the kernel
 * takes the stack's hash masked to the table size, and a second stack landing
 * on a taken slot gets `-EEXIST` instead of an id unless `BPF_F_REUSE_STACKID`
 * is set — which would then silently point the id at somebody else's stack. A
 * node with a few hundred allocation sites collides in a 65536-slot table as a
 * matter of course, and the losing site stays invisible for the lifetime of
 * the agent: its `malloc` calls all fail, so a whole process's heap disappears
 * from the report. Hashing here gives 64 bits of key space, where a collision
 * is not something to design for.
 *
 * The map also keeps the frames for symbolization, since the id is no longer
 * a handle the kernel can resolve. */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u64);
    __type(value, struct alloc_frames);
} alloc_stacks SEC(".maps");

/* pointer -> (size, allocating stack), for `free` and `realloc`. */
struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 262144);
    __type(key, struct alloc_ptr_key);
    __type(value, struct alloc_ptr_val);
} alloc_live SEC(".maps");

/* Drained every interval: bytes and objects allocated in the window. */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, struct alloc_key);
    __type(value, struct alloc_counts);
} alloc_totals SEC(".maps");

/* Read as a snapshot, never cleared: bytes and objects still held. */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, struct alloc_key);
    __type(value, struct alloc_counts);
} alloc_inuse SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u32);
    __type(value, struct alloc_pending);
} alloc_pending SEC(".maps");

/* Why the capture failed, by errno.
 *
 * From userland every failure looks the same — the row arrives with stack 0
 * and its bytes are lost — but the causes need opposite fixes: `-ENOMEM` means
 * `alloc_stacks` is out of entries and wants a bigger table, `-EFAULT` means
 * the user stack could not be walked and wants a different capture. Counting
 * them here is what tells the two apart. */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 16);
    __type(key, __u32);
    __type(value, __u64);
} alloc_errors SEC(".maps");

/* The agent's own thread group, written once at startup.
 *
 * Not vanity: the agent allocates thousands of times a second and none of it
 * belongs in a report, but the capture still runs, and the agent — like most
 * optimized binaries — has no frame pointer, so the walk off the allocator
 * lands in whatever is left on the stack and every call hashes differently.
 * That fills the frames table within seconds, after which real sites cannot
 * register at all. Cheaper to not look. */
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u32);
} agent_tgid SEC(".maps");

/* The tgid to account this call to, or zero for "not ours".
 *
 * Also the single place that decides the agent is out of scope, so the
 * programs below read the same way. */
static __always_inline __u32 alloc_tgid(void) {
    __u32 tgid = current_tgid();
    if (tgid == 0) {
        return 0;
    }
    __u32 zero = 0;
    __u32 *self = bpf_map_lookup_elem((void *)&agent_tgid, &zero);
    if (self && *self == tgid) {
        return 0;
    }
    return tgid;
}

static __always_inline void alloc_note_error(__u32 failure) {
    __u64 *hits = bpf_map_lookup_elem((void *)&alloc_errors, &failure);
    if (hits) {
        __sync_fetch_and_add(hits, 1);
        return;
    }
    __u64 one = 1;
    bpf_map_update_elem((void *)&alloc_errors, &failure, &one, BPF_NOEXIST);
}

/* Capture the allocation stack and return a 64-bit hash of it.
 *
 * `bpf_get_stack` copies the frames into a buffer instead of interning them in
 * a `STACK_TRACE` map, which is what lets the caller keep a key with enough
 * bits to not collide. Frames are FNV-1a'd in the same pass and the buffer is
 * stored only when the hash is new, so a repeated allocation site costs one
 * map lookup.
 *
 * Zero is not a valid hash here: it is what "capture failed" is spelled as. */
static __always_inline __u64 alloc_stack(struct pt_regs *ctx) {
    __u64 ips[ALLOC_STACK_FRAMES];
    long read = bpf_get_stack(ctx, ips, (__u32)sizeof(ips), BPF_F_USER_STACK);
    if (read <= 0) {
        alloc_note_error(read < 0 ? (__u32)(-read) : 0);
        return 0;
    }
    __u32 nr = (__u32)read / 8;
    __u64 hash = 0xcbf29ce484222325ULL;
#pragma unroll
    for (int i = 0; i < ALLOC_STACK_KEY_FRAMES; i++) {
        __u64 ip = 0;
        if (i < nr) {
            ip = ips[i];
        }
        hash ^= ip;
        hash *= 0x100000001b3ULL;
    }
    if (hash == 0) {
        hash = 1;
    }

    if (!bpf_map_lookup_elem((void *)&alloc_stacks, &hash)) {
        struct alloc_frames frames = {};
        frames.nr = nr;
#pragma unroll
        for (int i = 0; i < ALLOC_STACK_FRAMES; i++) {
            if (i < nr) {
                frames.ips[i] = ips[i];
            }
        }
        bpf_map_update_elem((void *)&alloc_stacks, &hash, &frames, BPF_NOEXIST);
    }
    return hash;
}

/* `bytes`/`objects` added to, or subtracted from, the balance of one stack.
 *
 * The insertion is `BPF_NOEXIST` with a lookup-and-retry on failure rather than
 * a blind update: two threads allocating on the same stack at the same instant
 * would otherwise both see "no entry" and the slower one would overwrite the
 * faster one's counters, losing a window's worth of bytes. */
static __always_inline void alloc_add(void *map, struct alloc_key *key, __u64 bytes, __u64 objects,
                                      int negative) {
    __u64 delta_bytes = negative ? (__u64)(0 - bytes) : bytes;
    __u64 delta_objects = negative ? (__u64)(0 - objects) : objects;

    struct alloc_counts *slot = bpf_map_lookup_elem(map, key);
    if (slot) {
        __sync_fetch_and_add(&slot->bytes, delta_bytes);
        __sync_fetch_and_add(&slot->objects, delta_objects);
        return;
    }
    struct alloc_counts fresh = {};
    fresh.bytes = delta_bytes;
    fresh.objects = delta_objects;
    if (bpf_map_update_elem(map, key, &fresh, BPF_NOEXIST) == 0) {
        return;
    }
    slot = bpf_map_lookup_elem(map, key);
    if (slot) {
        __sync_fetch_and_add(&slot->bytes, delta_bytes);
        __sync_fetch_and_add(&slot->objects, delta_objects);
    }
}

/* Account a pointer that is going away. A pointer this process never allocated
 * under our watch — or one whose record was evicted — has nothing to release. */
static __always_inline void alloc_release(__u32 tgid, __u64 ptr) {
    if (ptr == 0) {
        return;
    }
    struct alloc_ptr_key key = {};
    key.tgid = tgid;
    key.ptr = ptr;

    struct alloc_ptr_val *live = bpf_map_lookup_elem((void *)&alloc_live, &key);
    if (!live) {
        return;
    }
    __u64 size = live->size;
    __u64 stack = live->stack;
    bpf_map_delete_elem((void *)&alloc_live, &key);

    struct alloc_key agg = {};
    agg.tgid = tgid;
    agg.stack = stack;
    alloc_add((void *)&alloc_inuse, &agg, size, 1, 1);
}

/* Record the arguments of an allocation whose return value is still to come. */
static __always_inline void alloc_begin(struct pt_regs *ctx, __u64 size, __u32 kind, __u64 out) {
    __u32 tid = (__u32)bpf_get_current_pid_tgid();
    struct alloc_pending pending = {};
    pending.size = size;
    pending.out = out;
    pending.kind = kind;
    pending.stack = alloc_stack(ctx);
    bpf_map_update_elem((void *)&alloc_pending, &tid, &pending, BPF_ANY);
}

SEC("uprobe/malloc")
int lidlesseye_malloc(struct pt_regs *ctx) {
    if (alloc_tgid() == 0) {
        return 0;
    }
    alloc_begin(ctx, ALLOC_ARG0(ctx), ALLOC_KIND_MALLOC, 0);
    return 0;
}

SEC("uprobe/calloc")
int lidlesseye_calloc(struct pt_regs *ctx) {
    if (alloc_tgid() == 0) {
        return 0;
    }
    __u64 count = ALLOC_ARG0(ctx);
    __u64 size = ALLOC_ARG1(ctx);
    /* The product is what the allocator will serve, and an overflowing request
     * is rejected by it (NULL) rather than honoured. Refuse the same ones here
     * so that a garbage register cannot add 2^63 bytes to a balance. */
    if (count != 0 && size > (1ULL << 44) / count) {
        return 0;
    }
    alloc_begin(ctx, count * size, ALLOC_KIND_CALLOC, 0);
    return 0;
}

SEC("uprobe/realloc")
int lidlesseye_realloc(struct pt_regs *ctx) {
    __u32 tgid = alloc_tgid();
    if (tgid == 0) {
        return 0;
    }
    /* `realloc` frees its argument before the new pointer exists, and both
     * halves have to land in the same window or a balance drifts. */
    alloc_release(tgid, ALLOC_ARG0(ctx));
    alloc_begin(ctx, ALLOC_ARG1(ctx), ALLOC_KIND_REALLOC, 0);
    return 0;
}

SEC("uprobe/free")
int lidlesseye_free(struct pt_regs *ctx) {
    __u32 tgid = alloc_tgid();
    if (tgid == 0) {
        return 0;
    }
    alloc_release(tgid, ALLOC_ARG0(ctx));
    return 0;
}

/* `aligned_alloc(alignment, size)` and the older `memalign(alignment, size)`. */
SEC("uprobe/aligned_alloc")
int lidlesseye_aligned_alloc(struct pt_regs *ctx) {
    if (alloc_tgid() == 0) {
        return 0;
    }
    alloc_begin(ctx, ALLOC_ARG1(ctx), ALLOC_KIND_ALIGNED, 0);
    return 0;
}

/* `posix_memalign(void **memptr, alignment, size)` returns a status and writes
 * the pointer through its first argument, so the pointer is only known at
 * return time and has to be carried across. */
SEC("uprobe/posix_memalign")
int lidlesseye_posix_memalign(struct pt_regs *ctx) {
    if (alloc_tgid() == 0) {
        return 0;
    }
    alloc_begin(ctx, ALLOC_ARG2(ctx), ALLOC_KIND_POSIX_MEMALIGN, ALLOC_ARG0(ctx));
    return 0;
}

/* Shared return probe for every allocating entry point above. */
SEC("uretprobe/alloc")
int lidlesseye_alloc_return(struct pt_regs *ctx) {
    __u32 tid = (__u32)bpf_get_current_pid_tgid();
    struct alloc_pending *pending = bpf_map_lookup_elem((void *)&alloc_pending, &tid);
    if (!pending) {
        /* A return without a recorded entry: the probe was attached while the
         * call was already in flight. Not an allocation we can account. */
        return 0;
    }
    __u64 size = pending->size;
    __u64 out = pending->out;
    __u64 stack = pending->stack;
    __u32 kind = pending->kind;
    bpf_map_delete_elem((void *)&alloc_pending, &tid);

    __u64 ptr = ALLOC_RET(ctx);
    if (kind == ALLOC_KIND_POSIX_MEMALIGN) {
        /* Non-zero is an error status; on success the pointer is in `*memptr`. */
        if (ptr != 0) {
            return 0;
        }
        __u64 written = 0;
        if (bpf_probe_read_user(&written, sizeof(written), (void *)out) != 0) {
            return 0;
        }
        ptr = written;
    }
    if (ptr == 0 || size == 0) {
        return 0;
    }

    __u32 tgid = alloc_tgid();
    if (tgid == 0) {
        return 0;
    }

    struct alloc_ptr_key key = {};
    key.tgid = tgid;
    key.ptr = ptr;
    struct alloc_ptr_val val = {};
    val.size = size;
    val.stack = stack;
    bpf_map_update_elem((void *)&alloc_live, &key, &val, BPF_ANY);

    struct alloc_key agg = {};
    agg.tgid = tgid;
    agg.stack = stack;
    alloc_add((void *)&alloc_totals, &agg, size, 1, 0);
    alloc_add((void *)&alloc_inuse, &agg, size, 1, 0);
    return 0;
}

/* The kernel requires an explicit license for GPL-only helpers, and
 * `bpf_get_stackid` is one of them. The eBPF program is therefore licensed
 * GPL-2.0-only (see LICENSE-BPF); every user space crate in this workspace
 * remains Apache-2.0. This split is the same one used by bpftrace,
 * parca-agent and the OpenTelemetry eBPF profiler. */
char LICENSE[] SEC("license") = "GPL";
