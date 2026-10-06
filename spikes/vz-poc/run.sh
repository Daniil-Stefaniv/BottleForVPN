#!/usr/bin/env bash
# Запуск vz-спайка (M4 Task 9) на dev-машине (VF недоступен на CI-раннерах).
# Virtualization.framework требует entitlement com.apple.security.virtualization
# даже для adhoc-подписи — вписываем его перед запуском.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

TOKEN="${POC_TOKEN:-poc-token-0123456789abcdef}"

cargo build --release
codesign --force --sign - --entitlements entitlements.plist \
    target/release/vz-poc >/dev/null 2>&1 || {
    echo "codesign не удался (нужен Xcode CLT)" >&2
    exit 1
}

POC_TOKEN="$TOKEN" ./target/release/vz-poc image
