# Stage 1: build a fully static musl binary. The alpine rust image targets
# *-unknown-linux-musl by default, on both amd64 and arm64.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src

# Dependency layer: build a stub crate against the real manifest + lockfile so
# the (slow) dependency compilation is cached until Cargo.toml/Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/zzzboard target/release/deps/zzzboard-*

COPY src ./src
RUN cargo build --release --locked

# Stage 2: distroless static, non-root. No shell, no libc, one binary.
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/zzzboard /zzzboard
ENV ZZZ_DATA_DIR=/data ZZZ_PORT=8080
EXPOSE 8080
VOLUME ["/data"]
USER nonroot:nonroot
ENTRYPOINT ["/zzzboard"]
