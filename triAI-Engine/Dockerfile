# Build the engine in a reproducible Rust image. llama-server and GGUF files
# are deliberately runtime mounts: model payloads must never be baked in.
FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd --system --create-home --uid 10001 triai
WORKDIR /app
COPY --from=build /src/target/release/tri-ai-engine /usr/local/bin/tri-ai-engine
COPY config/example.toml /etc/tri-ai-engine/config.toml
RUN mkdir -p /var/lib/tri-ai-engine/stage /var/log/tri-ai-engine && \
    chown -R triai:triai /var/lib/tri-ai-engine /var/log/tri-ai-engine
USER triai
EXPOSE 8900
ENV TRI_AI_CONFIG=/etc/tri-ai-engine/config.toml
ENTRYPOINT ["tri-ai-engine", "--no-network"]
