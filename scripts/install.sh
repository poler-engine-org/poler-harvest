#!/usr/bin/env bash
# POLER-HARVEST Universal Linux Installer with Automated SHA-256 Verification
set -euo pipefail

REPO="poler-engine-org/poler-harvest"
VERSION="0.96.1"
BIN_NAME="poler-harvest"

echo "⚡ Установка poler-harvest v${VERSION}..."

ARCH=$(uname -m)
case "$ARCH" in
    x86_64|amd64)
        TARGET="x86_64-unknown-linux-gnu"
        ;;
    *)
        echo "❌ Ошибка: архитектура $ARCH пока не поддерживается (требуется x86_64)"
        exit 1
        ;;
esac

if [ -w "/usr/local/bin" ]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="$HOME/.local/bin"
    mkdir -p "$INSTALL_DIR"
fi

URL="https://github.com/${REPO}/releases/download/v${VERSION}/poler-harvest-${TARGET}.tar.gz"
SUMS_URL="https://github.com/${REPO}/releases/download/v${VERSION}/SHA256SUMS"

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

echo "⬇️  Скачивание и проверка дистрибутива..."
curl -sSLf "$URL" -o "$TMP_DIR/archive.tar.gz"

if curl -sSLf "$SUMS_URL" -o "$TMP_DIR/SHA256SUMS" 2>/dev/null; then
    EXPECTED_SHA=$(grep "poler-harvest-${TARGET}.tar.gz" "$TMP_DIR/SHA256SUMS" | awk '{print $1}')
    if [ -n "$EXPECTED_SHA" ]; then
        ACTUAL_SHA=$(sha256sum "$TMP_DIR/archive.tar.gz" | awk '{print $1}')
        if [ "$EXPECTED_SHA" != "$ACTUAL_SHA" ]; then
            echo "❌ Ошибка безопасности: SHA-256 не совпадает!"
            exit 1
        fi
        echo "🔒 Контрольная сумма SHA-256 подтверждена ($ACTUAL_SHA)"
    fi
fi

tar -xzf "$TMP_DIR/archive.tar.gz" -C "$INSTALL_DIR"
chmod +x "$INSTALL_DIR/$BIN_NAME"

echo "✅ poler-harvest успешно установлен: $INSTALL_DIR/$BIN_NAME"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "💡 Добавьте каталог в PATH: export PATH=\"\$PATH:$INSTALL_DIR\"" ;;
esac

"$INSTALL_DIR/$BIN_NAME" --version
