FROM rust:1-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:trixie-slim
LABEL org.opencontainers.image.source=https://github.com/LianZiZhou/CivitaiProxy \
      org.opencontainers.image.description="Reverse proxy for Civitai, Hugging Face, GitHub, GHCR and Docker Hub"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/civitai-proxy /usr/local/bin/civitai-proxy
WORKDIR /data
ENV CP_LISTEN=0.0.0.0:8787 \
    CP_CONFIG=/data/config.toml
EXPOSE 8787
ENTRYPOINT ["civitai-proxy"]
CMD ["serve"]
