#!/usr/bin/env bash
set -euo pipefail

APP_NAME="CutDub"
BIN_NAME="cutdub"
VERSION="${1:-0.1.1}"
TARGET="${2:-$(rustc -vV | sed -n 's/^host: //p')}"
case "$TARGET" in
    x86_64-unknown-linux-gnu) ARCH="x86_64" ;;
    aarch64-unknown-linux-gnu) ARCH="arm64" ;;
    *) echo "Unsupported Linux target: $TARGET" >&2; exit 1 ;;
esac

echo "==> Building ${APP_NAME} v${VERSION} (Linux ${ARCH}) in release mode..."
cargo build --locked --release --bin "${BIN_NAME}" --target "${TARGET}"

DIST_DIR="releases/${VERSION}"
PKG_DIR="${DIST_DIR}/${APP_NAME}-${VERSION}-linux-${ARCH}"
rm -rf "${PKG_DIR}"
mkdir -p "${PKG_DIR}/bin"
mkdir -p "${PKG_DIR}/share/applications"
mkdir -p "${PKG_DIR}/share/icons/hicolor/scalable/apps"
mkdir -p "${PKG_DIR}/share/icons/hicolor/256x256/apps"

cp "target/${TARGET}/release/${BIN_NAME}" "${PKG_DIR}/bin/${BIN_NAME}"
cp README.md LICENSE "${PKG_DIR}/"
cp assets/icons/cutdub.svg "${PKG_DIR}/share/icons/hicolor/scalable/apps/cutdub.svg"
cp assets/icons/cutdub-256.png "${PKG_DIR}/share/icons/hicolor/256x256/apps/cutdub.png"

cat > "${PKG_DIR}/share/applications/cutdub.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=CutDub
Comment=Precision Cross-Platform Video Processor
Exec=cutdub %F
Icon=cutdub
StartupWMClass=cutdub
Terminal=false
Categories=AudioVideo;Video;AudioVideoEditing;
EOF

ARCHIVE_PATH="${DIST_DIR}/${APP_NAME}-${VERSION}-linux-${ARCH}.tar.gz"
tar -czf "${ARCHIVE_PATH}" -C "${DIST_DIR}" "${APP_NAME}-${VERSION}-linux-${ARCH}"
echo "==> Created Linux release archive: ${ARCHIVE_PATH}"
