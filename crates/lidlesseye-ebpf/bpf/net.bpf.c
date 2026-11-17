// LidlessEye eBPF network programs: connection-level accounting.
//
// This is a second object next to `profile.bpf.c`, loaded by its own collector.
// The split is deliberate: a kernel that rejects one of these programs should
// cost the service map its byte counters, not the flamegraph its samples.
//
// ## Why a probe and not `/proc`
//
// `/proc/<pid>/net/tcp` is polled, so it sees only sockets that happen to be
// open at the instant of the scan, it cannot tell how many bytes a connection
// carried, and — because kube-proxy rewrites the source address of a same-node
// pod-to-ClusterIP connection (hairpin SNAT) — it cannot name the client of
// such a connection at all. A probe on `tcp_set_state` fires exactly once per
// state transition and carries the socket, whose addresses are the ones the
// application wrote, before any NAT.
//
// ## Why kprobes and not the tracepoint or fentry
//
// `sock:inet_sock_set_state` looks like the natural probe, but its `TP_PROTO`
// is not stable: 5.x passes `(sk, skaddr, old, new)` and 6.18 passes
// `(sk, old, new)`, and a raw tracepoint program that reads the wrong slot is
// rejected at attach time with `EINVAL` rather than at compile time.
// `fentry/tcp_sendmsg` is equally version-coupled: it needs the kernel's BTF
// function prototype, and one whose parameter type cannot be resolved fails to
// load. A kprobe takes the same arguments on every kernel — they are just the
// calling convention's registers — so the only thing that varies is `pt_regs`,
// which is a CO-RE struct like any other, and `tcp_set_state`'s old state,
// which is still in `sk_state` because a kprobe runs before the body does.
//
// ## What is counted
//
//   * `lidlesseye_tcp_state`  - connection open (client and server side), the
//                               round-trip time at close, and the sock -> flow
//                               mapping the other programs need.
//   * `lidlesseye_tcp_send`   - bytes handed to TCP, per window.
//   * `lidlesseye_tcp_recv`   - bytes handed to the application, per window.
//   * `lidlesseye_tcp_retrans`- retransmitted segments.
//
// ## CO-RE struct names matter
//
// Every struct below is named exactly as the kernel names it (`sock`,
// `sock_common`, `tcp_sock`). aya resolves a field by walking the local type
// and then looking the member name up in the running kernel's BTF, which only
// works when the type names match; a mismatched name makes the relocation
// silently fall back to the local offset. The members declared here are the
// only ones read, and their order is irrelevant to that lookup.

typedef unsigned char __u8;
typedef unsigned short __u16;
typedef unsigned int __u32;
typedef int __s32;
typedef unsigned long long __u64;
typedef long long __s64;

#define SEC(name) __attribute__((section(name), used))
#define __always_inline inline __attribute__((always_inline))

#define BPF_MAP_TYPE_HASH 1
#define BPF_MAP_TYPE_LRU_HASH 9
#define BPF_ANY 0

#define AF_INET 2
#define AF_INET6 10

#define TCP_SYN_SENT 2
#define TCP_SYN_RECV 3
#define TCP_TIME_WAIT 6
#define TCP_CLOSE 7
#define TCP_ESTABLISHED 1

#define ROLE_UNKNOWN 0
#define ROLE_CLIENT 1
#define ROLE_SERVER 2

/* ---- helper ABI ------------------------------------------------------- */
static void *(*bpf_map_lookup_elem)(void *map, const void *key) = (void *)1;
static long (*bpf_map_update_elem)(void *map, const void *key, const void *value,
                                   __u64 flags) = (void *)2;
static long (*bpf_map_delete_elem)(void *map, const void *key) = (void *)3;
static long (*bpf_probe_read_kernel)(void *dst, __u32 size, const void *src) = (void *)113;
static __u64 (*bpf_get_current_cgroup_id)(void) = (void *)80;

/* ---- kernel structs, by the kernel's own names ------------------------ */
struct in6_addr {
    __u8 bytes[16];
} __attribute__((preserve_access_index));

