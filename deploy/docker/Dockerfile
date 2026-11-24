# syntax=docker/dockerfile:1
# Both build stages run on the build platform; the server is cross-compiled
# with zig as the musl linker instead of emulating cargo under QEMU.

ARG RUST_VERSION=1.100
ARG NODE_VERSION=22
ARG ZIG_VERSION=0.17.1
ARG ZIGBUILD_VERSION=0.24.1

FROM --platform=$BUILDPLATFORM node:${NODE_VERSION}-trixie-slim AS web
WORKDIR /src
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN npm install --global "$(node -p 'require("./package.json").packageManager')"
COPY packages packages
COPY web web
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile --filter '@tagmail/web...'
RUN pnpm --filter @tagmail/web run build

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-slim-trixie AS server
ARG BUILDARCH
ARG TARGETARCH
ARG ZIG_VERSION
ARG ZIGBUILD_VERSION
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl unzip xz-utils \
    && rm -rf /var/lib/apt/lists/*
# zig from its PyPI wheel: ziglang.org asks CI not to download from it.
RUN set -eu; \
    case "$BUILDARCH" in \
      amd64) arch=x86_64; \
        zig_url=https://files.pythonhosted.org/packages/59/e3/2a34676370a11c36b7cd04ffb2439c1089c0530cf4b01408c77cf7f8d9c3/ziglang-${ZIG_VERSION}-py3-none-manylinux_2_12_x86_64.manylinux2010_x86_64.musllinux_1_1_x86_64.whl; \
        zig_sha=ea6cc5d87b033cf0aedfbc8f9549a908cae6c50ac31f1698188fa48a74524b54; \
        zigbuild_sha=32581c8e13f27a807bb244aaaac870522f5b51263c766cf77b1a99771ce9fa5c ;; \
      arm64) arch=aarch64; \
        zig_url=https://files.pythonhosted.org/packages/91/72/bb7c4f098ba3e535fa2ebefd9f26327ba63fb0672562959255cb5dd37aff/ziglang-${ZIG_VERSION}-py3-none-manylinux_2_17_aarch64.manylinux2014_aarch64.musllinux_1_1_aarch64.whl; \
        zig_sha=b7c5f02fccd40f89b999ee16a1f5ddced56e6182370f8affe494bcf8a28cf07e; \
        zigbuild_sha=70bb4c9cc71a6d16211b9079996e3cd588671053312adeff4d043499ed887601 ;; \
      *) echo "unsupported build platform $BUILDARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/zig.whl "$zig_url"; \
    echo "$zig_sha  /tmp/zig.whl" | sha256sum -c -; \
    unzip -q /tmp/zig.whl 'ziglang/*' -d /opt; \
    chmod +x /opt/ziglang/zig; \
    ln -s /opt/ziglang/zig /usr/local/bin/zig; \
    curl -fsSLo /tmp/zigbuild.tar.xz "https://github.com/rust-cross/cargo-zigbuild/releases/download/v${ZIGBUILD_VERSION}/cargo-zigbuild-${arch}-unknown-linux-gnu.tar.xz"; \
    echo "$zigbuild_sha  /tmp/zigbuild.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/zigbuild.tar.xz -C /usr/local/bin --strip-components=1 --wildcards '*/cargo-zigbuild'; \
    rm /tmp/zig.whl /tmp/zigbuild.tar.xz; \
    zig version
WORKDIR /src
COPY rust-toolchain.toml ./
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl ;; \
      arm64) echo aarch64-unknown-linux-musl ;; \
      *) echo "unsupported target platform $TARGETARCH" >&2; exit 1 ;; \
    esac > /target \
    && rustup toolchain install \
    && rustup target add "$(cat /target)"
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
COPY desktop/src-tauri desktop/src-tauri
COPY xtask xtask
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry-$TARGETARCH \
    --mount=type=cache,target=/src/target,id=tagmail-target-$TARGETARCH \
    cargo zigbuild --release --locked --target "$(cat /target)" \
      -p tagmail-api-server --bin tagmail-server \
    && mkdir -p /out/data \
    && cp "target/$(cat /target)/release/tagmail-server" /out/

# Server binary and web app only; release.yml packs these into tarballs.
FROM scratch AS dist
COPY --from=server /out/tagmail-server /tagmail-server
COPY --from=web /src/web/build /web

FROM gcr.io/distroless/static-debian13:nonroot
COPY --from=server --chown=nonroot:nonroot /out/data /data
COPY --from=dist / /app/
ENV TAGMAIL_MASTER_KEY_FILE=/run/secrets/tagmail_master_key
EXPOSE 7878
VOLUME /data
USER nonroot
ENTRYPOINT ["/app/tagmail-server", "--data-dir", "/data", "--listen", "0.0.0.0:7878"]
