# Dashboard stage.
#
# The UI is embedded into the binary at compile time and is not committed, so it
# has to exist before `cargo build` runs. Building it in its own stage keeps
# Node out of the Rust image and lets the npm layer cache independently of the
# Rust sources; `sync` writes the static export into `web/dist`, which
# `lidlesseye-server`'s build script mirrors into the crate's `ui/` for
# `rust-embed`.
FROM node:22-bookworm-slim AS ui

WORKDIR /ui
COPY web/package.json web/package-lock.json ./
# `npm ci` from a lockfile generated on Windows leaves the native lightningcss
# binary out of the tree: the platform packages are optional dependencies, and
# the lock records the win32 one as resolved. On a cold cache that surfaces as
# `Cannot find module '../lightningcss.linux-x64-gnu.node'` from tailwind's
# postcss plugin, so the Linux package is installed explicitly and copied where
# lightningcss looks for it before the build runs.
RUN npm ci --no-audit --no-fund \
    && npm install --no-save --no-audit --no-fund lightningcss-linux-x64-gnu@1.33.0 \
    && cp node_modules/lightningcss-linux-x64-gnu/lightningcss.linux-x64-gnu.node \
        node_modules/lightningcss/lightningcss.linux-x64-gnu.node
COPY web ./
RUN npm run build && npm run sync

# bpftime stage.
#
# The userspace eBPF runtime is not packaged for Debian, so its own image is
# used as the build environment: it already carries clang-16, libbpf and the
# runtime shared libraries. Only the CLI, the agent the collector injects, and
# one compiled loader with its BPF object end up in the runtime image.
#
# Why it is here at all: a kernel uprobe on `malloc` cost 79 microseconds of
# system time per allocation on this cluster, against 2.6 microseconds with no
# probe attached. The same program run inside the process by bpftime costs
# hundreds of nanoseconds, and the price is a shared library injected into the
# target — which is why the switch that enables it is per workload.
FROM ghcr.io/eunomia-bpf/bpftime:latest AS bpftime

RUN cd /bpftime/third_party/libbpf/src && make -j"$(nproc)" libbpf.a
COPY deploy/bpftime /src/bpftime
RUN mkdir -p /out \
    && clang-16 -O2 -g -target bpf -D__TARGET_ARCH_x86 \
        -I/bpftime/third_party/vmlinux/x86 \
        -I/bpftime/third_party/bpftool/src/libbpf/include \
        -c /src/bpftime/alloc.bpf.c -o /out/alloc.bpf.o \
    && cc -O2 -Wall \
        -I/bpftime/third_party/bpftool/src/libbpf/include \
        /src/bpftime/loader.c \
        /bpftime/third_party/libbpf/src/libbpf.a -lelf -lz \
        -o /out/lidlesseye-bpftime
# The loader has to run inside the *target* pod's mount namespace (that is the
# only place its `/dev/shm` is), and the target pod may have an older glibc or
# no libelf at all. Ship the loader with its own dynamic linker and libraries so
# it can be copied into the target's `/tmp` and started there regardless of what
# the target image contains.
RUN mkdir -p /out/bundle/lib \
    && cp /out/lidlesseye-bpftime /out/bundle/ \
    && cp /out/alloc.bpf.o /out/bundle/ \
    && cp /root/.bpftime/libbpftime-syscall-server.so /out/bundle/ \
    && cp /root/.bpftime/libbpftime-agent.so /out/bundle/ \
    && cp /root/.bpftime/libbpftime-agent-transformer.so /out/bundle/ \
    && cp "$(readlink -f /lib64/ld-linux-x86-64.so.2)" /out/bundle/ \
    && for lib in $(ldd /out/lidlesseye-bpftime | awk '{print $3}' | grep '^/'); do \
        cp "$lib" /out/bundle/lib/; \
    done \
    && for lib in $(ldd /root/.bpftime/libbpftime-syscall-server.so | awk '{print $3}' | grep '^/' | sort -u); do \
        cp "$lib" /out/bundle/lib/; \
    done

# Build stage.
#
# The eBPF object is compiled here, so the builder needs clang/llvm even though
# the runtime image does not. `cargo build` also downloads the prebuilt
# `libduckdb.so` into target/release/build/.../out; copy it to a fixed path so
# the runtime stage can install it next to the binary.
FROM rust:1.98-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        clang \
        llvm \
        libelf-dev \
        libzstd-dev \
        pkg-config \
        ca-certificates \
        git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY web ./web
