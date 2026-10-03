# syntax=docker/dockerfile:1
FROM rust:1.99-slim-trixie AS build
WORKDIR /src
COPY . .
ARG COMPUTER_USE_MCP_VERSION=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p computerd \
    && cp target/release/computerd /usr/local/bin/computerd

FROM ghcr.io/astral-sh/uv:0.12.23 AS uv

FROM debian:trixie-slim AS aws
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl gnupg unzip
COPY crates/computerd/image/awscli.pub /tmp/awscli.pub
RUN gpg --import /tmp/awscli.pub \
    && curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-$(uname -m).zip" -o /tmp/aws.zip \
    && curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-$(uname -m).zip.sig" -o /tmp/aws.zip.sig \
    && gpg --status-fd 1 --verify /tmp/aws.zip.sig /tmp/aws.zip | grep -q 'VALIDSIG .* FB5DB77FD5C118B80511ADA8A6310ACC4672475C' \
    && unzip -q /tmp/aws.zip -d /tmp \
    && /tmp/aws/install --bin-dir /usr/local/bin --install-dir /usr/local/aws-cli
FROM debian:trixie-slim AS novnc
ARG NOVNC_VERSION=1.7.0
ARG NOVNC_SHA256=b1003a11b6e6e8d8f7f5e5586daae7f8ca651d8aee0aa155ff9ac841c48f52c6
RUN apt-get update     && apt-get install -y --no-install-recommends ca-certificates curl
RUN curl -fsSL "https://github.com/novnc/noVNC/archive/refs/tags/v${NOVNC_VERSION}.tar.gz" -o /tmp/novnc.tar.gz     && echo "${NOVNC_SHA256}  /tmp/novnc.tar.gz" | sha256sum -c -     && mkdir -p /out /tmp/src     && tar -xzf /tmp/novnc.tar.gz -C /tmp/src --strip-components=1     && cp -r /tmp/src/core /tmp/src/vendor /tmp/src/LICENSE.txt /out/

FROM debian:trixie-slim
ARG NODE_MAJOR=24
# apt lists stay in the image so `sudo apt-get install` works without an update first.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        locales tzdata tigervnc-standalone-server fluxbox xterm xkb-data x11-utils imagemagick hsetroot \
        fonts-dejavu-core fonts-noto-core fonts-noto-cjk fonts-noto-color-emoji \
        sudo ca-certificates curl git openssh-client jq ripgrep xclip xsel \
        python3 python3-pip python3-venv ruby build-essential fish \
        procps less file unzip zip xz-utils nano groff-base \
    && install -d -m 0755 /etc/apt/keyrings \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli.gpg \
    && curl -fsSL https://deb.nodesource.com/gpgkey/nodesource-repo.gpg.key -o /etc/apt/keyrings/nodesource.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
    && echo "deb [signed-by=/etc/apt/keyrings/nodesource.asc] https://deb.nodesource.com/node_${NODE_MAJOR}.x nodistro main" > /etc/apt/sources.list.d/nodesource.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends gh nodejs \
    && sed -i 's/^# *en_US.UTF-8/en_US.UTF-8/' /etc/locale.gen \
    && locale-gen
ENV LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8
COPY --from=uv /uv /uvx /usr/local/bin/
COPY --from=aws /usr/local/aws-cli /usr/local/aws-cli
RUN ln -s /usr/local/aws-cli/v2/current/bin/aws /usr/local/bin/aws \
    && ln -s /usr/local/aws-cli/v2/current/bin/aws_completer /usr/local/bin/aws_completer
RUN useradd --create-home --uid 1000 --shell /bin/bash computer \
    && echo 'computer ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/computer \
    && chmod 0440 /etc/sudoers.d/computer
COPY --chmod=0644 crates/computerd/image/profile.sh /etc/profile.d/computer.sh
COPY --chmod=0644 crates/computerd/image/XTerm /etc/X11/app-defaults/XTerm
COPY --chmod=0755 crates/computerd/image/check-tools.sh /usr/local/bin/check-tools
COPY --from=build /usr/local/bin/computerd /usr/local/bin/computerd
COPY --from=novnc /out /opt/novnc
COPY crates/computerd/image/fluxbox-init /etc/computerd/fluxbox-init
ENV NPM_CONFIG_PREFIX=/home/computer/.local \
    PIP_BREAK_SYSTEM_PACKAGES=1 \
    PATH=/home/computer/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
USER computer
WORKDIR /home/computer
EXPOSE 7070 20900-20916
CMD ["computerd"]
