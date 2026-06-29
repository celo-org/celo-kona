FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y libclang-dev pkg-config && rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json --bin celo-reth

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
ARG BUILD_PROFILE=release
# Profiling-friendly build: retain frame pointers + line-table debuginfo so
# perf / eBPF profilers (Parca, Grafana Pyroscope) can unwind and symbolize
# celo-reth stacks in production. The release profile already keeps the symbol
# table (no `strip`); these flags add reliable stack unwinding (frame pointers)
# and source line numbers (line-tables) at ~1% runtime cost and a modest binary
# size increase. Set in the builder stage so chef-cooked deps and the final
# binary are built with identical flags (avoids a full dep recompile).
ARG RUSTFLAGS="-C force-frame-pointers=yes -C debuginfo=line-tables-only"
ENV RUSTFLAGS=$RUSTFLAGS
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo chef cook --profile $BUILD_PROFILE --recipe-path recipe.json --bin celo-reth
COPY . .
# SHA injected AFTER chef cook so dep-cache layer stays valid across PR commits.
# `.dockerignore` excludes `.git/`, so `crates/celo-reth/build.rs` cannot run
# `git rev-parse HEAD` in-container. CI passes the SHA via `--build-arg`; defaults
# to `unknown` so local `docker build` without the arg still succeeds.
ARG CELO_KONA_GIT_SHA=unknown
ENV CELO_KONA_GIT_SHA=$CELO_KONA_GIT_SHA
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --profile $BUILD_PROFILE --locked -p celo-reth --bin celo-reth \
    && cp /app/target/$BUILD_PROFILE/celo-reth /app/celo-reth

FROM ubuntu:24.04 AS runtime
RUN apt-get update && apt-get install -y ca-certificates wget && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/celo-reth /usr/local/bin/celo-reth
# Logs under $HOME/.cache/reth/logs. Sticky HOME so any runtime uid (e.g. K8s runAsUser) can write.
RUN mkdir -p /reth && chmod 1777 /reth
ENV HOME=/reth
WORKDIR /reth
EXPOSE 30303 30303/udp 9001 8545 8546
ENTRYPOINT ["/usr/local/bin/celo-reth"]
