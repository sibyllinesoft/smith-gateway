FROM rust:1.88-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build

COPY . .

RUN cargo build --release --package catalog --bin catalog

FROM alpine:3.19
RUN apk add --no-cache wget
COPY --from=builder /build/target/release/catalog /usr/local/bin/catalog
ENTRYPOINT ["catalog"]
