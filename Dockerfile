# Multi-stage build for Flux load testing tool
# Stage 1: Build the Rust binary
FROM rust:slim AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /build

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to cache dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Copy source code
COPY src ./src

# Build the actual application
RUN touch src/main.rs && \
    cargo build --release

# Stage 2: Create the runtime image
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create app directory and required subdirectories
WORKDIR /app
RUN mkdir -p /app/data /app/results

# Copy the binary from builder
COPY --from=builder /build/target/release/flux /app/flux

# Set permissions
RUN chmod +x /app/flux

# Create a non-root user
RUN useradd -m -u 1000 flux && \
    chown -R flux:flux /app

USER flux

# Set environment variables
ENV RUST_LOG=info

# The container expects these volumes to be mounted:
# - /app/config.yaml (configuration file)
# - /app/data (directory for multipart files)
# - /app/results (directory for output reports)

# Default command
CMD ["/app/flux"]
