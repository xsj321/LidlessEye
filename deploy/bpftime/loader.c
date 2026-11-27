// Loader for `alloc.bpf.c`: runs the malloc counter on the userspace eBPF
// runtime and prints what it counted.
//
// Started by the agent with `libbpftime-syscall-server.so` preloaded, then the
// agent injects `libbpftime-agent.so` into each target process with `bpftime
// attach <pid>`. The counters live in shared memory maps, so this process only
// has to drain them.
//
// Output: one JSON object per line, one line per (pid, stack) that was touched
// since the previous drain. The agent parses it and turns the stack into frames:
//   {"pid":1234,"bytes":4096,"count":7,"stack":"0x7f..,0x7f.."}
// A stack that the runtime could not record prints as an empty string.
//
// SPDX-License-Identifier: (LGPL-2.1 OR BSD-2-Clause)
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <bpf/bpf.h>
#include <bpf/libbpf.h>

// Matches `struct alloc_key` and `struct alloc_val` in alloc.bpf.c.
struct alloc_key {
	uint32_t pid;
	int32_t stack;
};

struct alloc_val {
	uint64_t bytes;
	uint64_t count;
};

// Must stay equal to `SAMPLING_RATE` in alloc.bpf.c. It is only used for the
// log line: the byte and count values coming out of the maps have already been
// scaled back up by the BPF program, so nothing here multiplies again.
#define SAMPLING_RATE 64

static volatile int exiting = 0;

static void on_signal(int sig)
{
	(void)sig;
	exiting = 1;
}

static int on_libbf(enum libbpf_print_level level, const char *fmt, va_list args)
{
	// Library chatter goes to stderr: stdout carries the JSON the agent parses,
	// and a single stray log line there would look like a malformed sample.
	if (level != LIBBPF_DEBUG) {
		vfprintf(stderr, fmt, args);
	}
	return 0;
}

static void print_one(int stacks_fd, int allocs_fd, const struct alloc_key *key,
		      const struct alloc_val *val)
{
	uint64_t ips[32];
	memset(ips, 0, sizeof(ips));

	printf("{\"pid\":%u,\"bytes\":%llu,\"count\":%llu,\"stack\":\"", key->pid,
	       (unsigned long long)val->bytes, (unsigned long long)val->count);

	if (key->stack >= 0 && bpf_map_lookup_elem(stacks_fd, &key->stack, ips) == 0) {
		for (size_t i = 0; i < sizeof(ips) / sizeof(ips[0]); i++) {
			if (ips[i] == 0) {
				break;
			}
			printf("%s0x%llx", i == 0 ? "" : ",",
			       (unsigned long long)ips[i]);
		}
	}
	printf("\"}\n");
	// Drop what was just reported so the next interval counts only new calls.
	bpf_map_delete_elem(allocs_fd, (void *)key);
}

static unsigned long long g_alloc_rows;
static unsigned long long g_batch;
static unsigned long long g_hit_rows;

static int drain(int stacks_fd, int allocs_fd)
{
	struct alloc_key key = { 0 }, next = { 0 };
	struct alloc_val val = { 0 };
	int first = 1;

	for (;;) {
		int err = bpf_map_get_next_key(allocs_fd, first ? NULL : &key, &next);
		if (err) {
			if (errno == ENOENT) {
				return 0;
			}
			fprintf(stderr, "get_next_key: %s\n", strerror(errno));
			return -1;
		}
		if (bpf_map_lookup_elem(allocs_fd, &next, &val) == 0) {
			print_one(stacks_fd, allocs_fd, &next, &val);
			g_alloc_rows++;
		}
		key = next;
		first = 0;
	}
}

static int drain_hits(int hits_fd, int sampled_fd)
{
	uint32_t pid = 0, next = 0;
	uint64_t count = 0;
	int first = 1;

	if (hits_fd < 0) {
		return 0;
	}
	for (;;) {
		int err = bpf_map_get_next_key(hits_fd, first ? NULL : &pid, &next);
		if (err) {
			if (errno == ENOENT) {
				return 0;
			}
			fprintf(stderr, "hits get_next_key: %s\n", strerror(errno));
			return -1;
		}
		if (bpf_map_lookup_elem(hits_fd, &next, &count) == 0) {
			// A cumulative per-pid count: the probe ran this many times. Zero
			// anywhere means the program never entered, not that the map lost
			// rows. `samples` is how many of those calls were recorded, so the
			// two together show the sampling ratio actually achieved.
			uint64_t taken = 0;
			if (sampled_fd >= 0) {
				bpf_map_lookup_elem(sampled_fd, &next, &taken);
			}
			printf("{\"hits\":true,\"pid\":%u,\"calls\":%llu,\"samples\":%llu}\n",
			       next, (unsigned long long)count,
			       (unsigned long long)taken);
			g_hit_rows++;
		}
		pid = next;
		first = 0;
	}
	return 0;
}

