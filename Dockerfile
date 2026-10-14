# Same release as `rust-toolchain.toml`. rustup still fetches that file's `1.95`
# channel once, since the image names its toolchain `1.95.0`.
FROM rust:1.95.0-trixie AS builder

RUN apt-get update && apt-get install -y \
	build-essential \
	libclang-dev \
	libssl-dev \
	&& rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/celo-kona

COPY . .

# Only the binaries this image ships; celo-reth has its own image.
RUN cargo build --release --locked -p celo-host -p celo-client -p execution-verifier

FROM debian:trixie-slim

# The binaries link libssl dynamically (alloy-transport-http -> hyper-tls -> native-tls).
RUN apt-get update && apt-get install -y --no-install-recommends \
	ca-certificates \
	libssl3t64 \
	&& rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/celo-kona/target/release/execution-verifier /usr/local/bin/execution-verifier
COPY --from=builder /usr/src/celo-kona/target/release/celo-client /usr/local/bin/celo-client
COPY --from=builder /usr/src/celo-kona/target/release/celo-host /usr/local/bin/celo-host

CMD ["celo-client"]
