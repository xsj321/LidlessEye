// Cost probe A: the hook body does essentially nothing.
//
// `allocs` and `stacks` exist only because the loader refuses to run without
// them; they are referenced behind a branch that is never taken at run time, so
// the verifier keeps the maps and the measurement still pays no map operation.
// Without that, the comparison would measure "the loader could not start".
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

SEC("uprobe/libc.so.6:malloc")
int count_malloc(struct pt_regs *ctx)
{
	// `ctx` is a kernel pointer, so this branch never runs. It exists only to
	// keep both maps alive in the object.
	if ((unsigned long)ctx == 1UL) {
		struct alloc_key key = {};
		__u64 size = PT_REGS_PARM1(ctx);
		struct alloc_val value = { size, 1 };
		bpf_map_update_elem(&allocs, &key, &value, BPF_NOEXIST);
		(void)bpf_get_stackid(ctx, &stacks, BPF_F_USER_STACK);
	}
	return 0;
}

char LICENSE[] SEC("license") = "GPL";
