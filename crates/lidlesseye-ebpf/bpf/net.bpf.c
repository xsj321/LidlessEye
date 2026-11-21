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
//   * `lidlesseye_tcp_send`   - bytes handed to TCP, per window, plus the
//                               payload samples described below.
//   * `lidlesseye_tcp_recv`   - bytes handed to the application, per window.
//   * `lidlesseye_tcp_retrans`- retransmitted segments.
//   * `lidlesseye_ssl_write`  - plaintext samples from a TLS library, optional.
//
// ## Where L7 lives
//
// The kernel does not parse HTTP or SQL. It recognises the protocol of a flow
// and, within a budget, copies the first bytes of a request into a perf ring
// buffer; `lidlesseye_ebpf::l7` finds the `Host:` header, the MySQL statement
// and the PostgreSQL query in user space. Extraction in C was tried and the
// verifier rejected the send probe with EINVAL: bounded copy loops plus a
// dozen classifiers in one program exceed what it accepts, and every protocol
// added later would add to that budget. Samples are capped so a busy
// connection cannot flood the ring buffer, and bulk writes are dropped in the
// kernel before they are copied at all.
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
#define BPF_MAP_TYPE_PERF_EVENT_ARRAY 4
#define BPF_MAP_TYPE_LRU_HASH 9
#define BPF_ANY 0
#define BPF_F_CURRENT_CPU 0xffffffffULL

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
/* `const` is not decoration: without it clang keeps the pointer in .data and
 * emits an indirect call whose target the verifier cannot resolve, which it
 * rejects as `invalid func unknown#<address>`. libbpf's own bpf_helpers.h
 * declares them the same way. */
static void *(*const bpf_map_lookup_elem)(void *map, const void *key) = (void *)1;
static long (*const bpf_map_update_elem)(void *map, const void *key, const void *value,
                                         __u64 flags) = (void *)2;
static long (*const bpf_map_delete_elem)(void *map, const void *key) = (void *)3;
static long (*const bpf_probe_read_kernel)(void *dst, __u32 size, const void *src) = (void *)113;
/* `BPF_FUNC_probe_read_user` is 112 in `enum bpf_func_id`, which the running
 * kernel carries in its own BTF (`/sys/kernel/btf/vmlinux`). The neighbouring
 * ids are the *string* variants — 114 is `probe_read_user_str`, 115 is
 * `probe_read_kernel_str` — and both return a length instead of zero, so a
 * wrong id here fails silently: every read looks like an error and no payload
 * is ever captured. */
static long (*const bpf_probe_read_user)(void *dst, __u32 size, const void *src) = (void *)112;
static __u64 (*const bpf_get_current_cgroup_id)(void) = (void *)80;
static __u64 (*const bpf_ktime_get_ns)(void) = (void *)5;
static long (*const bpf_perf_event_output)(void *ctx, void *map, __u64 flags, void *data,
                                           __u64 size) = (void *)25;

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

/* Enough of `struct iovec` to copy one entry. Plain layout on purpose: the
 * bytes are read into this struct by the probe, so it is our own copy, not a
 * view into the kernel's type. */
struct iovec {
    void *iov_base;
    __u64 iov_len;
};

/* Where the first iovec pointer lives, resolved by the agent from the running
 * kernel's BTF.
 *
 * This used to be `msg->msg_iter.iov` with CO-RE doing the walk. Kernel 6.16
 * reworked `struct iov_iter`: the iovec pointer moved and `iov` was renamed
 * `__iov`. A CO-RE access by name then fails to resolve, and aya replaces the
 * instruction with `call 0xBAD2310` — which the verifier rejects outright, so
 * the whole send probe failed to load with EINVAL and no payload was ever
 * captured. Two patched constants work on both layouts, and the agent reads
 * them from the same BTF that CO-RE uses anyway. A wrong offset is harmless:
 * the pointer read fails and the sample is dropped. */
const volatile __u32 msg_iter_offset = 16;
const volatile __u32 iov_ptr_offset = 16;

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
    /* Payload samples already sent to user space for this flow. The map is
     * drained every interval, so this is a per-window budget: a chatty
     * connection must not fill the ring buffer, but the first request of a
     * fresh one always gets through. */
    __u32 l7_samples;
};

/* Payload samples for user-space L7 parsing.
 *
 * The kernel used to extract the Host header and the SQL text itself. A dozen
 * protocol classifiers plus bounded copy loops in one verification unit is
 * what the verifier kept rejecting on the send path, so the split moved: the
 * probe copies request bytes into a ring buffer and the agent parses them.
 * The probe now does a fixed amount of work per sample, and every protocol
 * added in Rust costs nothing here.
 *
 * `flags & L7_HAS_FLOW` says whether `key` names a connection; the TLS uprobe
 * has no socket to key on, so it sends only the cgroup. */
#define L7_SAMPLE 256
#define L7_MASK (L7_SAMPLE - 1)
#define L7_HAS_FLOW 1U
#define L7_PER_WINDOW 12

