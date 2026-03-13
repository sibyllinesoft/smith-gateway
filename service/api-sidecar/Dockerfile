FROM rust:1.89-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build

COPY . .

RUN cargo build --release --package api-sidecar --bin api-sidecar

FROM alpine:3.19
RUN apk add --no-cache wget
COPY --from=builder /build/target/release/api-sidecar /usr/local/bin/api-sidecar
ENTRYPOINT ["api-sidecar"]