# The dashboard comes from the Node stage; the copies above carry the sources
# only, because `.dockerignore` drops any local `web/dist` and `ui/`.
# Seed `ui/` from the same export as `web/dist`: rust-embed expands `ui/` while
# build.rs runs, and having build.rs copy a fresh export over a stale `ui/`
# mid-compile can make the folder vanish from under it. Seeded from identical
# bytes, mirror() is a no-op and the folder stays stable at expansion time.
COPY --from=ui /ui/dist ./web/dist
RUN rm -rf crates/lidlesseye-server/ui && cp -a web/dist crates/lidlesseye-server/ui
# A dummy `main` keeps the dependency layer cacheable when only sources change.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    LIDLESSEYE_BUILD_BPF=1 cargo build --release -p lidlesseye \
    && cp target/release/lidlesseye /usr/local/bin/lidlesseye \
    && cp "$(find target/release/build -name libduckdb.so -type f -print -quit)" \
          /usr/local/lib/libduckdb.so

# Runtime stage.
#
# Ubuntu 24.04, not Debian bookworm, and only because of bpftime: its shared
# libraries and CLI are built against glibc 2.38 / libstdc++ 3.4.32, and loading
# them on bookworm's 2.36 fails with "version `GLIBC_2.38' not found". The
# binary itself is built on bookworm and runs unchanged here.
FROM ubuntu:24.04

LABEL org.opencontainers.image.title="LidlessEye" \
      org.opencontainers.image.description="Continuous eBPF profiling server and agent" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates libstdc++6 tini libelf1 \
        libncurses6 libtinfo6 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/lidlesseye /usr/local/bin/lidlesseye
COPY --from=builder /usr/local/lib/libduckdb.so /usr/local/lib/libduckdb.so
# The userspace eBPF runtime: the CLI injects the agent, the agent carries the
# malloc program into the target, and the loader drains the counters. The
# transformer is the piece that rewrites the target's libc entry, so `attach`
# needs it too. They live in /opt/bpftime *and* in the CLI's default install
# location, because `bpftime attach` resolves the .so from there and fails
# without saying why.
COPY --from=bpftime /root/.bpftime/bpftime /opt/bpftime/bpftime
COPY --from=bpftime /root/.bpftime/libbpftime-agent.so /opt/bpftime/libbpftime-agent.so
COPY --from=bpftime /root/.bpftime/libbpftime-agent-transformer.so /opt/bpftime/libbpftime-agent-transformer.so
COPY --from=bpftime /root/.bpftime/libbpftime-syscall-server.so /opt/bpftime/libbpftime-syscall-server.so
COPY --from=bpftime /out/alloc.bpf.o /opt/bpftime/alloc.bpf.o
COPY --from=bpftime /out/lidlesseye-bpftime /opt/bpftime/lidlesseye-bpftime
# The self-contained bundle the loader is started from inside the target pod's
# namespace: its own dynamic linker, its libraries, the object and the runtime.
COPY --from=bpftime /out/bundle /opt/bpftime/bundle
COPY --from=bpftime /root/.bpftime/bpftime /root/.bpftime/bpftime
COPY --from=bpftime /root/.bpftime/libbpftime-agent.so /root/.bpftime/libbpftime-agent.so
COPY --from=bpftime /root/.bpftime/libbpftime-agent-transformer.so /root/.bpftime/libbpftime-agent-transformer.so
COPY --from=bpftime /root/.bpftime/libbpftime-syscall-server.so /root/.bpftime/libbpftime-syscall-server.so
# libduckdb.so is a C++ library; /usr/local/lib is in Debian's default
# loader path once ldconfig has refreshed the cache.
RUN ldconfig

# The default data directory; mount a volume here in production.
VOLUME ["/var/lib/lidlesseye"]

# The agent DaemonSet runs with `hostPID`, so PID 1 inside the container is the
# node's init rather than tini. Registering tini as a child subreaper keeps it
# reaping the collector's children there too (and silences its warning).
ENV RUST_LOG=lidlesseye=info,lidlesseye_core=info \
    LD_LIBRARY_PATH=/usr/local/lib \
    TINI_SUBREAPER=1
EXPOSE 4040

# `tini` reaps the zombie processes the eBPF collector can inherit in a
# container PID namespace.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/lidlesseye"]
CMD ["serve", "--data-dir", "/var/lib/lidlesseye", "--listen", "0.0.0.0:4040"]
