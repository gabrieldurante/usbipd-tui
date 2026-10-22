FROM rust:1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc-mingw-w64-x86-64-posix \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-pc-windows-gnu

WORKDIR /build
COPY . .

RUN cargo build --release