#define __uint(name, val) int (*name)[val]
#define __type(name, val) typeof(val) *name

struct l7_event {
    struct flow_key key;
    __u64 cgroup;
    __u32 len;
    __u32 flags;
    __u8 payload[L7_SAMPLE];
};

struct {
    __uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
    __uint(max_entries, 256);
    __type(key, __u32);
    __type(value, __u32);
} l7_events SEC(".maps");

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
/* Payload access                                                         */
/* --------------------------------------------------------------------- */
/* A kprobe on tcp_sendmsg sees the application's buffer before the kernel
 * splits it into segments, so the first iovec holds the request line, the
 * MySQL command or the PostgreSQL query.
 *
 * Recognising the protocol used to happen here, with a parser per wire format.
 * It no longer does: fifteen classifiers in one program is what pushed the
 * verifier past its one-million-instruction ceiling, and the payload already
 * travels to user space for the Host and SQL parsers. The agent names the
 * protocol from the same bytes, so a new protocol costs a Rust function
 * instead of a slice of the verifier's budget.
 */

#define PAYLOAD_SAMPLE 128
#define PAYLOAD_MASK (PAYLOAD_SAMPLE - 1)

static __always_inline char at(const char *buf, __u64 offset) {
    /* `volatile` keeps the compiler from folding `base | (offset & 127)`
     * into one instruction. That fold is valid for a 128-byte aligned stack
     * buffer, but the verifier rejects any bitwise op on a pointer. */
    volatile __u32 index = (__u32)offset & PAYLOAD_MASK;
    return buf[index];
}

/* --------------------------------------------------------------------- */
/* L7 sampling: hand payloads to user space                               */
/* --------------------------------------------------------------------- */
/* The kernel only decides whether a payload is worth shipping — a request
 * line, a database command, a query. Finding `Host:`, decoding a MySQL
 * COM_QUERY and walking PostgreSQL's Parse body all happen in the agent,
 * where a parser costs neither instruction budget nor a map entry. The check
 * below exists so that bulk data on a long-lived connection is never copied
 * into the ring buffer at all. */
static __always_inline int looks_like_request(const char *b, __u32 size) {
    if (size < 8) {
        return 0;
    }
    char c = at(b, 0);
    /* HTTP request line. Only the methods that carry a body or a resource. */
    if ((c == 'G' && at(b, 1) == 'E' && at(b, 2) == 'T') ||
        (c == 'P' && ((at(b, 1) == 'O' && at(b, 2) == 'S') ||
                      (at(b, 1) == 'U' && at(b, 2) == 'T') ||
                      (at(b, 1) == 'A' && at(b, 2) == 'T'))) ||
        (c == 'H' && at(b, 1) == 'E' && at(b, 2) == 'A' && at(b, 3) == 'D') ||
        (c == 'D' && at(b, 1) == 'E' && at(b, 2) == 'L' && at(b, 3) == 'E') ||
        (c == 'O' && at(b, 1) == 'P' && at(b, 2) == 'T')) {
        return 1;
    }
    /* PostgreSQL: a simple query, or a Parse that carries the statement. */
    if (c == 'Q' || c == 'P') {
        return 1;
    }
    /* MySQL: length(3) + sequence 0 + COM_QUERY / COM_STMT_PREPARE. Prepared
     * statements are what most drivers use, so 0x16 is not optional. */
    if ((__u8)at(b, 3) == 0) {
        __u8 cmd = (__u8)at(b, 4);
        if (cmd == 0x03 || cmd == 0x16) {
            return 1;
        }
    }
    return 0;
}

/* `struct iov_iter.iter_type`. `write()` and `send()` on a connected socket
 * hand the kernel one buffer (`ITER_UBUF`), while `sendmsg` passes an array
 * (`ITER_IOVEC`) or, for in-kernel callers, a `kvec` array with the same
 * layout. Values are the kernel's own enum and have been stable since 5.19. */
#define ITER_UBUF 0
#define ITER_IOVEC 1
#define ITER_KVEC 3

/* Read the first buffer of a tcp_sendmsg. The payload belongs to user space,
 * but a few in-kernel callers pass a kvec, hence the kernel fallback.
 *
 * Returns 0 or a negative stage, which the caller logs: a host whose layout
 * differs should say so rather than silently produce nothing. */
