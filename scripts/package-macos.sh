#!/usr/bin/env bash
set -euo pipefail

APP_NAME="CutDub"
BIN_NAME="cutdub"
VERSION="${1:-0.1.1}"
TARGET="${2:-$(rustc -vV | sed -n 's/^host: //p')}"
case "$TARGET" in
    aarch64-apple-darwin) ARCH="arm64" ;;
    x86_64-apple-darwin) ARCH="x86_64" ;;
    *) echo "Unsupported macOS target: $TARGET" >&2; exit 1 ;;
esac

echo "==> Building ${APP_NAME} v${VERSION} (${ARCH}) in release mode..."
cargo build --locked --release --bin "${BIN_NAME}" --target "${TARGET}"

DIST_DIR="releases/${VERSION}"
BUNDLE_DIR="${DIST_DIR}/${APP_NAME}.app"
rm -rf "${BUNDLE_DIR}"
mkdir -p "${BUNDLE_DIR}/Contents/MacOS"
mkdir -p "${BUNDLE_DIR}/Contents/Resources"

cp "target/${TARGET}/release/${BIN_NAME}" "${BUNDLE_DIR}/Contents/MacOS/${BIN_NAME}"
chmod +x "${BUNDLE_DIR}/Contents/MacOS/${BIN_NAME}"
cp LICENSE README.md "${BUNDLE_DIR}/Contents/Resources/"
cp assets/icons/CutDub.icns "${BUNDLE_DIR}/Contents/Resources/CutDub.icns"

cat > "${BUNDLE_DIR}/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>org.cutdub.app</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleExecutable</key>
    <string>${BIN_NAME}</string>
    <key>CFBundleIconFile</key>
    <string>CutDub.icns</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
</dict>
</plist>
EOF

codesign --force --sign - "${BUNDLE_DIR}"
codesign --verify --strict "${BUNDLE_DIR}"

ARCHIVE_PATH="${DIST_DIR}/${APP_NAME}-${VERSION}-macos-${ARCH}.tar.gz"
tar -czf "${ARCHIVE_PATH}" -C "${DIST_DIR}" "${APP_NAME}.app"
echo "==> Created macOS app bundle archive: ${ARCHIVE_PATH}"
