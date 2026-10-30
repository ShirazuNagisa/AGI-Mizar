#!/usr/bin/env bash
#
# Fetch the model runner the app ships.
#
# The model is 1 GB and lives in the Mizarllm repository; the *program* that
# runs it is 20 MB and belongs in the bundle. Pinning the build here (rather
# than building llama.cpp in CI) keeps the release pipeline short and makes the
# exact binary reproducible: the tag in `llama-version.txt` is the whole answer
# to "which llama.cpp is in this release".
#
# Both architectures are downloaded and lipo'd into one universal directory.
# Not every file exists in both: arm64 alone carries the Metal backend, so those
# files stay thin — an Intel Mac never loads Metal anyway.
#
# Usage:
#   scripts/fetch-llama.sh          # download, lipo, ad-hoc sign into resources/bin
#
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION="$(cat scripts/llama-version.txt)"
OUT="crates/agi-app/resources/bin"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

base="https://github.com/ggml-org/llama.cpp/releases/download/${VERSION}"

for arch in arm64 x64; do
  echo "== ${arch}"
  mkdir -p "$WORK/$arch"
  curl -fsSL "${base}/llama-${VERSION}-bin-macos-${arch}.tar.gz" -o "$WORK/$arch.tgz"
  tar -xzf "$WORK/$arch.tgz" -C "$WORK/$arch" --strip-components=1
done

A_DIR="$(dirname "$(find "$WORK/arm64" -name llama-server -type f -print -quit)")"
X_DIR="$(dirname "$(find "$WORK/x64" -name llama-server -type f -print -quit)")"

if [[ ! -x "$A_DIR/llama-server" || ! -x "$X_DIR/llama-server" ]]; then
  echo "!! llama-server missing from one of the archives" >&2
  exit 1
fi

rm -rf "$OUT"
mkdir -p "$OUT"

# Every file the arm64 build needs, merged with its x86_64 twin where one exists.
# `cp -R` keeps the `.so`-style symlink chains (`libggml.0.dylib -> libggml.0.8.0.dylib`)
# that the loader resolves by name.
for source in "$A_DIR"/*; do
  name="$(basename "$source")"
  target="$X_DIR/$name"
  if [[ -f "$source" && -f "$target" ]] \
    && file "$source" | grep -q "Mach-O" \
    && file "$target" | grep -q "Mach-O"; then
    if ! lipo -create "$source" "$target" -output "$OUT/$name" 2>/dev/null; then
      # Same name, incompatible kinds (a script, a config): keep the arm64 one.
      cp -R "$source" "$OUT/$name"
    fi
  else
    cp -R "$source" "$OUT/$name"
  fi
done

# An ad-hoc signature is enough to run from this machine and to let the bundler
# sign the whole thing; a release re-signs every Mach-O with the Developer ID.
for binary in "$OUT"/*; do
  if file "$binary" | grep -q "Mach-O"; then
    codesign --force --sign - "$binary" >/dev/null 2>&1 || true
  fi
done

echo "== $(du -sh "$OUT" | cut -f1) in $OUT"
"$OUT/llama-server" --version 2>&1 | head -2 || true
