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
RUN cargo build --release --target x86_64-unknown-linux-musl

# ---- runtime stage ----
FROM gcr.io/distroless/static-debian12
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/1router /1router
ENV ROUTER_LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/1router"]
