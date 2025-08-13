FROM rust:1.89.0-trixie AS builder

RUN apt-get update && apt-get install -y \
	build-essential \
	ccache \
	libclang-dev \
	libssl-dev \
	&& rm -rf /var/lib/apt/lists/*

RUN cargo install just

WORKDIR /usr/src/celo-kona

COPY . .

RUN just build-native --release

FROM rust:1.89.0-trixie

COPY --from=builder /usr/src/celo-kona/target/release/execution-verifier /usr/local/bin/execution-verifier
COPY --from=builder /usr/src/celo-kona/target/release/celo-client /usr/local/bin/celo-client
COPY --from=builder /usr/src/celo-kona/target/release/celo-host /usr/local/bin/celo-host

CMD ["celo-client"]
