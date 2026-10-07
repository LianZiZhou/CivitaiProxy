FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/civitai-proxy /usr/local/bin/civitai-proxy
WORKDIR /data
ENV CP_LISTEN=0.0.0.0:8787 \
    CP_CONFIG=/data/config.toml
EXPOSE 8787
ENTRYPOINT ["civitai-proxy"]
CMD ["serve"]
