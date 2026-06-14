# Stage 1: Build
FROM rust:1.85-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs 2>/dev/null; \
    cargo build --release 2>/dev/null || true; \
    rm -rf src

COPY src/ src/
COPY web/ web/

RUN cargo build --release && \
    strip target/release/ssh-remote

# Stage 2: Runtime
FROM alpine:3.21

RUN apk add --no-cache ca-certificates tzdata

COPY --from=builder /app/target/release/ssh-remote /usr/local/bin/ssh-remote

EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/ssh-remote"]
