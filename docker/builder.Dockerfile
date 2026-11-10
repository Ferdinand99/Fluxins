# Shared builder image for Inlet. Built and run by scripts/docker-build.sh on a Docker
# host; nothing in here is a secret and no source code is baked in.
#
# The image is tagged inlet-builder:<hash of docker/*>, so different versions of
# this file never overwrite each other. Edit this file (or builder-entry.sh) and the
# next scripts/docker-build.sh run builds a new tag next to the old one.
#
# Contents:
#   - the Rust toolchain from rust-toolchain.toml (a bump there needs the FROM line
#     and the inlet.rust label below changed too; scripts/docker-build.sh checks that)
#   - the system libraries gpui needs on Linux (X11 and Wayland)
#   - clang 21 from apt.llvm.org (the LiveKit voice crate builds webrtc-sys with it)
#   - cargo-deny and cargo-about (the same versions CI uses)
#   - Xvfb, a software Vulkan driver (lavapipe), ImageMagick, xdotool and an emoji font
#     for UI checks

# rust:1.99.1-trixie, pinned by digest of the multi-arch index.
FROM rust:1.99.1-trixie@sha256:8a4e90c7946b74626488b1446f712c0fe0ed1a596cbd3d871684bc9c5433ae07

LABEL owner=inlet \
      inlet.rust=1.99.1

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TERM_COLOR=never \
    RUSTUP_AUTO_INSTALL=0

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl gnupg git time procps \
      build-essential cmake lld pkg-config \
      libasound2-dev libfontconfig-dev libfreetype-dev libglib2.0-dev libssl-dev \
      libvulkan1 libvulkan-dev mesa-vulkan-drivers libegl1 libgl1-mesa-dri \
      libwayland-dev libx11-xcb-dev libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev \
      libzstd-dev \
      xvfb x11-utils imagemagick xdotool fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*

# clang >= 21 for the voice crate (Debian trixie ships 19).
RUN curl -fsSL https://apt.llvm.org/llvm-snapshot.gpg.key | gpg --dearmor -o /usr/share/keyrings/llvm.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/llvm.gpg] http://apt.llvm.org/trixie/ llvm-toolchain-trixie-21 main" \
       > /etc/apt/sources.list.d/llvm.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends clang-21 lld-21 \
    && rm -rf /var/lib/apt/lists/*
ENV CC=clang-21 \
    CXX=clang++-21

RUN rustup component add clippy rustfmt

# Same tool versions as CI (cargo-about 0.9.3 with the cli feature).
RUN CARGO_BUILD_JOBS=4 cargo install --locked --root /usr/local/cargo cargo-about --version 0.9.3 --features cli \
    && CARGO_BUILD_JOBS=4 cargo install --locked --root /usr/local/cargo cargo-deny --version 0.20.2 \
    && rm -rf /usr/local/cargo/registry /usr/local/cargo/git

# Emoji font for screenshots (without it emoji render as empty boxes under Xvfb).
RUN apt-get update && apt-get install -y --no-install-recommends fonts-noto-color-emoji \
    && rm -rf /var/lib/apt/lists/*

COPY builder-entry.sh /usr/local/bin/inlet-run
RUN sed -i 's/\r$//' /usr/local/bin/inlet-run && chmod 0755 /usr/local/bin/inlet-run

WORKDIR /src
ENTRYPOINT ["/usr/local/bin/inlet-run"]
