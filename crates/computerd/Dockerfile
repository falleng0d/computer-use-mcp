# syntax=docker/dockerfile:1
FROM rust:1.99-slim-trixie AS build
WORKDIR /src
COPY . .
ARG COMPUTER_USE_MCP_VERSION=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p computerd \
    && cp target/release/computerd /usr/local/bin/computerd

FROM debian:trixie-slim
RUN useradd --create-home --uid 1000 computer
COPY --from=build /usr/local/bin/computerd /usr/local/bin/computerd
USER computer
WORKDIR /home/computer
EXPOSE 7070
CMD ["computerd"]
