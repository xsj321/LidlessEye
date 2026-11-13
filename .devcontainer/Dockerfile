# Development image for LidlessEye.
#
# Provides a full Linux toolchain for building and *running* the eBPF
# profiler: clang/llvm for the BPF target, rust, node for the UI build and the
# usual suspects for native dependencies. DuckDB is linked against the
# prebuilt shared library downloaded at build time; cmake and g++ are kept
# only for the optional `bundled-cmake` path.
#
#   docker build -t lidlesseye-dev .devcontainer
#   docker run --rm -it --privileged --pid=host -v "$PWD:/work" lidlesseye-dev
#
# Node comes from the official image rather than Debian's `nodejs` package:
# bookworm ships 18, and the dashboard's PostCSS pipeline loads ESM-only
# packages (`magic-string`) that Node 18 cannot `require()`. `make build` runs
# the dashboard build first, so a too-old Node breaks the Rust build too.
FROM node:22-bookworm-slim AS node

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
        python3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -sf /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -sf /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

# rustfmt / clippy are handy when iterating on the code base.
RUN rustup component add rustfmt clippy

WORKDIR /work
