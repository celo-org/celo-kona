FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y libclang-dev pkg-config && rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json --bin celo-reth

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
ARG BUILD_PROFILE=release
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo chef cook --profile $BUILD_PROFILE --recipe-path recipe.json --bin celo-reth
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --profile $BUILD_PROFILE --locked -p celo-reth --bin celo-reth \
    && cp /app/target/$BUILD_PROFILE/celo-reth /app/celo-reth

FROM ubuntu:24.04 AS runtime
RUN apt-get update && apt-get install -y ca-certificates wget && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/celo-reth /usr/local/bin/celo-reth
# File logs use $HOME/.cache/reth/logs; an unset HOME yields /.cache and permission errors.
USER ubuntu
ENV HOME=/home/ubuntu
WORKDIR /home/ubuntu
EXPOSE 30303 30303/udp 9001 8545 8546
ENTRYPOINT ["/usr/local/bin/celo-reth"]
