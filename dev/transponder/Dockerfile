FROM rust:1.90-bookworm AS builder

ARG TRANSPONDER_REF=a224e674a60df20612dfd3bc31fd51cff2c9759b

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

RUN curl -fsSL "https://github.com/marmot-protocol/transponder/archive/${TRANSPONDER_REF}.tar.gz" \
    | tar -xz --strip-components=1

RUN cargo build --locked --release --bin transponder

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/transponder /usr/local/bin/transponder

USER 65532:65532

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/transponder"]
CMD ["--config", "/etc/transponder/config.toml"]
