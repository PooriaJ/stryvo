# Multi-stage build for stryvo proxy application

# Build stage
FROM rust:1.86 AS builder
WORKDIR /app
# Install build dependencies including perl and required modules
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    cmake \
    perl \
    perl-modules \
    && rm -rf /var/lib/apt/lists/*
# Copy the source code directly
COPY Cargo.toml Cargo.lock ./
COPY source ./source
# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim
WORKDIR /app
# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Copy the binary from the builder stage
COPY --from=builder /app/target/release/stryvo /usr/local/bin/stryvo
# Copy configuration files
COPY source/stryvo/assets /app/assets
# Create volume for logs
VOLUME ["/app/logs"]
# Expose default ports
EXPOSE 80 443
# Set the entrypoint
ENTRYPOINT ["stryvo"]
# Default command
CMD ["--config-toml", "/app/assets/example-config.toml"]