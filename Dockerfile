# Celerity server container (for CasaOS, Docker or Podman).
# Build:  docker build -t celerity-server .
# Run:    docker run -d -p 7878:7878/tcp -p 7878:7878/udp -v ./data:/data celerity-server

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p celerity-server \
 && cp target/release/celerity-server /celerity-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /celerity-server /usr/local/bin/celerity-server
# Settings, database, uploads and log all live here. Mount it as a volume.
VOLUME /data
WORKDIR /data
EXPOSE 7878/tcp 7878/udp
CMD ["celerity-server", "/data/celerity-server.toml"]
