FROM rust as build

WORKDIR /usr/src
RUN USER=root cargo new discord-twitch-notif
WORKDIR /usr/src/discord-twitch-notif

# Caches build dependencies by writing placeholder lib and main files.
COPY Cargo.toml Cargo.lock ./

RUN cargo build --release --locked

COPY src ./src

# To trigger cargo to recompile
RUN touch src/main.rs

RUN cargo install --path . --offline

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y openssl ca-certificates

COPY --from=build /usr/local/cargo/bin/discord-twitch-notif /usr/local/bin/discord-twitch-notif

EXPOSE 8000
CMD ["discord-twitch-notif", "webhook"]
