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

/* Keep these in sync with lidlesseye_core::servicemap::protocol. A connection
 * can carry more than one application protocol over its lifetime, so this is
 * a bit set rather than a single enum. */
#define PROTO_HTTP        (1U << 0)
#define PROTO_HTTPS       (1U << 1)
#define PROTO_POSTGRES    (1U << 2)
#define PROTO_MYSQL       (1U << 3)
#define PROTO_MONGODB     (1U << 4)
#define PROTO_REDIS       (1U << 5)
#define PROTO_MEMCACHED   (1U << 6)
#define PROTO_CASSANDRA   (1U << 7)
#define PROTO_KAFKA       (1U << 8)
#define PROTO_CLICKHOUSE  (1U << 9)
#define PROTO_ZOOKEEPER   (1U << 10)
#define PROTO_FOUNDATIONDB (1U << 11)
#define PROTO_HTTP2       (1U << 12)
#define PROTO_DNS         (1U << 13)
#define PROTO_GRPC        (1U << 14)

/* ---- helper ABI ------------------------------------------------------- */
static void *(*bpf_map_lookup_elem)(void *map, const void *key) = (void *)1;
static long (*bpf_map_update_elem)(void *map, const void *key, const void *value,
                                   __u64 flags) = (void *)2;
static long (*bpf_map_delete_elem)(void *map, const void *key) = (void *)3;
static long (*bpf_probe_read_kernel)(void *dst, __u32 size, const void *src) = (void *)113;
static long (*bpf_probe_read_user)(void *dst, __u32 size, const void *src) = (void *)115;
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

/* Enough of `msghdr`/`iov_iter` to reach the first payload iovec. CO-RE
 * relocates by member name, so only the members we read are described here;
 * the rest of the kernel's layout is filled in at load time. */
struct iovec {
    void *iov_base;
    __u64 iov_len;
} __attribute__((preserve_access_index));

struct iov_iter {
    __u64 count;
    const struct iovec *iov;
} __attribute__((preserve_access_index));

