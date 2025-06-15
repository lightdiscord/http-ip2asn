# TODO: Make smaller docker image

FROM rust:1.87-alpine AS builder

WORKDIR /app

RUN apk update && apk add --no-cache musl-dev

# TOOD: Remove nightly when btree_cursors is stable (https://github.com/rust-lang/rust/issues/107540)
RUN rustup toolchain install nightly
RUN rustup target add x86_64-unknown-linux-musl

COPY . .
RUN cargo +nightly install --target x86_64-unknown-linux-musl --path .

FROM scratch

COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/http-ip2asn /http-ip2asn

ENTRYPOINT ["/http-ip2asn"]