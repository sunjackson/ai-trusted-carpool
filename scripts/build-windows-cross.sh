#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="x86_64-pc-windows-msvc"
OUTPUT_DIR="${ROOT_DIR}/artifacts/windows-x86_64"
BUILD_LOG="$(mktemp "${TMPDIR:-/tmp}/trusted-carpool-windows.XXXXXX.log")"

cleanup() {
  rm -f "${BUILD_LOG}"
}
trap cleanup EXIT

export PATH="/opt/homebrew/opt/lld/bin:/opt/homebrew/opt/llvm/bin:${PATH}"

for command in cargo-xwin llvm-rc lld-link makensis; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "Missing ${command}. Follow the Tauri Windows cross-build prerequisites." >&2
    exit 1
  fi
done

cd "${ROOT_DIR}"
rustup target add "${TARGET}"
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
if ! npm run tauri build -- --runner cargo-xwin --target "${TARGET}" --bundles nsis \
  2>&1 | tee "${BUILD_LOG}"; then
  NSIS_DIR="${ROOT_DIR}/src-tauri/target/${TARGET}/release/nsis/x64"
  BINARY="${ROOT_DIR}/src-tauri/target/${TARGET}/release/trusted-carpool-desktop.exe"
  if [ "$(uname -s)" != "Darwin" ] \
    || [ "$(uname -m)" != "arm64" ] \
    || ! grep -q "Failed to bundle app with makensis" "${BUILD_LOG}" \
    || [ ! -f "${NSIS_DIR}/installer.nsi" ] \
    || [ ! -f "${BINARY}" ] \
    || ! command -v docker >/dev/null 2>&1; then
    echo "Windows cross-build failed before the supported Apple Silicon NSIS fallback" >&2
    exit 1
  fi

  echo "macOS arm64 makensis failed; rebuilding the generated NSIS script in Linux amd64"
  TAURI_CACHE="${HOME}/Library/Caches/tauri"
  docker run --platform linux/amd64 --rm \
    -v "${ROOT_DIR}:${ROOT_DIR}" \
    -v "${TAURI_CACHE}:${TAURI_CACHE}:ro" \
    -w "${NSIS_DIR}" \
    debian:bookworm-slim \
    sh -lc 'apt-get update >/dev/null && apt-get install -y --no-install-recommends nsis >/dev/null && makensis -INPUTCHARSET UTF8 -OUTPUTCHARSET UTF8 -V3 installer.nsi'

  VERSION="$(node -p "JSON.parse(require('fs').readFileSync('src-tauri/tauri.conf.json', 'utf8')).version")"
  mkdir -p "src-tauri/target/${TARGET}/release/bundle/nsis"
  cp -f "${NSIS_DIR}/nsis-output.exe" \
    "src-tauri/target/${TARGET}/release/bundle/nsis/可信拼车_${VERSION}_x64-setup.exe"
fi

rm -rf "${OUTPUT_DIR}"
mkdir -p "${OUTPUT_DIR}"
find "src-tauri/target/${TARGET}/release/bundle/nsis" -type f -name '*.exe' \
  -exec cp {} "${OUTPUT_DIR}/" \;

if ! find "${OUTPUT_DIR}" -type f -name '*.exe' -print -quit | grep -q .; then
  echo "Windows bundle completed without producing an NSIS installer" >&2
  exit 1
fi

echo "Windows artifacts:"
find "${OUTPUT_DIR}" -maxdepth 1 -type f -print
