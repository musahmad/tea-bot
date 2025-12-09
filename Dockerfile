# Build stage
FROM rust:latest AS builder

WORKDIR /usr/src/app

# Copy manifests
COPY . .

# Build dependencies
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim AS runtime

# Install necessary runtime dependencies
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the built binary from builder
COPY --from=builder /usr/src/app/target/release/tea-bot .

# Copy necessary runtime files
COPY abi.json .

# Expose the port the app runs on
EXPOSE 6969

# Run the binary with environment variables
CMD ["./tea-bot"]