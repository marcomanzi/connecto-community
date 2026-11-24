FROM rust:1.100-slim-bookworm AS linux
RUN apt-get update && apt-get install -y --no-install-recommends \
    git clang pkg-config libwayland-dev libxkbcommon-dev libx11-dev \
    libxcursor-dev libxrandr-dev libxi-dev libgl-dev libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src

FROM linux AS windows
RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc-mingw-w64-x86-64-posix g++-mingw-w64-x86-64-posix zip \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-pc-windows-gnu
ENV CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
WORKDIR /src
