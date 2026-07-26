# ---- build stage ----
FROM rust:1.90-alpine AS builder
ARG TARGETARCH
# rustls (not openssl) handles TLS and sqlx's sqlite feature bundles/statically
# links libsqlite3, so no OpenSSL dependency is actually needed at build or
# runtime - musl-dev + sqlite-static is sufficient.
RUN apk add --no-cache musl-dev sqlite-static pkgconfig
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY migrations ./migrations
COPY src ./src
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) rust_target="x86_64-unknown-linux-musl" ;; \
      arm64) rust_target="aarch64-unknown-linux-musl" ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    rustup target add "$rust_target"; \
    cargo build --release --target "$rust_target"; \
    mkdir -p /out /out-data; \
    cp "target/$rust_target/release/1router" /out/1router

# ---- runtime stage ----
FROM gcr.io/distroless/static-debian12
COPY --from=builder /out/1router /1router
COPY --chown=nonroot:nonroot --from=builder /out-data /data
ENV ROUTER_LISTEN_ADDR=0.0.0.0:8080
ENV ROUTER_SQLITE_PATH=/data/1router.db
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 CMD ["/1router", "healthcheck"]
USER nonroot:nonroot
ENTRYPOINT ["/1router"]
