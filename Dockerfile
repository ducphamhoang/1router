# ---- build stage ----
FROM rust:1.90-alpine AS builder
RUN apk add --no-cache musl-dev sqlite-static openssl-dev pkgconfig
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY src ./src
RUN cargo build --release --target x86_64-unknown-linux-musl

# ---- runtime stage ----
FROM gcr.io/distroless/static-debian12
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/1router /1router
ENV ROUTER_LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["/1router"]
