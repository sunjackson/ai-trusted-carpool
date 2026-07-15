FROM rust:1-bookworm

ARG NODE_VERSION=20.19.4
ARG TARGETARCH

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        file \
        libayatana-appindicator3-dev \
        libgtk-3-dev \
        librsvg2-dev \
        libssl-dev \
        libwebkit2gtk-4.1-dev \
        patchelf \
        pkg-config \
        xdg-utils \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*

RUN case "${TARGETARCH}" in \
      amd64) node_arch="x64" ;; \
      arm64) node_arch="arm64" ;; \
      *) echo "Unsupported Docker architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
    && mkdir -p /opt/node \
    && tar -xJf "node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" --strip-components=1 -C /opt/node \
    && rm "node-v${NODE_VERSION}-linux-${node_arch}.tar.xz"

RUN rustup component add rustfmt clippy

ENV PATH="/opt/node/bin:/usr/local/cargo/bin:${PATH}"
WORKDIR /workspace
