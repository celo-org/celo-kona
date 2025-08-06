FROM rust:1.88.0-bookworm AS builder
WORKDIR /usr/src/celo-kona

RUN apt-get update && apt-get install -y \
	build-essential \
	gcc \
	g++ \
	ccache \
	cmake \
	libssh-dev \
	libclang-14-dev \
  pkg-config \
	libssl-dev \
	&& rm -rf /var/lib/apt/lists/*

RUN cargo install just

COPY . .

RUN just build-native --release

FROM debian:bookworm-slim

# runtime dependencies that are dynamically linked
RUN apt-get update && apt-get install -y --no-install-recommends \
    libc6 \
    libgcc-s1 \
    libstdc++6 \
    libssl3 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/celo-kona/target/release/execution-verifier /usr/local/bin/execution-verifier
COPY --from=builder /usr/src/celo-kona/target/release/celo-client /usr/local/bin/celo-client
COPY --from=builder /usr/src/celo-kona/target/release/celo-host /usr/local/bin/celo-host

CMD ["celo-client"]
