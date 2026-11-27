/* Hook-only variant: count allocations per pid, no user stack capture.
 *
 * Exists to separate the cost of entering the uprobe from the cost of walking
 * and hashing the user stack: same attach path, same map update, one fewer
 * helper call. The stack map stays in the object so the loader links unchanged.
 */
#define BPF_NO_GLOBAL_DATA
#include <vmlinux.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

#define MAX_STACK_DEPTH 32

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

SEC("uprobe/libc.so.6:malloc")
int count_malloc(struct pt_regs *ctx)
{
	__u32 pid = bpf_get_current_pid_tgid() >> 32;
	__u64 size = PT_REGS_PARM1(ctx);

	struct alloc_key key = {};
	key.pid = pid;
	// -1 means "no stack": this variant never walks one.
	key.stack = -1;

	struct alloc_val *val = bpf_map_lookup_elem(&allocs, &key);
	if (val) {
		__sync_fetch_and_add(&val->bytes, size);
		__sync_fetch_and_add(&val->count, 1);
		return 0;
	}

	struct alloc_val fresh = { .bytes = size, .count = 1 };
	bpf_map_update_elem(&allocs, &key, &fresh, BPF_NOEXIST);
	return 0;
}

char LICENSE[] SEC("license") = "GPL";
