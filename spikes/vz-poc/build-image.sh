#!/usr/bin/env bash
# Сборка POC-образа для vz-спайка (M4 Task 9) БЕЗ docker:
# Alpine 3.22 aarch64 minirootfs + пакеты (wireguard-go/iproute2/
# iptables и либы) прямой распаковкой .apk + настоящий bottle-agent
# (aarch64-musl, собран rust-lld) → rootfs.cpio.gz (newc, питон-
# архиватор — системный cpio macOS пишет odc) + vmlinuz-virt.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
WORK="${1:-$SCRIPT_DIR/image}"
ALPINE_BRANCH="v3.22"
ALPINE_BASE="https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH"
AGENT="$REPO_ROOT/target/aarch64-unknown-linux-musl/release/bottle-agent"

[ -x "$AGENT" ] || {
    echo "нет агента: $AGENT"
    echo "соберите: RUSTFLAGS=\"-C linker=rust-lld\" cargo build --release -p agent --target aarch64-unknown-linux-musl"
    exit 1
}

mkdir -p "$WORK"
cd "$WORK"

# --- 1. minirootfs ---
if [ ! -d rootfs ]; then
    echo "==> minirootfs"
    if [ ! -f minirootfs.tar.gz ]; then
        curl -sLfL -o minirootfs.tar.gz \
            "$ALPINE_BASE/releases/aarch64/alpine-minirootfs-3.22.0-aarch64.tar.gz"
    fi
    mkdir -p rootfs
    tar -xzf minirootfs.tar.gz -C rootfs
fi

# --- 2. пакеты apk: индексы + резолвер + распаковка ---
for repo in main community; do
    if [ ! -f "APKINDEX-$repo" ]; then
        echo "==> APKINDEX $repo"
        curl -sLfL -o "APKINDEX-$repo.tar.gz" "$ALPINE_BASE/$repo/aarch64/APKINDEX.tar.gz"
        tar -xzf "APKINDEX-$repo.tar.gz" APKINDEX
        mv APKINDEX "APKINDEX-$repo"
    fi
done

if [ ! -f .pkgs-unpacked ]; then
    echo "==> резолв/скачивание/распаковка пакетов"
    python3 "$SCRIPT_DIR/resolve-apk-deps.py" --urls \
        APKINDEX-main APKINDEX-community \
        wireguard-go iproute2 iptables libcap libmnl libnftnl libpcap libbz2 \
        > pkg-urls.txt
    wc -l < pkg-urls.txt | xargs echo "пакетов:"
    while read -r url; do
        fn=$(basename "$url")
        [ -f "$fn" ] || curl -sLfL -o "$fn" "$url"
        tar -xzf "$fn" -C rootfs
    done < pkg-urls.txt
    touch .pkgs-unpacked
fi

# --- 3. агент, init, announce ---
echo "==> overlay спайка"
install -m 0755 "$AGENT" rootfs/bin/bottle-agent
mkdir -p rootfs/etc rootfs/run
# busybox init: DHCP на eth0, агент (respawn) и announce (respawn).
cat > rootfs/etc/inittab <<'EOF'
::sysinit:/bin/mkdir -p /proc /sys /dev /run
::sysinit:/bin/mount -t proc proc /proc
::sysinit:/bin/mount -t sysfs sys /sys
::sysinit:/bin/mount -t devtmpfs dev /dev 2>/dev/null || true
::sysinit:/bin/mount -t tmpfs run /run
::sysinit:/sbin/mdev -s 2>/dev/null || true
::respawn:/sbin/udhcpc -i eth0 -f -q -t 10
::respawn:/bin/bottle-agent serve --boot
::respawn:/etc/poc-announce.sh
::ctrlaltdel:/sbin/reboot
EOF
# announce: токен из kernel-cmdline, IP от udhcpc; шлём в broadcast,
# слушатель-хост фильтрует по токену (POC; подпись — T10).
cat > rootfs/etc/poc-announce.sh <<'EOF'
#!/bin/sh
TOKEN="$(sed -n 's/.*bfa.token=\([^ ]*\).*/\1/p' /proc/cmdline)"
[ -n "$TOKEN" ] || exit 1
while :; do
    IP="$(ip -4 addr show dev eth0 2>/dev/null | sed -n 's/.*inet \([0-9.]*\).*/\1/p' | head -1)"
    if [ -n "$IP" ]; then
        printf 'BOTTLE-ANNOUNCE %s %s' "$TOKEN" "$IP" | nc -u -w1 192.168.76.255 7778
    fi
    sleep 2
done
EOF
chmod 0755 rootfs/etc/poc-announce.sh

# --- 4. kernel ---
if [ ! -f vmlinuz-virt ]; then
    echo "==> linux-virt kernel"
    python3 "$SCRIPT_DIR/resolve-apk-deps.py" --urls \
        APKINDEX-main APKINDEX-community linux-virt > kernel-url.txt
    KURL=$(grep '/linux-virt-[0-9]' kernel-url.txt | head -1)
    [ -n "$KURL" ] || { echo "linux-virt не найден в индексе"; exit 1; }
    curl -sLfL -o linux-virt.apk "$KURL"
    tar -xzf linux-virt.apk boot/vmlinuz-virt
    # Alpine aarch64 kernel — PE/EFI «zboot»-контейнер; VZLinuxBootLoader
    # ждёт сырой ARM64 Image — вырезаем gzip-пейлоад.
    python3 - "$SCRIPT_DIR" <<'PYEOF'
import zlib, sys
data = open("boot/vmlinuz-virt", "rb").read()
offset = data.find(b"\x1f\x8b\x08", 4)
raw = zlib.decompressobj(31).decompress(data[offset:])
open("Image", "wb").write(raw)
print(f"unzboot: {len(raw)//1024} KiB raw Image")
PYEOF
    rm -f vmlinuz-virt
fi

# --- 5. rootfs → cpio newc (gzip) ---
echo "==> cpio"
python3 "$SCRIPT_DIR/mkcpio.py" rootfs rootfs.cpio
gzip -9 -c rootfs.cpio > rootfs.cpio.gz

echo "Готово: $WORK/vmlinuz-virt, $WORK/rootfs.cpio.gz"
