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
# A dummy `main` keeps the dependency layer cacheable when only sources change.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    LIDLESSEYE_BUILD_BPF=1 cargo build --release -p lidlesseye \
    && cp target/release/lidlesseye /usr/local/bin/lidlesseye \
    && cp "$(find target/release/build -name libduckdb.so -type f -print -quit)" \
          /usr/local/lib/libduckdb.so

# Runtime stage.
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="LidlessEye" \
      org.opencontainers.image.description="Continuous eBPF profiling server and agent" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 tini \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/lidlesseye /usr/local/bin/lidlesseye
COPY --from=builder /usr/local/lib/libduckdb.so /usr/local/lib/libduckdb.so
# libduckdb.so is a C++ library; /usr/local/lib is in Debian's default
# loader path once ldconfig has refreshed the cache.
RUN ldconfig

# The default data directory; mount a volume here in production.
VOLUME ["/var/lib/lidlesseye"]

ENV RUST_LOG=lidlesseye=info \
    LD_LIBRARY_PATH=/usr/local/lib
EXPOSE 4040

# `tini` reaps the zombie processes the eBPF collector can inherit in a
# container PID namespace.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/lidlesseye"]
CMD ["serve", "--data-dir", "/var/lib/lidlesseye", "--listen", "0.0.0.0:4040"]
