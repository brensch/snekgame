# syntax=docker/dockerfile:1
# Cross-builds the server for the schlift.com VM (Oracle Cloud, ARM64): the web
# frontend with trunk, then the server (which embeds web/dist) as a static
# aarch64 musl binary. zig is the cross C compiler/linker for the C inside
# rusqlite (bundled SQLite) and ring. `make build` exports it to deploy/out/.
FROM ghcr.io/rust-cross/cargo-zigbuild@sha256:d8313491ec5798de0633fdc1c5753761bff79967bea69076020dc78121b2cca8 AS build
ARG TRUNK_VERSION=0.21.14
WORKDIR /src
# The toolchain the repo pins (rust-toolchain.toml), with the targets.
COPY rust-toolchain.toml .
RUN rustup toolchain install \
 && rustup target add aarch64-unknown-linux-musl wasm32-unknown-unknown \
 && curl -fsSL https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-x86_64-unknown-linux-musl.tar.gz \
    | tar -xz -C /usr/local/bin trunk
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/root/.cache/trunk \
    cd web && trunk build --release && cd .. \
 && cargo zigbuild --release --locked -p snek-server --target aarch64-unknown-linux-musl \
 && cp target/aarch64-unknown-linux-musl/release/snek-server /snek-server

FROM scratch AS export
COPY --from=build /snek-server /
