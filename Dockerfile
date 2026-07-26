# ---- build stage ----
FROM rust:1.90-alpine AS builder
# rustls (not openssl) handles TLS and sqlx's sqlite feature bundles/statically
# links libsqlite3, so no OpenSSL dependency is actually needed at build or
# runtime - musl-dev + sqlite-static is sufficient.
RUN apk add --no-cache musl-dev sqlite-static pkgconfig
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY migrations ./migrations
COPY src ./src
# TARGETARCH is supplied by buildx (docker/amd64 -> "amd64", docker/arm64 ->
# "arm64"). Map it to the matching Rust musl triple, then stage the binary at a
# fixed, arch-independent path so the runtime-stage COPY needs no arch in it
# (COPY cannot run shell, so it cannot do the mapping itself).
ARG TARGETARCH
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) RUST_TARGET=x86_64-unknown-linux-musl ;; \
      arm64) RUST_TARGET=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    rustup target add "$RUST_TARGET"; \
    cargo build --release --target "$RUST_TARGET"; \
    cp "target/${RUST_TARGET}/release/1router" /app/1router

# ---- runtime stage ----
FROM gcr.io/distroless/static-debian12
LABEL org.opencontainers.image.source="https://github.com/ducphamhoang/1router"
COPY --from=builder /app/1router /1router
ENV ROUTER_LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/1router"]