struct sock_common {
    union {
        struct {
            __u32 skc_daddr;     /* the peer */
            __u32 skc_rcv_saddr; /* us */
        };
    };
    struct in6_addr skc_v6_daddr;
    struct in6_addr skc_v6_rcv_saddr;
    union {
        struct {
            __u16 skc_dport; /* network order */
            __u16 skc_num;   /* host order */
        };
    };
    __u16 skc_family;
    unsigned char skc_state;
} __attribute__((preserve_access_index));

struct sock {
    struct sock_common __sk_common;
} __attribute__((preserve_access_index));

struct tcp_sock {
    __u32 srtt_us; /* smoothed RTT, fixed point, microseconds << 3 */
} __attribute__((preserve_access_index));

/* Registers of the probed function's caller. Declared with the names the
 * kernel's own BTF uses (`di`, not `rdi`; `bp`, not `rbp`) because aya
 * relocates members by name. */
#if defined(LIDLESSEYE_ARCH_X86_64)
struct pt_regs {
    unsigned long r15, r14, r13, r12, bp, bx, r11, r10, r9, r8, ax, cx, dx, si,
        di, orig_ax, ip, cs, flags, sp, ss;
} __attribute__((preserve_access_index));
#define PARM1(ctx) ((ctx)->di)
#define PARM2(ctx) ((ctx)->si)
#define PARM3(ctx) ((ctx)->dx)
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
#define PARM1(ctx) ((ctx)->user_regs.regs[0])
#define PARM2(ctx) ((ctx)->user_regs.regs[1])
#define PARM3(ctx) ((ctx)->user_regs.regs[2])
#else
/* An architecture whose calling convention this file does not know: the
 * programs below compile but report no arguments, and the collector refuses to
 * start because nothing useful would be counted. */
struct pt_regs {
    __u64 unused;
} __attribute__((preserve_access_index));
#define PARM1(ctx) 0
#define PARM2(ctx) 0
#define PARM3(ctx) 0
#endif

/* ---- maps ------------------------------------------------------------- */
/* One address pair, in the observer's own view: `saddr` is always the local
 * end. Two ends of one connection are two keys, which is what lets the user
 * space side decide which of them to count. */
struct flow_key {
    __u8 saddr[16];
    __u8 daddr[16];
    __u16 sport;
    __u16 dport;
    __u16 family;
    __u16 pad;
};

struct flow_stats {
    __u64 opens;
    __u64 tx_bytes;
    __u64 rx_bytes;
    __u64 retransmits;
    __u64 rtt_us_sum;
    __u64 rtt_samples;
    /* Which container opened the connection. A 5-tuple only says "this pod",
     * and a pod is not one process: a sidecar has its own sockets. Filled
     * from `sock_owners`, which records it where the task is known. */
    __u64 cgroup;
    __u32 role;
    __u32 pad;
};

#define __uint(name, val) int (*name)[val]
#define __type(name, val) typeof(val) *name

/* Per window counters, drained and cleared by user space once per interval. */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, struct flow_key);
    __type(value, struct flow_stats);
} flows SEC(".maps");

/* sock pointer -> flow, so the data programs do not have to re-read the
 * addresses on every packet. LRU: a socket that dies without a CLOSE
 * transition must not pin an entry forever. */
struct sock_flow {
    struct flow_key key;
    __u32 state;
    __u32 pad;
};

struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 131072);
    __type(key, __u64);
    __type(value, struct sock_flow);
} sock_flows SEC(".maps");

/* sock pointer -> cgroup of the task that dialled it.
 *
 * The handshake completes in softirq, where `bpf_get_current_cgroup_id()` is
 * whatever task the CPU happened to interrupt — not the one that called
 * connect(). The dialler's identity is therefore taken where the socket enters
 * SYN_SENT, which connect() does in the caller's context, and carried until
 * the state machine reports the socket established. LRU as well, for the
 * connections that never complete. */
struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 131072);
    __type(key, __u64);
    __type(value, __u64);
} sock_owners SEC(".maps");

/* ---- helpers ---------------------------------------------------------- */
#ifdef LIDLESSEYE_LE
#define NTOHS(x) ((__u16)__builtin_bswap16((__u16)(x)))
#else
#define NTOHS(x) ((__u16)(x))
#endif

