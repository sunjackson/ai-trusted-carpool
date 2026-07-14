#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="x86_64-pc-windows-msvc"
OUTPUT_DIR="${ROOT_DIR}/artifacts/windows-x86_64"

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
npm run tauri build -- --runner cargo-xwin --target "${TARGET}" --bundles nsis

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