int main(int argc, char **argv)
{
	struct bpf_object *obj = NULL;
	struct bpf_program *prog = NULL;
	struct bpf_link *link = NULL;
	int err = 0;

	/* stdout is a pipe when the shim reads it, and a pipe is block buffered:
	   without this line every sample waits in libc's buffer until the loader
	   exits, which looks exactly like "the reader delivers nothing". */
	setvbuf(stdout, NULL, _IOLBF, 0);

	if (argc < 2) {
		fprintf(stderr, "usage: %s <alloc.bpf.o> [interval-seconds]\n", argv[0]);
		return 2;
	}
	int interval = argc > 2 ? atoi(argv[2]) : 5;
	long rate = argc > 3 ? atol(argv[3]) : SAMPLING_RATE;
	if (interval < 1) {
		interval = 1;
	}

	libbpf_set_print(on_libbf);
	signal(SIGINT, on_signal);
	signal(SIGTERM, on_signal);

	obj = bpf_object__open_file(argv[1], NULL);
	if (!obj) {
		fprintf(stderr, "open %s: %s\n", argv[1], strerror(errno));
		return 1;
	}
	err = bpf_object__load(obj);
	if (err) {
		fprintf(stderr, "load %s: %s\n", argv[1], strerror(-err));
		bpf_object__close(obj);
		return 1;
	}

	prog = bpf_object__find_program_by_name(obj, "count_malloc");
	if (!prog) {
		fprintf(stderr, "program count_malloc not found\n");
		bpf_object__close(obj);
		return 1;
	}
	LIBBPF_OPTS(bpf_uprobe_opts, opts, .func_name = "malloc", .retprobe = false);
	link = bpf_program__attach_uprobe_opts(prog, -1, "libc.so.6", 0, &opts);
	if (!link) {
		fprintf(stderr, "attach malloc: %s\n", strerror(errno));
		bpf_object__close(obj);
		return 1;
	}

	int allocs_fd = bpf_object__find_map_fd_by_name(obj, "allocs");
	int stacks_fd = bpf_object__find_map_fd_by_name(obj, "stacks");
	int cfg_fd = bpf_object__find_map_fd_by_name(obj, "cfg");
	int hits_fd = bpf_object__find_map_fd_by_name(obj, "hits");
	int sampled_fd = bpf_object__find_map_fd_by_name(obj, "sampled");
	if (allocs_fd < 0 || stacks_fd < 0) {
		fprintf(stderr, "maps allocs=%d stacks=%d\n", allocs_fd, stacks_fd);
		bpf_link__destroy(link);
		bpf_object__close(obj);
		return 1;
	}

	if (cfg_fd >= 0) {
		uint32_t zero = 0;
		uint64_t value = (uint64_t)rate;
		bpf_map_update_elem(cfg_fd, &zero, &value, BPF_ANY);
	}
	fprintf(stderr, "lidlesseye-bpftime: counting malloc, interval %ds, sampling 1/%ld\n",
		interval, rate);
	// The uprobe is registered by this process, so the target only sees it if it
	// is in the same mount namespace and this runtime's maps are shared. Saying
	// which of those we believe is true saves a lot of guessing.
	fprintf(stderr, "lidlesseye-bpftime: hits map fd %d, allocs fd %d\n",
		hits_fd, allocs_fd);
	while (!exiting) {
		sleep((unsigned)interval);
		if (exiting) {
			break;
		}
		if (drain_hits(hits_fd, sampled_fd) != 0) {
			break;
		}
		if (drain(stacks_fd, allocs_fd) != 0) {
			break;
		}
		// A tick with zero rows is the interesting case: it separates "the
		// probe never ran" from "the probe ran and the map has no rows", and
		// without it both look like silence.
		fprintf(stderr, "tick alloc_rows=%llu hit_rows=%llu\n", g_alloc_rows,
			g_hit_rows);
		// The batch marker goes to stdout and carries the row count, so the
		// reader can prove it received every row the loader produced. Without
		// it a short read (pipe backpressure, a killed reader) looks exactly
		// like an idle process.
		printf("{\"batch\":%llu,\"rows\":%llu}\n", (unsigned long long)++g_batch,
		       g_alloc_rows);
		g_alloc_rows = 0;
		g_hit_rows = 0;
		fflush(stdout);
	}

	bpf_link__destroy(link);
	bpf_object__close(obj);
	return 0;
}
