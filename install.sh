#!/bin/bash
# install.sh — Build and install obs-glitch plugin
#
# Usage:
#   ./install.sh          Build release + install to OBS plugins dir
#   ./install.sh --debug  Build debug + install
#   ./install.sh --remove Uninstall the plugin

set -euo pipefail

PLUGIN_NAME="glitch"
LIB_NAME="libobs_glitch.so"

# Arch/CachyOS: OBS loads plugins from /usr/lib/obs-plugins/ (flat dir)
OBS_PLUGIN_DIR="/usr/lib/obs-plugins"

case "${1:-}" in
    --remove)
        echo "🗑  Removing ${PLUGIN_NAME}..."
        sudo rm -f "${OBS_PLUGIN_DIR}/${LIB_NAME}"
        echo "✅ Removed. Restart OBS to apply."
        exit 0
        ;;
    --debug)
        PROFILE="debug"
        cargo build
        ;;
    *)
        PROFILE="release"
        cargo build --release
        ;;
esac

LIB_PATH="target/${PROFILE}/${LIB_NAME}"

if [ ! -f "${LIB_PATH}" ]; then
    echo "❌ Build output not found: ${LIB_PATH}"
    exit 1
fi

# Install alongside the other OBS plugins (requires sudo)
echo "📦 Installing to ${OBS_PLUGIN_DIR}/ (requires sudo)..."
sudo cp "${LIB_PATH}" "${OBS_PLUGIN_DIR}/"

echo ""
echo "✅ Installed ${PLUGIN_NAME}:"
echo "   ${OBS_PLUGIN_DIR}/${LIB_NAME}"
echo ""
echo "📋 Next steps:"
echo "   1. Restart OBS Studio"
echo "   2. Right-click any video source → Filters"
echo "   3. Click '+' → Glitch"
echo "   4. Change the expression in the properties panel"
echo ""
