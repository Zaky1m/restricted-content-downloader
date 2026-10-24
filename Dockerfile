# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# Build the dependencies first so source edits do not recompile them.
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/deps/rcd* target/release/rcd*
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /app rcd
WORKDIR /app
COPY --from=build /src/target/release/rcd /usr/local/bin/rcd
COPY locales ./locales
RUN mkdir -p data && chown -R rcd:rcd /app
USER rcd
ENV DATA_DIR=/app/data
CMD ["rcd", "serve"]