/* Fill `key` from `sk`. Returns 0 when the socket is not IP (a netlink socket,
 * an AF_UNIX peer) or the addresses could not be read. */
static __always_inline int flow_key_of(struct sock *sk, struct flow_key *key) {
    __u16 family = 0;
    if (bpf_probe_read_kernel(&family, sizeof(family), &sk->__sk_common.skc_family) != 0) {
        return -1;
    }
    if (family != AF_INET && family != AF_INET6) {
        return -1;
    }
    key->family = family;

    __u16 sport = 0;
    __u16 dport = 0;
    if (family == AF_INET) {
        __u32 saddr = 0;
        __u32 daddr = 0;
        if (bpf_probe_read_kernel(&saddr, sizeof(saddr), &sk->__sk_common.skc_rcv_saddr) != 0 ||
            bpf_probe_read_kernel(&daddr, sizeof(daddr), &sk->__sk_common.skc_daddr) != 0) {
            return -1;
        }
        __builtin_memcpy(key->saddr, &saddr, 4);
        __builtin_memcpy(key->daddr, &daddr, 4);
    } else {
        if (bpf_probe_read_kernel(key->saddr, 16, &sk->__sk_common.skc_v6_rcv_saddr) != 0 ||
            bpf_probe_read_kernel(key->daddr, 16, &sk->__sk_common.skc_v6_daddr) != 0) {
            return -1;
        }
    }

    if (bpf_probe_read_kernel(&sport, sizeof(sport), &sk->__sk_common.skc_num) != 0 ||
        bpf_probe_read_kernel(&dport, sizeof(dport), &sk->__sk_common.skc_dport) != 0) {
        return -1;
    }
    key->sport = sport;
    key->dport = NTOHS(dport);
    return 0;
}

static __always_inline struct flow_stats *stats_for(struct flow_key *key) {
    struct flow_stats *stats = bpf_map_lookup_elem((void *)&flows, key);
    if (stats) {
        return stats;
    }
    struct flow_stats fresh = {};
    bpf_map_update_elem((void *)&flows, key, &fresh, BPF_ANY);
    return bpf_map_lookup_elem((void *)&flows, key);
}

/* --------------------------------------------------------------------- */
/* State transitions: the connection lifecycle                            */
/* --------------------------------------------------------------------- */
SEC("kprobe/tcp_set_state")
int lidlesseye_tcp_state(struct pt_regs *ctx) {
    __u64 addr = PARM1(ctx);
    __u32 new_state = (__u32)PARM2(ctx);
    if (addr == 0) {
        return 0;
    }
    struct sock *sk = (struct sock *)addr;

    /* A kprobe runs before the body, so `sk_state` still holds the state the
     * socket is leaving. Reading it is what keeps this program free of the
     * tracepoint's version-dependent argument list. */
    __u8 old_state = 0;
    if (bpf_probe_read_kernel(&old_state, sizeof(old_state), &sk->__sk_common.skc_state) != 0) {
        return 0;
    }

    struct sock_flow entry = {};
    if (flow_key_of(sk, &entry.key) != 0) {
        return 0;
    }
    entry.state = new_state;

    /* Entering SYN_SENT is the one transition a client's own task performs:
     * connect() calls tcp_connect(), which sets the state before sending the
     * SYN. Recording the cgroup here is exact, and the completion of the
     * handshake — which happens later, in softirq — can still read it. */
    if (new_state == TCP_SYN_SENT) {
        __u64 owner = bpf_get_current_cgroup_id();
        bpf_map_update_elem((void *)&sock_owners, &addr, &owner, BPF_ANY);
    }

    /* The client's handshake completes on SYN_SENT -> ESTABLISHED and the
     * server's on SYN_RECV -> ESTABLISHED. Counting the completed handshake
     * rather than the SYN keeps half-open sockets out of the map, and it is
     * the only transition both ends are guaranteed to go through
     * `tcp_set_state` for. */
    if (new_state == TCP_ESTABLISHED &&
        (old_state == TCP_SYN_SENT || old_state == TCP_SYN_RECV)) {
        struct flow_stats *stats = stats_for(&entry.key);
        if (stats) {
            __sync_fetch_and_add(&stats->opens, 1);
            stats->role = old_state == TCP_SYN_SENT ? ROLE_CLIENT : ROLE_SERVER;
            /* The client side has an owner worth recording: the task that
             * called connect(). The server's socket was accepted in softirq,
             * so its container comes from the port the pod declares instead. */
            if (old_state == TCP_SYN_SENT) {
                __u64 *owner = bpf_map_lookup_elem((void *)&sock_owners, &addr);
                if (owner) {
                    stats->cgroup = *owner;
                }
            }
        }
    }

    /* One RTT sample per connection, taken when it is about to disappear:
     * reading it per packet would drown the kernel-side cost, and reading it
     * at open would report the handshake estimate for the whole lifetime. */
    if (new_state == TCP_TIME_WAIT || new_state == TCP_CLOSE) {
        __u32 srtt = 0;
        if (bpf_probe_read_kernel(&srtt, sizeof(srtt), &((struct tcp_sock *)sk)->srtt_us) == 0 &&
            srtt > 0) {
            struct flow_stats *stats = stats_for(&entry.key);
            if (stats) {
                __sync_fetch_and_add(&stats->rtt_us_sum, (__u64)(srtt >> 3));
                __sync_fetch_and_add(&stats->rtt_samples, 1);
            }
        }
    }

    if (new_state == TCP_CLOSE) {
        bpf_map_delete_elem((void *)&sock_flows, &addr);
        bpf_map_delete_elem((void *)&sock_owners, &addr);
    } else {
        bpf_map_update_elem((void *)&sock_flows, &addr, &entry, BPF_ANY);
    }
    return 0;
}

