# syntax=docker/dockerfile:1
# Апплайенс BottleForApps: Alpine rootfs + bottle-agent + сетевые пакеты.
# Целевая платформа — x86_64 (WSL2); сборка всегда идёт под linux/amd64.

FROM --platform=linux/amd64 rust:1-alpine AS builder
RUN rustup target add x86_64-unknown-linux-musl
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release -p agent --target x86_64-unknown-linux-musl

# alpine:3.22, а не 3.20: пакет wireguard-go (жёсткая runtime-зависимость
# агента — ip netns exec <ns> wireguard-go) отсутствует в репозиториях 3.20.
FROM --platform=linux/amd64 alpine:3.22
RUN apk add --no-cache wireguard-go wireguard-tools openvpn iproute2 iptables
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/bottle-agent /bin/bottle-agent
RUN chmod 0755 /bin/bottle-agent
COPY appliance/overlay/etc/wsl.conf /etc/wsl.conf
COPY appliance/overlay/etc/bottle-appliance-version /etc/bottle-appliance-version
CMD ["/bin/bottle-agent", "serve", "--boot"]
