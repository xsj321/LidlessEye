// Cost probe C: what the shipping program does on every call.
//
// pid lookup + hash-map atomic add + a random draw: the work the reader pays on
// all but one of every 64 allocations. C - B is the price of the hash map
// (key construction, lookup, atomic); C - A is the whole per-call body.
#define BPF_NO_GLOBAL_DATA
#include <vmlinux.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

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
	__uint(value_size, 32 * sizeof(__u64));
	__uint(max_entries, 8192);
} stacks SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 16384);
	__type(key, struct alloc_key);
	__type(value, struct alloc_val);
} allocs SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 16384);
	__type(key, __u32);
	__type(value, __u64);
} hits SEC(".maps");

SEC("uprobe/libc.so.6:malloc")
int count_malloc(struct pt_regs *ctx)
{
	if ((unsigned long)ctx == 1UL) {
		struct alloc_key key = {};
		struct alloc_val value = { PT_REGS_PARM1(ctx), 1 };
		bpf_map_update_elem(&allocs, &key, &value, BPF_NOEXIST);
		(void)bpf_get_stackid(ctx, &stacks, BPF_F_USER_STACK);
	}

	__u32 pid = bpf_get_current_pid_tgid() >> 32;
	__u64 *hit = bpf_map_lookup_elem(&hits, &pid);
	if (hit) {
		__sync_fetch_and_add(hit, 1ULL);
	} else {
		__u64 one = 1;
		bpf_map_update_elem(&hits, &pid, &one, BPF_NOEXIST);
	}
	(void)bpf_get_prandom_u32();
	return 0;
}

char LICENSE[] SEC("license") = "GPL";
