FROM rust:1.88-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build

COPY . .

RUN cargo build --release --package mcp-index --bin mcp-index

FROM alpine:3.19
RUN apk add --no-cache wget
COPY --from=builder /build/target/release/mcp-index /usr/local/bin/mcp-index
ENTRYPOINT ["mcp-index"]
