#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCKER_PLATFORM="${DOCKER_PLATFORM:-linux/arm64}"
ARTIFACT_ARCH="${ARTIFACT_ARCH:-aarch64}"
IMAGE_NAME="${LINUX_BUILD_IMAGE:-trusted-carpool-linux-builder:bookworm}"
OUTPUT_DIR="${ROOT_DIR}/artifacts/linux-${ARTIFACT_ARCH}"
CACHE_DIR="${ROOT_DIR}/.build/linux-${ARTIFACT_ARCH}"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/trusted-carpool-linux.XXXXXX")"

cleanup() {
  rm -rf "${WORK_DIR}"
}
trap cleanup EXIT

docker build \
  --platform "${DOCKER_PLATFORM}" \
  -t "${IMAGE_NAME}" \
  -f "${ROOT_DIR}/scripts/linux/Dockerfile" \
  "${ROOT_DIR}/scripts/linux"

rsync -a --delete \
  --exclude node_modules \
  --exclude dist \
  --exclude src-tauri/target \
  --exclude .build \
  --exclude artifacts \
  "${ROOT_DIR}/" "${WORK_DIR}/"

mkdir -p \
  "${CACHE_DIR}/cargo-git" \
  "${CACHE_DIR}/cargo-registry" \
  "${CACHE_DIR}/npm" \
  "${CACHE_DIR}/target"

docker run --rm \
  --platform "${DOCKER_PLATFORM}" \
  -e CI=true \
  -e APPIMAGE_EXTRACT_AND_RUN=1 \
  -e SKIP_CHECKS="${SKIP_CHECKS:-0}" \
  -v "${WORK_DIR}:/workspace" \
  -v "${CACHE_DIR}/cargo-git:/usr/local/cargo/git" \
  -v "${CACHE_DIR}/cargo-registry:/usr/local/cargo/registry" \
  -v "${CACHE_DIR}/npm:/root/.npm" \
  -v "${CACHE_DIR}/target:/workspace/src-tauri/target" \
  -w /workspace \
  "${IMAGE_NAME}" \
  bash -c '
    set -euo pipefail
    node --version
    npm --version
    rustc --version
    cargo --version
    npm ci
    npm run build
    if [ "${SKIP_CHECKS}" != "1" ]; then
      npm audit --audit-level=high
      npm test -- --run
      npm run lint
      cargo fmt --manifest-path src-tauri/Cargo.toml --check
      cargo test --manifest-path src-tauri/Cargo.toml --all-targets --all-features
      cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
    fi
    npm run tauri build -- --bundles deb,appimage -vv
  '

rm -rf "${OUTPUT_DIR}"
mkdir -p "${OUTPUT_DIR}"
find "${CACHE_DIR}/target/release/bundle" -type f \
  \( -name '*.deb' -o -name '*.AppImage' \) \
  -exec cp {} "${OUTPUT_DIR}/" \;

if ! find "${OUTPUT_DIR}" -type f -print -quit | grep -q .; then
  echo "Linux bundle completed without producing .deb or .AppImage files" >&2
  exit 1
fi

echo "Linux artifacts:"
find "${OUTPUT_DIR}" -maxdepth 1 -type f -print
