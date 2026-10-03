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
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        locales tzdata tigervnc-standalone-server fluxbox xkb-data fonts-dejavu-core \
    && sed -i 's/^# *en_US.UTF-8/en_US.UTF-8/' /etc/locale.gen \
    && locale-gen \
    && rm -rf /var/lib/apt/lists/*
ENV LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8
RUN useradd --create-home --uid 1000 computer
COPY --from=build /usr/local/bin/computerd /usr/local/bin/computerd
COPY crates/computerd/image/fluxbox-init /etc/computerd/fluxbox-init
USER computer
WORKDIR /home/computer
EXPOSE 7070
CMD ["computerd"]