/* --------------------------------------------------------------------- */
/* Bytes sent by the application                                          */
/* --------------------------------------------------------------------- */
SEC("kprobe/tcp_sendmsg")
int lidlesseye_tcp_send(struct pt_regs *ctx) {
    __u64 addr = PARM1(ctx);
    __u64 size = PARM3(ctx);
    if (size == 0 || addr == 0) {
        return 0;
    }
    struct sock_flow *entry = bpf_map_lookup_elem((void *)&sock_flows, &addr);
    if (!entry) {
        return 0;
    }
    struct flow_stats *stats = stats_for(&entry->key);
    if (stats) {
        __sync_fetch_and_add(&stats->tx_bytes, size);
    }
    return 0;
}

/* --------------------------------------------------------------------- */
/* Bytes consumed by the application                                      */
/* --------------------------------------------------------------------- */
SEC("kprobe/tcp_cleanup_rbuf")
int lidlesseye_tcp_recv(struct pt_regs *ctx) {
    __u64 addr = PARM1(ctx);
    /* `copied` is an `int` in the kernel's signature, so only the low 32 bits
     * of the register carry it and they carry it signed: a negative value read
     * as 64-bit becomes +4GiB of "received" bytes. */
    __s32 copied = (__s32)(__u32)PARM2(ctx);
    if (copied <= 0 || addr == 0) {
        return 0;
    }
    struct sock_flow *entry = bpf_map_lookup_elem((void *)&sock_flows, &addr);
    if (!entry) {
        return 0;
    }
    struct flow_stats *stats = stats_for(&entry->key);
    if (stats) {
        __sync_fetch_and_add(&stats->rx_bytes, (__u64)copied);
    }
    return 0;
}

/* --------------------------------------------------------------------- */
/* Retransmissions                                                        */
/* --------------------------------------------------------------------- */
SEC("kprobe/tcp_retransmit_skb")
int lidlesseye_tcp_retrans(struct pt_regs *ctx) {
    __u64 addr = PARM1(ctx);
    /* `segs` is an `int`, with the same 32-bit caveat as `copied` above. */
    __s32 segs = (__s32)(__u32)PARM3(ctx);
    if (addr == 0) {
        return 0;
    }
    struct sock_flow *entry = bpf_map_lookup_elem((void *)&sock_flows, &addr);
    if (!entry) {
        return 0;
    }
    struct flow_stats *stats = stats_for(&entry->key);
    if (stats) {
        __sync_fetch_and_add(&stats->retransmits, segs > 0 ? (__u64)segs : 1);
    }
    return 0;
}

char LICENSE[] SEC("license") = "GPL";
