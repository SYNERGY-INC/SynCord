# SynCord server container (for CasaOS, Docker or Podman).
# Build:  docker build -t syncord-server .
# Run:    docker run -d -p 7878:7878/tcp -p 7878:7878/udp -v ./data:/data syncord-server

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p syncord-server \
 && cp target/release/syncord-server /syncord-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /syncord-server /usr/local/bin/syncord-server
# Settings, database, uploads and log all live here. Mount it as a volume.
VOLUME /data
WORKDIR /data
EXPOSE 7878/tcp 7878/udp
CMD ["syncord-server", "/data/syncord-server.toml"]
