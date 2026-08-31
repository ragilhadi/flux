# Stage 1: Build the Rust application
FROM rust:slim AS builder

# Set working directory
WORKDIR /build

# Copy all source files
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# Build the application
RUN cargo build --release

# Stage 2: Create minimal runtime image
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from builder
COPY --from=builder /build/target/release/flux /usr/local/bin/flux

# Make binary executable
RUN chmod +x /usr/local/bin/flux

# Run as a non-root user. The default config and output paths
# (/app/config.yaml, /app/data, /app/results) are typically bind-mounted from
# the host, so these directories are made writable by any UID rather than
# just this one: a bind mount keeps the host directory's own permissions, and
# the container's UID will rarely match the host user's.
RUN useradd --no-create-home --uid 10001 flux && \
    mkdir -p /app/data /app/results && \
    chmod 777 /app/data /app/results
USER flux
WORKDIR /app

# Run the application
ENTRYPOINT ["/usr/local/bin/flux"]