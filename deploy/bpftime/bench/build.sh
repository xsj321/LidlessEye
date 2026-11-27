#!/bin/sh
set -e
cd /bpftime/third_party/libbpf/src
[ -f libbpf.a ] || make libbpf.a >/dev/null 2>&1
INC="-I/bpftime/third_party/vmlinux/x86 -I/bpftime/third_party/bpftool/src/libbpf/include"
for f in empty percpu hash; do
	clang-16 -O2 -g -target bpf -D__TARGET_ARCH_x86 $INC -c /w/$f.bpf.c -o /w/$f.bpf.o
	echo "built:$f"
done
