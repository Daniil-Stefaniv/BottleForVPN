#!/usr/bin/env bash
# Сборка rootfs-апплайенса BottleForApps:
# Alpine + bottle-agent (musl, x86_64) + сетевые пакеты => artifacts/bottleforapps-rootfs.tar.gz
#
# Работает локально (docker через Colima, платформа linux/amd64 через Rosetta)
# и в CI (ubuntu-latest, нативный amd64).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="bottleforapps-appliance:latest"
PLATFORM="linux/amd64"
ARTIFACT_DIR="$ROOT_DIR/artifacts"
ARTIFACT="$ARTIFACT_DIR/bottleforapps-rootfs.tar.gz"

# --- 1. version-файл из [workspace.package] корневого Cargo.toml -------------
# Та же версия попадает в agent (CARGO_PKG_VERSION) через version.workspace = true.
VERSION="$(awk -F'=' '/^\[workspace\.package\]/{in_section=1; next}
    in_section && $1 ~ /^[[:space:]]*version[[:space:]]*$/ {gsub(/[[:space:]\"]/, "", $2); print $2; exit}' \
    "$ROOT_DIR/Cargo.toml")"
if [[ -z "$VERSION" ]]; then
    echo "ERROR: version не найдена в [workspace.package] $ROOT_DIR/Cargo.toml" >&2
    exit 1
fi
mkdir -p "$ROOT_DIR/appliance/overlay/etc"
echo "$VERSION" > "$ROOT_DIR/appliance/overlay/etc/bottle-appliance-version"
echo "==> Версия апплайенса: $VERSION"

# --- 2. Сборка образа (multi-stage, всегда linux/amd64) ----------------------
echo "==> docker build $IMAGE ($PLATFORM)"
docker build -f "$ROOT_DIR/appliance/Dockerfile" -t "$IMAGE" --platform "$PLATFORM" "$ROOT_DIR"

# --- 3. Smoke: бинарник отвечает, overlay-файлы на месте ---------------------
echo "==> smoke-проверка образа"
docker run --rm --platform "$PLATFORM" --entrypoint /bin/sh "$IMAGE" \
    -c '/bin/bottle-agent --help >/dev/null \
        && cat /etc/bottle-appliance-version \
        && test -f /etc/wsl.conf \
        && echo SMOKE-OK'

# --- 4. Экспорт rootfs в tar.gz ----------------------------------------------
# Пишем во временный файл и mv-им атомарно: на месте артефакта не может
# оказаться обрезанный tar. Trap снимает контейнер и мусор при любом выходе.
echo "==> экспорт rootfs"
cid="$(docker create "$IMAGE" /bin/true)"
mkdir -p "$ARTIFACT_DIR"
tmp_artifact="$(mktemp "$ARTIFACT_DIR/bottleforapps-rootfs.tar.gz.XXXXXX")"
cleanup() {
    docker rm -f "$cid" > /dev/null 2>&1 || true
    rm -f "$tmp_artifact"
}
trap cleanup EXIT
docker export "$cid" | gzip > "$tmp_artifact"
mv "$tmp_artifact" "$ARTIFACT"

# --- 5. Итог -----------------------------------------------------------------
echo "==> Готово: $ARTIFACT ($(du -h "$ARTIFACT" | cut -f1))"
