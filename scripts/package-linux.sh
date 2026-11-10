#!/usr/bin/env bash
set -euo pipefail

APP_NAME="CutDub"
BIN_NAME="cutdub"
VERSION="${1:-0.1.0}"
ARCH="$(uname -m)"

echo "==> Building ${APP_NAME} v${VERSION} (Linux ${ARCH}) in release mode..."
cargo build --release --bin "${BIN_NAME}"

DIST_DIR="target/dist"
PKG_DIR="${DIST_DIR}/${APP_NAME}-${VERSION}-linux-${ARCH}"
rm -rf "${PKG_DIR}"
mkdir -p "${PKG_DIR}/bin"
mkdir -p "${PKG_DIR}/share/applications"

cp "target/release/${BIN_NAME}" "${PKG_DIR}/bin/${BIN_NAME}"
cp README.md "${PKG_DIR}/README.md"

cat > "${PKG_DIR}/share/applications/cutdub.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=CutDub
Comment=Precision Cross-Platform Video Processor
Exec=cutdub %F
Terminal=false
Categories=AudioVideo;Video;AudioVideoEditing;
EOF

ARCHIVE_PATH="${DIST_DIR}/${APP_NAME}-${VERSION}-linux-${ARCH}.tar.gz"
tar -czf "${ARCHIVE_PATH}" -C "${DIST_DIR}" "${APP_NAME}-${VERSION}-linux-${ARCH}"
echo "==> Created Linux release archive: ${ARCHIVE_PATH}"
