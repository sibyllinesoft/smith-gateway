FROM rust:1.89-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build

COPY . .

RUN cargo build --release --package pg-auth-gateway --bin pg-auth-gateway

FROM alpine:3.19
RUN apk add --no-cache wget
COPY --from=builder /build/target/release/pg-auth-gateway /usr/local/bin/pg-auth-gateway
ENTRYPOINT ["pg-auth-gateway"]