static __always_inline int read_send_payload(__u64 msg, char *dst, __u32 *size_out) {
    __u8 kind = 0;
    if (bpf_probe_read_kernel(&kind, sizeof(kind), (void *)(msg + msg_iter_offset)) != 0) {
        return -1;
    }
    __u64 ptr = 0;
    if (bpf_probe_read_kernel(&ptr, sizeof(ptr), (void *)(msg + msg_iter_offset + iov_ptr_offset)) !=
            0 ||
        ptr == 0) {
        return -2;
    }
    __u64 base = 0;
    __u64 len = 0;
    if (kind == ITER_UBUF) {
        /* `write()` and `send()` hand over one buffer: the pointer *is* the
         * payload, and the byte count follows the union member. */
        base = ptr;
        if (bpf_probe_read_kernel(
                &len, sizeof(len), (void *)(msg + msg_iter_offset + iov_ptr_offset + 8)) != 0) {
            return -3;
        }
    } else if (kind == ITER_IOVEC || kind == ITER_KVEC) {
        struct iovec iov = {};
        if (bpf_probe_read_kernel(&iov, sizeof(iov), (void *)ptr) != 0) {
            return -4;
        }
        base = (__u64)iov.iov_base;
        len = iov.iov_len;
    } else {
        // BVEC, XARRAY, FOLIOQ: data that is not a user buffer.
        return -5;
    }
    /* The size argument must be provably in range: the length came from
     * memory, so the verifier has no idea how large it is. Masking with the
     * buffer's own power-of-two size both caps it and leaves a `var_off` the
     * verifier accepts, which a comparison cannot guarantee once the value has
     * been spilled. */
    __u32 n = (__u32)len & L7_MASK;
    if (n < 4 || base == 0) {
        return -6;
    }
    __builtin_memset(dst, 0, L7_SAMPLE);
    if (bpf_probe_read_user(dst, n, (void *)base) != 0) {
        return -7;
    }
    *size_out = n;
    return 0;
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
/* Sample the first writes of a connection and hand them to user space. The
 * budget is per window: a chatty connection must not fill the ring buffer,
 * but the first request of a fresh one always gets through. Server flows get
 * a smaller budget — their first bytes name the protocol, and a response
 * never carries a Host or a statement. */
SEC("kprobe/tcp_sendmsg_send")
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
    if (!stats) {
        return 0;
    }
    __sync_fetch_and_add(&stats->tx_bytes, size);

    __u32 budget = stats->role == ROLE_CLIENT ? L7_PER_WINDOW : L7_PER_WINDOW / 4;
    if (stats->l7_samples >= budget) {
        return 0;
    }
    struct l7_event ev = {};
    int status = read_send_payload(PARM2(ctx), (char *)ev.payload, &ev.len);
    if (status != 0) {
        return 0;
    }
    ev.key = entry->key;
    ev.cgroup = stats->cgroup;
    ev.flags = L7_HAS_FLOW;
    stats->l7_samples += 1;
    bpf_perf_event_output(ctx, (void *)&l7_events, BPF_F_CURRENT_CPU, &ev, sizeof(ev));
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
/* --------------------------------------------------------------------- */
/* TLS plaintext, via an optional uprobe on the TLS library               */
/* --------------------------------------------------------------------- */
/* A TLS library hands `SSL_write` the plaintext before it encrypts it. The
 * probe has no socket to key on, so the event carries the cgroup and the
 * agent attaches the parsed detail to the flows that cgroup owns. */
struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 4096);
    __type(key, __u64);
    __type(value, __u64);
} ssl_last SEC(".maps");

/* At most ten samples a second per cgroup. A TLS connection under polling
 * load would otherwise push one event per request, and none of them would say
 * anything the previous one did not. */
#define SSL_MIN_GAP_NS 100000000ULL

/* `SSL_write(SSL *ssl, const void *buf, int num)` and
 * `SSL_write_ex(SSL *ssl, const void *buf, size_t num, size_t *written)` put
 * the plaintext in the same argument, so one program covers both on every
 * ABI this file supports. */
SEC("uprobe/SSL_write")
int lidlesseye_ssl_write(struct pt_regs *ctx) {
    const char *buf = (const char *)PARM2(ctx);
    /* Bound `len` explicitly: the verifier does not infer a positive range
     * through `min()` of a register loaded from pt_regs, and rejects a size
     * it cannot prove non-negative. Clamping rather than masking matters:
     * `& 255` would make a 300-byte write look 44 bytes long. */
    __u64 num = PARM3(ctx);
    if (buf == 0 || num < 8) {
        return 0;
    }
    if (num > L7_SAMPLE) {
        num = L7_SAMPLE;
    }
    __u32 len = (__u32)num;
    if (buf == 0 || len < 8) {
        return 0;
    }
    struct l7_event ev = {};
    if (bpf_probe_read_user(ev.payload, len, buf) != 0) {
        return 0;
    }
    // Bulk data — a file upload, a result set — can never yield a Host or a
    // statement, and is the common case on a busy socket.
    if (!looks_like_request((char *)ev.payload, len)) {
        return 0;
    }
    __u64 cgroup = bpf_get_current_cgroup_id();
    __u64 now = bpf_ktime_get_ns();
    __u64 *last = bpf_map_lookup_elem((void *)&ssl_last, &cgroup);
    if (last && now - *last < SSL_MIN_GAP_NS) {
        return 0;
    }
    bpf_map_update_elem((void *)&ssl_last, &cgroup, &now, BPF_ANY);

    ev.len = len;
    ev.flags = 0;
    ev.cgroup = cgroup;
    bpf_perf_event_output(ctx, (void *)&l7_events, BPF_F_CURRENT_CPU, &ev, sizeof(ev));
    return 0;
}

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