struct msghdr {
    struct iov_iter msg_iter;
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
    /* Bit set of protocols seen on this connection, from the payload sniffer.
     * Occupies the old padding slot: the map value stays the same size. */
    __u32 protocols;
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
/* Application protocol sniffing                                          */
/* --------------------------------------------------------------------- */
/* A kprobe on tcp_sendmsg sees the application's buffer before the kernel
 * splits it into segments. Reading the first iovec is enough to recognise the
 * request line or binary frame header of the protocols the service map cares
 * about, and it keeps the probe cheap: one bounded user read per send, no
 * per-packet work and no perf-buffer events.
 *
 * The parsers deliberately answer "which protocol", not "which request".
 * Getting that far costs per-request state and a userspace decoder; naming the
 * wire protocol on an edge is what the map needs, and a conservative guess is
 * better than labelling a long-lived connection as whatever it carried once.
 */

#define PAYLOAD_SAMPLE 128
#define PAYLOAD_MASK (PAYLOAD_SAMPLE - 1)

static __always_inline __u32 be32(const char *p) {
    return ((__u32)(__u8)p[0] << 24) | ((__u32)(__u8)p[1] << 16) |
           ((__u32)(__u8)p[2] << 8) | (__u32)(__u8)p[3];
}

static __always_inline __u32 le32(const char *p) {
    return (__u32)(__u8)p[0] | ((__u32)(__u8)p[1] << 8) |
           ((__u32)(__u8)p[2] << 16) | ((__u32)(__u8)p[3] << 24);
}

static __always_inline __u32 be16(const char *p) {
    return ((__u32)(__u8)p[0] << 8) | (__u32)(__u8)p[1];
}

static __always_inline char at(const char *buf, __u64 offset) {
    return buf[offset & PAYLOAD_MASK];
}

static __always_inline __u32 is_http(const char *b, __u32 size) {
    if (size < 8) {
        return 0;
    }
    if (at(b, 0) == 'G' && at(b, 1) == 'E' && at(b, 2) == 'T') {
        return PROTO_HTTP;
    }
    if (at(b, 0) == 'P' && at(b, 1) == 'O' && at(b, 2) == 'S' && at(b, 3) == 'T') {
        return PROTO_HTTP;
    }
    if (at(b, 0) == 'P' && at(b, 1) == 'U' && at(b, 2) == 'T') {
        return PROTO_HTTP;
    }
    if (at(b, 0) == 'H' && at(b, 1) == 'E' && at(b, 2) == 'A' && at(b, 3) == 'D') {
        return PROTO_HTTP;
    }
    if (at(b, 0) == 'D' && at(b, 1) == 'E' && at(b, 2) == 'L' && at(b, 3) == 'E' &&
        at(b, 4) == 'T' && at(b, 5) == 'E') {
        return PROTO_HTTP;
    }
    if (at(b, 0) == 'O' && at(b, 1) == 'P' && at(b, 2) == 'T' && at(b, 3) == 'I' &&
        at(b, 4) == 'O' && at(b, 5) == 'N' && at(b, 6) == 'S') {
        return PROTO_HTTP;
    }
    if (at(b, 0) == 'P' && at(b, 1) == 'A' && at(b, 2) == 'T' && at(b, 3) == 'C' &&
        at(b, 4) == 'H') {
        return PROTO_HTTP;
    }
    // A response travels on the server's socket; recognising it matters for
    // inbound-only flows whose request predates the agent.
    if (at(b, 0) == 'H' && at(b, 1) == 'T' && at(b, 2) == 'T' && at(b, 3) == 'P' &&
        at(b, 4) == '/' && at(b, 5) >= '0' && at(b, 5) <= '9' && at(b, 6) == '.' &&
        at(b, 7) >= '0' && at(b, 7) <= '9') {
        return PROTO_HTTP;
    }
    return 0;
}

static __always_inline __u32 is_https(const char *b, __u32 size) {
    if (size < 6 || (__u8)at(b, 0) != 0x16 || (__u8)at(b, 1) != 0x03) {
        return 0;
    }
    if ((__u8)at(b, 2) > 0x04) {
        return 0;
    }
    return be16(b + 3) + 5 <= size ? PROTO_HTTPS : 0;
}

static __always_inline __u32 is_http2(const char *b, __u32 size) {
    if (size >= 24 && at(b, 0) == 'P' && at(b, 1) == 'R' && at(b, 2) == 'I' &&
        at(b, 3) == ' ' && at(b, 4) == '*') {
        return PROTO_HTTP2;
    }
    if (size < 9) {
        return 0;
    }
    __u32 length = be32(b) >> 8;
    __u8 type = (__u8)at(b, 3);
    // Settings, headers, data, Windows update and ping are the frames a
    // client or server sends first; type 0 also covers an empty DATA frame.
    if (type > 0x9 || length + 9 > size) {
        return 0;
    }
    return PROTO_HTTP2;
}

static __always_inline __u32 is_postgres(const char *b, __u32 size) {
    if (size < 5) {
        return 0;
    }
    __u32 length = be32(b + 1);
    char cmd = at(b, 0);
    // Startup packet: 196608 is protocol 3.0 in network byte order.
    if (length == 196608) {
        return PROTO_POSTGRES;
    }
    if (length + 1 == size &&
        (cmd == 'Q' || cmd == 'P' || cmd == 'C' || cmd == 'S')) {
        return PROTO_POSTGRES;
    }
    if ((cmd == 'E' || cmd == 'T' || cmd == 'D' || cmd == 'C' || cmd == 't' ||
         cmd == '1' || cmd == '2') && length + 1 <= size) {
        return PROTO_POSTGRES;
    }
    return 0;
}

static __always_inline __u32 is_mysql(const char *b, __u32 size) {
    if (size < 5) {
        return 0;
    }
    __u32 length = (__u32)(__u8)at(b, 0) | ((__u32)(__u8)at(b, 1) << 8) |
                   ((__u32)(__u8)at(b, 2) << 16);
    if (length + 4 != size || (__u8)at(b, 3) != 0) {
        return 0;
    }
    __u8 cmd = (__u8)at(b, 4);
    if (cmd == 0 || cmd == 1 || cmd == 2 || cmd == 3 || cmd == 0x16 || cmd == 0x17 ||
        cmd == 0x19) {
        return PROTO_MYSQL;
    }
    return 0;
}

static __always_inline __u32 is_mongodb(const char *b, __u32 size) {
    if (size < 16) {
        return 0;
    }
    __u32 length = (__u32)(__s32)be32(b);
    __u32 opcode = (__u32)(__s32)be32(b + 12);
    if (length < 16 || length != size) {
        return 0;
    }
    // OP_MSG and OP_COMPRESSED are what modern drivers use; OP_QUERY (2004)
    // is still emitted by older ones and by the handshake.
    if (opcode == 2013 || opcode == 2012 || opcode == 2004) {
        return PROTO_MONGODB;
    }
    return 0;
}

static __always_inline __u32 is_redis(const char *b, __u32 size) {
    if (size < 11 || at(b, 0) != '*' || at(b, 1) < '0' || at(b, 1) > '9') {
        return 0;
    }
    __u64 p;
    if (at(b, 2) == '\r' && at(b, 3) == '\n') {
        p = 4;
    } else if (at(b, 2) >= '0' && at(b, 2) <= '9' && at(b, 3) == '\r' &&
               at(b, 4) == '\n') {
        p = 5;
    } else {
        return 0;
    }
    if (at(b, p) != '$' || at(b, p + 1) < '0' || at(b, p + 1) > '9') {
        return 0;
    }
    if (at(b, p + 2) == '\r' && at(b, p + 3) == '\n') {
        p += 4;
    } else if (at(b, p + 2) >= '0' && at(b, p + 2) <= '9' && at(b, p + 3) == '\r' &&
               at(b, p + 4) == '\n') {
        p += 5;
    } else {
        return 0;
    }
    char c = at(b, p);
    return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ? PROTO_REDIS : 0;
}

static __always_inline __u32 is_memcached(const char *b, __u32 size) {
    if (size < 9 || at(b, size - 2) != '\r' || at(b, size - 1) != '\n') {
        return 0;
    }
    if (at(b, 0) == 's' && at(b, 1) == 'e' && at(b, 2) == 't' && at(b, 3) == ' ') {
        return PROTO_MEMCACHED;
    }
    if (at(b, 0) == 'g' && (at(b, 1) == 'a' || at(b, 1) == 'e') && at(b, 2) == 't' &&
        (at(b, 3) == ' ' || at(b, 3) == 's')) {
        return PROTO_MEMCACHED;
    }
    if (at(b, 0) == 'a' && at(b, 1) == 'd' && at(b, 2) == 'd' && at(b, 3) == ' ') {
        return PROTO_MEMCACHED;
    }
    if (at(b, 0) == 'i' && at(b, 1) == 'n' && at(b, 2) == 'c' && at(b, 3) == 'r' &&
        at(b, 4) == ' ') {
        return PROTO_MEMCACHED;
    }
    if (at(b, 0) == 'd' && at(b, 1) == 'e' && at(b, 2) == 'c' && at(b, 3) == 'r' &&
        at(b, 4) == ' ') {
        return PROTO_MEMCACHED;
    }
    if (at(b, 0) == 'V' && at(b, 1) == 'A' && at(b, 2) == 'L') {
        return PROTO_MEMCACHED;
    }
    if (at(b, 0) == 'S' && at(b, 1) == 'T' && at(b, 2) == 'O') {
        return PROTO_MEMCACHED;
    }
    return 0;
}

static __always_inline __u32 is_cassandra(const char *b, __u32 size) {
    if (size < 5) {
        return 0;
    }
    __u8 version = (__u8)at(b, 0);
    __u8 opcode = (__u8)at(b, 4);
    // Requests use 0x04, responses 0x84; v3 and v5 are still in the wild.
    if (version != 0x03 && version != 0x04 && version != 0x05 && version != 0x83 &&
        version != 0x84 && version != 0x85) {
        return 0;
    }
    return (opcode == 0x00 || opcode == 0x07 || opcode == 0x08 || opcode == 0x0a ||
            opcode == 0x0d)
               ? PROTO_CASSANDRA
               : 0;
}

static __always_inline __u32 is_kafka(const char *b, __u32 size) {
    if (size < 12) {
        return 0;
    }
    __u32 length = be32(b);
    __u32 api_key = be16(b + 4);
    __s32 correlation = (__s32)be32(b + 8);
    return (length + 4 == size && correlation >= 0 && api_key <= 67) ? PROTO_KAFKA : 0;
}

static __always_inline __u32 is_clickhouse(const char *b, __u32 size) {
    if (size < 40 || (__u8)at(b, 0) != 1) {
        return 0;
    }
    __u8 query_id_len = (__u8)at(b, 1);
    if (query_id_len != 0 && query_id_len != 36) {
        return 0;
    }
    __u64 off = 2 + query_id_len;
    __u8 kind = (__u8)at(b, off);
    if (kind != 1 && kind != 2) {
        return 0;
    }
    off += 1;
    __u8 user_len = (__u8)at(b, off);
    if (user_len > 63) {
        return 0;
    }
    off += 1 + user_len;
    __u8 initial_len = (__u8)at(b, off);
    if (initial_len > 63) {
        return 0;
    }
    off += 1 + initial_len;
    __u8 addr_len = (__u8)at(b, off);
    if (addr_len > 48) {
        return 0;
    }
    off += 1 + addr_len + 8;
    __u8 iface = (__u8)at(b, off);
    return (iface >= 1 && iface <= 8) ? PROTO_CLICKHOUSE : 0;
}

static __always_inline __u32 is_zookeeper(const char *b, __u32 size) {
    if (size < 12) {
        return 0;
    }
    __u32 length = be32(b);
    __s32 xid = (__s32)be32(b + 4);
    __s32 opcode = (__s32)be32(b + 8);
    if (length + 4 != size || (xid < 0 && xid != -1 && xid != -2)) {
        return 0;
    }
    return ((opcode >= 0 && opcode <= 21) || opcode == -11 || opcode == 100 ||
            opcode == 101 || opcode == -1)
               ? PROTO_ZOOKEEPER
               : 0;
}

static __always_inline __u32 is_fdb_id(__u32 id) {
    // The common file identifiers from FoundationDB's Flow serialisation.
    // The full list lives in flow's generated headers; these cover the client
    // reads/writes and the worker frames that make up ordinary cluster traffic.
    switch (id) {
    case 8454530U:  /* GetValueRequest */
    case 6795746U:  /* GetKeyValuesRequest */
    case 6795747U:  /* GetMappedKeyValuesRequest */
    case 93948U:    /* CommitTransactionRequest */
    case 838566U:   /* GetReadVersionRequest */
    case 7554186U:  /* SetRequest */
    case 8500026U:  /* ClearRequest */
    case 10457870U: /* GetKeyRequest */
    case 1378929U:  /* GetValueReply */
    case 1783066U:  /* GetKeyValuesReply */
    case 1783067U:  /* GetMappedKeyValuesReply */
    case 11226513U: /* GetKeyReply */
    case 15709388U: /* GetReadVersionReply */
    case 14332605U: /* RegisterWorkerRequest */
    case 1254174U:  /* GetWorkersRequest */
        return 1;
    default:
        return 0;
    }
}

static __always_inline __u32 is_foundationdb(const char *b, __u32 size) {
    if (size < 36) {
        return 0;
    }
    // ConnectPacket carries 0x1FDB at bytes 10-11, then one or more length-
    // prefixed Flow messages. Plain and TLS frames put the file identifier
    // at different offsets.
    if ((__u8)at(b, 10) == 0xdb && (__u8)at(b, 11) == 0x1f) {
        return PROTO_FOUNDATIONDB;
    }
    return (is_fdb_id(le32(b + 24)) || is_fdb_id(le32(b + 32))) ? PROTO_FOUNDATIONDB : 0;
}

static __always_inline __u32 is_dns(const char *b, __u32 size) {
    if (size < 12 || be16(b) + 2 != size) {
        return 0;
    }
    // Standard query/response with no authoritative-answer bit nonsense: a
    // TCP DNS message is length-prefixed, which is what tells it from Kafka.
    if ((__u8)at(b, 2) & 0x80) {
        return PROTO_DNS;
    }
    return ((__u8)at(b, 3) & 0x0f) == 0 ? PROTO_DNS : 0;
}

static __always_inline __u32 classify_payload(char *b, __u32 size) {
    __u32 mask = 0;
    mask |= is_http(b, size);
    mask |= is_https(b, size);
    mask |= is_http2(b, size);
    mask |= is_postgres(b, size);
    mask |= is_mysql(b, size);
    mask |= is_mongodb(b, size);
    mask |= is_redis(b, size);
    mask |= is_memcached(b, size);
    mask |= is_cassandra(b, size);
    mask |= is_kafka(b, size);
    mask |= is_clickhouse(b, size);
    mask |= is_zookeeper(b, size);
    mask |= is_foundationdb(b, size);
    mask |= is_dns(b, size);
    return mask;
}

/* Read the first iovec of a tcp_sendmsg. The payload belongs to user space,
 * but a few in-kernel callers pass a kvec, hence the kernel fallback. */
static __always_inline int read_send_payload(struct msghdr *msg, char *dst, __u32 *size_out) {
    const struct iovec *vec = 0;
    if (bpf_probe_read_kernel(&vec, sizeof(vec), &msg->msg_iter.iov) != 0 || vec == 0) {
        return -1;
    }
    struct iovec iov = {};
    if (bpf_probe_read_kernel(&iov, sizeof(iov), vec) != 0) {
        return -1;
    }
    __u64 len = iov.iov_len;
    if (len > PAYLOAD_SAMPLE) {
        len = PAYLOAD_SAMPLE;
    }
    if (len < 4 || iov.iov_base == 0) {
        return -1;
    }
    __builtin_memset(dst, 0, PAYLOAD_SAMPLE);
    if (bpf_probe_read_user(dst, (__u32)len, iov.iov_base) != 0 &&
        bpf_probe_read_kernel(dst, (__u32)len, iov.iov_base) != 0) {
        return -1;
    }
    *size_out = (__u32)len;
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
    if (!stats) {
        return 0;
    }
    __sync_fetch_and_add(&stats->tx_bytes, size);

    /* Once a protocol is known it stays known for the window: a healthy
     * connection reuses the same protocol, and re-reading every iovec would
     * turn one classify per send into one per request. */
    if (stats->protocols == 0) {
        char payload[PAYLOAD_SAMPLE] = {};
        __u32 used = 0;
        if (read_send_payload((struct msghdr *)PARM2(ctx), payload, &used) == 0) {
            __u32 detected = classify_payload(payload, used);
            if (detected != 0) {
                // Not atomic on purpose: BPF has no 32-bit atomic OR, and a
                // lost bit here only means one flow falls back to its port.
                stats->protocols |= detected;
            }
        }
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
