# Multi-stage build for tttt terminal multiplexer
# Stage 1: Build tttt
FROM rust:slim AS builder

# Install build dependencies
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the entire workspace
COPY . .

# Build the release binary
RUN cargo build --release

# Stage 2: Create minimal runtime image
FROM debian:sid-slim

# Install runtime dependencies and Node.js for npm
RUN apt-get update && \
    apt-get install -y ca-certificates libssl3 curl gnupg && \
    rm -rf /var/lib/apt/lists/*

# Install Node.js and npm for Claude Code
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - && \
    apt-get update && \
    apt-get install -y nodejs && \
    rm -rf /var/lib/apt/lists/*

# Install Claude Code CLI via npm
RUN npm install -g @anthropic-ai/claude-code

# Copy apchat from ghcr.io
COPY --from=ghcr.io/ayourtch/apchat:latest /usr/local/bin/apchat /usr/local/bin/apchat

# Copy tttt binary from builder
COPY --from=builder /app/target/release/tttt /usr/local/bin/tttt

# Create a non-root user
RUN useradd -m -u 1000 tttt

# Create workspace directory and set ownership
RUN mkdir -p /workspace && chown tttt:tttt /workspace

# Copy the entrypoint wrapper
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Set the user
USER tttt

# Set working directory to /workspace for user projects
WORKDIR /workspace

# The entrypoint is the wrapper script
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

# Default to showing help
CMD ["--help"]