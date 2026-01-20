FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
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

# Install nushell
RUN NUSHELL_VERSION=$(curl -s https://api.github.com/repos/nushell/nushell/releases/latest | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/') && \
    curl -fsSL https://github.com/nushell/nushell/releases/download/${NUSHELL_VERSION}/nu-${NUSHELL_VERSION#v}-x86_64-unknown-linux-gnu.tar.gz | tar -xz && \
    mv nu-*-x86_64-unknown-linux-gnu/nu /usr/local/bin/nu && \
    rm -rf nu-*-x86_64-unknown-linux-gnu

# Use Ubuntu as the release image
FROM ubuntu AS runtime
WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get -y upgrade && apt-get install -y ca-certificates && update-ca-certificates

# Copy rpc-tester over from the build stage
COPY --from=builder /app/rpc-tester-cli /usr/local/bin
COPY --from=builder /usr/local/bin/nu /usr/local/bin/nu

EXPOSE 9119
ENTRYPOINT ["/usr/local/bin/rpc-tester-cli"]