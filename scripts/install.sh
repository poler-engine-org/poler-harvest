#!/usr/bin/env bash
# POLER-HARVEST Universal Linux Installer
# Работает на ЛЮБОМ Linux (Debian, Ubuntu, CachyOS, Arch, Fedora, Alpine) без sudo и без менеджеров пакетов.
set -e

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

echo "⬇️  Скачивание бинарника в $INSTALL_DIR..."
if command -v curl >/dev/null 2>&1; then
    curl -sSL "$URL" | tar -xz -C "$INSTALL_DIR"
elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$URL" | tar -xz -C "$INSTALL_DIR"
else
    echo "❌ Ошибка: не найден curl или wget"
    exit 1
fi

chmod +x "$INSTALL_DIR/$BIN_NAME"

echo "✅ poler-harvest успешно установлен: $INSTALL_DIR/$BIN_NAME"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "💡 Добавьте каталог в PATH: export PATH=\"\$PATH:$INSTALL_DIR\"" ;;
esac

"$INSTALL_DIR/$BIN_NAME" --version
