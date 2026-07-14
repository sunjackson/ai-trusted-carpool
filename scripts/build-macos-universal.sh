#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="universal-apple-darwin"
APP_NAME="可信拼车.app"
DMG_NAME="可信拼车_0.1.0_universal.dmg"
APP_PATH="${ROOT_DIR}/src-tauri/target/${TARGET}/release/bundle/macos/${APP_NAME}"
OUTPUT_DIR="${ROOT_DIR}/artifacts/macos-universal"
STAGING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/trusted-carpool-macos.XXXXXX")"

cleanup() {
  rm -rf "${STAGING_DIR}"
}
trap cleanup EXIT

cd "${ROOT_DIR}"
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npm ci
npm run build
if [ "${SKIP_CHECKS:-0}" != "1" ]; then
  npm audit --audit-level=high
  npm test -- --run
  npm run lint
  cargo fmt --manifest-path src-tauri/Cargo.toml --check
  cargo test --manifest-path src-tauri/Cargo.toml --all-targets --all-features
  cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
fi
npm run tauri build -- --target "${TARGET}" --bundles app

codesign --force --deep --sign - "${APP_PATH}"
codesign --verify --deep --strict --verbose=2 "${APP_PATH}"

rm -rf "${OUTPUT_DIR}"
mkdir -p "${OUTPUT_DIR}"
ditto "${APP_PATH}" "${STAGING_DIR}/${APP_NAME}"
ln -s /Applications "${STAGING_DIR}/Applications"
hdiutil create \
  -volname "可信拼车" \
  -srcfolder "${STAGING_DIR}" \
  -ov \
  -format UDZO \
  "${OUTPUT_DIR}/${DMG_NAME}"
ditto "${APP_PATH}" "${OUTPUT_DIR}/${APP_NAME}"

echo "macOS universal artifacts:"
find "${OUTPUT_DIR}" -maxdepth 2 -print
