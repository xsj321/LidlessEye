# Development image for LidlessEye.
#
# Provides a full Linux toolchain for building and *running* the eBPF
# profiler: clang/llvm for the BPF target and the C++ compiler for the
# statically linked DuckDB (cmake is installed for the optional
# `bundled-cmake` path), rust, node for the UI build and the usual suspects
# for native dependencies.
#
#   docker build -t lidlesseye-dev .devcontainer
#   docker run --rm -it --privileged --pid=host -v "$PWD:/work" lidlesseye-dev
#
FROM rust:1.98-bookworm

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TERM_COLOR=always

RUN apt-get update && apt-get install -y --no-install-recommends \
        clang \
        llvm \
        cmake \
        libelf-dev \
        libzstd-dev \
        pkg-config \
        zlib1g-dev \
        libssl-dev \
        curl \
        ca-certificates \
        git \
        jq \
        iproute2 \
        procps \
        less \
        vim-tiny \
        nodejs \
        npm \
        python3 \
    && rm -rf /var/lib/apt/lists/*

# rustfmt / clippy are handy when iterating on the code base.
RUN rustup component add rustfmt clippy

WORKDIR /work
