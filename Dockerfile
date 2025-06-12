# TODO: Make smaller docker image

FROM rustlang/rust:nightly AS builder

WORKDIR /app

COPY . .
RUN cargo install --path .

FROM ubuntu

RUN apt update && apt install -y ca-certificates
COPY --from=builder /app/target/release/http-ip2asn /http-ip2asn

ENTRYPOINT ["/http-ip2asn"]