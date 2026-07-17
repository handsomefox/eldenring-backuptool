#!/usr/bin/env bash
# Build the Windows x86-64 release and package a portable ZIP + SHA-256.
# Requires: cargo-xwin, zip, sha256sum.
set -euo pipefail

TARGET="x86_64-pc-windows-msvc"
NAME="eldenring-save-guard"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
# Resolve the real target dir (may be redirected via cargo config or env).
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
TARGET_DIR="${TARGET_DIR:-${CARGO_TARGET_DIR:-target}}"
STAGE="dist/${NAME}"
ZIP="dist/${NAME}-v${VERSION}-${TARGET}.zip"

echo ">> building release for ${TARGET}"
cargo xwin build --release --target "${TARGET}"

EXE="${TARGET_DIR}/${TARGET}/release/eldenring-backuptool.exe"
[ -f "$EXE" ] || { echo "!! exe not found at $EXE" >&2; exit 1; }

echo ">> staging into ${STAGE}"
rm -rf "dist"
mkdir -p "${STAGE}"
cp "$EXE" "${STAGE}/"
cp README.md LICENSE SAFETY.md "${STAGE}/"

echo ">> zipping ${ZIP}"
( cd dist && zip -r -q "$(basename "$ZIP")" "${NAME}" )

echo ">> checksum"
( cd dist && sha256sum "$(basename "$ZIP")" > "$(basename "$ZIP").sha256" )

echo ">> verifying"
( cd dist && sha256sum -c "$(basename "$ZIP").sha256" )
unzip -l "$ZIP"

echo ">> done: ${ZIP}"
