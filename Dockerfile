FROM lukemathwalker/cargo-chef:latest-rust-1.86.0 AS chef
WORKDIR /app

LABEL org.opencontainers.image.source=https://github.com/tempoxyz/rpc-tester

# Builds a cargo-chef plan
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json

# Install system dependencies
RUN apt-get update && apt-get -y upgrade && apt-get install -y libclang-dev pkg-config

# Builds dependencies
RUN cargo chef cook --recipe-path recipe.json

# Copy source
COPY . .

# Build application
RUN cargo build --locked --release

# ARG is not resolved in COPY so we have to hack around it by copying the
# binary to a temporary location
RUN cp /app/target/release/rpc-tester-cli /app/rpc-tester-cli

# Install nushell (pinned version with checksum verification)
ENV NUSHELL_VERSION=0.103.0
ENV NUSHELL_SHA256=8d765a31611b3ae8fb63582a53b39111c11e2a3a6be3c76afb2c0a4bb38eebee
RUN curl -fsSL https://github.com/nushell/nushell/releases/download/${NUSHELL_VERSION}/nu-${NUSHELL_VERSION}-x86_64-unknown-linux-gnu.tar.gz -o /tmp/nu.tar.gz && \
    echo "${NUSHELL_SHA256}  /tmp/nu.tar.gz" | sha256sum -c - && \
    tar -xzf /tmp/nu.tar.gz -C /tmp && \
    mv /tmp/nu-${NUSHELL_VERSION}-x86_64-unknown-linux-gnu/nu /usr/local/bin/nu && \
    rm -rf /tmp/nu*

# Use Ubuntu as the release image
FROM ubuntu:24.04 AS runtime
WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get -y upgrade && apt-get install -y ca-certificates && update-ca-certificates

# Copy rpc-tester over from the build stage
COPY --from=builder /app/rpc-tester-cli /usr/local/bin
COPY --from=builder /usr/local/bin/nu /usr/local/bin/nu

EXPOSE 9119
ENTRYPOINT ["/usr/local/bin/rpc-tester-cli"]