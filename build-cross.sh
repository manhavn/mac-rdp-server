#!/usr/bin/env bash
set -euo pipefail

# Standalone local release builder for mac-rdp-server.
# Outputs unpacked binaries to dist/<target>/ and upload-ready archives to
# dist/packages/. Override targets with TARGETS=target1,target2 ./build-cross.sh.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"
export PATH="${HOME}/.local/bin:${HOME}/.cargo/bin:${PATH}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"

PROJECT_NAME="mac-rdp-server"
BIN_NAME="mac-rdp-server"
PROJECT_KIND="macos"
DEFAULT_TARGETS="x86_64-apple-darwin,aarch64-apple-darwin"
TARGETS_CSV="${TARGETS:-$DEFAULT_TARGETS}"

have() { command -v "$1" >/dev/null 2>&1; }
fail() { echo "error: $*" >&2; exit 1; }
compute_sha256() {
  local file="$1"
  if have sha256sum; then
    sha256sum "$file"
  elif have shasum; then
    shasum -a 256 "$file"
  else
    fail "Neither sha256sum nor shasum found"
  fi
}

IFS=',' read -r -a BUILD_TARGETS <<< "$TARGETS_CSV"

needs_zigbuild=false
if [[ "$(uname -s)" != "Darwin" ]]; then
  needs_zigbuild=true
fi
for t in "${BUILD_TARGETS[@]}"; do
  t="$(echo "$t" | xargs)"
  if [[ -n "$t" && "$t" != *apple-darwin* ]]; then
    needs_zigbuild=true
  fi
done

if [[ "$needs_zigbuild" == "true" ]]; then
  have cargo-zigbuild || fail "cargo-zigbuild is required (cargo install cargo-zigbuild --locked)"
  have zig || fail "Zig is required"
fi

if [[ "$TARGETS_CSV" == *apple-darwin* && -z "${SDKROOT:-}" ]]; then
  if have xcrun; then
    SDKROOT="$(xcrun --sdk macosx --show-sdk-path 2>/dev/null || true)"
    [[ -d "${SDKROOT:-}" ]] && export SDKROOT
  fi
  if [[ -z "${SDKROOT:-}" ]]; then
    for sdk in \
      "${HOME}/.sdk/MacOSX11.3.sdk" \
      "${HOME}/.local/macos-sdk/MacOSX11.3.sdk" \
      "${HOME}/.local/macos-sdk/MacOSX12.3.sdk" \
      "${HOME}/.local/macos-sdk/MacOSX13.3.sdk" \
      "${HOME}/.local/macos-sdk/MacOSX14.0.sdk" \
      "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk" \
      "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"
    do
      if [[ -d "$sdk" ]]; then
        export SDKROOT="$sdk"
        break
      fi
    done
  fi
fi
if [[ "$TARGETS_CSV" == *apple-darwin* && "$(uname -s)" != "Darwin" ]]; then
  [[ -n "${SDKROOT:-}" && -d "${SDKROOT:-}" ]] || fail "Apple SDK not found; set SDKROOT=/path/to/MacOSX.sdk"
fi

build_target() {
  local target="$1"
  if [[ "$(uname -s)" == "Darwin" && "$target" == *apple-darwin* ]]; then
    cargo build --release --target "$target"
  elif have cargo-zigbuild && have zig; then
    SDKROOT="${SDKROOT:-}" cargo zigbuild --release --target "$target"
  else
    cargo build --release --target "$target"
  fi
}

mkdir -p "$ROOT/dist/packages"
cargo fmt --all --check

built=()
for target in "${BUILD_TARGETS[@]}"; do
  target="$(echo "$target" | xargs)"
  [[ -n "$target" ]] || continue
  echo "==> Building $PROJECT_NAME for $target"
  rustup target add "$target" >/dev/null
  build_target "$target"

  ext=""
  [[ "$target" == *windows* ]] && ext=".exe"
  src="$ROOT/target/$target/release/${BIN_NAME}${ext}"
  [[ -f "$src" ]] || fail "binary not found: $src"

  target_dist="$ROOT/dist/$target"
  mkdir -p "$target_dist"
  cp -f "$src" "$target_dist/"
  archive="$ROOT/dist/packages/${PROJECT_NAME}-${target}.tar.gz"
  tar -C "$target_dist" -czf "$archive" .
  (cd "$ROOT/dist/packages" && compute_sha256 "$(basename "$archive")" > "$(basename "$archive").sha256")
  built+=("$target")
  echo "    $archive"
done

if [[ "$PROJECT_KIND" != "linux"   && -f "$ROOT/dist/x86_64-apple-darwin/$BIN_NAME"   && -f "$ROOT/dist/aarch64-apple-darwin/$BIN_NAME" ]]; then
  lipo_cmd=""
  have llvm-lipo && lipo_cmd="llvm-lipo"
  [[ -z "$lipo_cmd" ]] && have lipo && lipo_cmd="lipo"
  if [[ -n "$lipo_cmd" ]]; then
    universal_dir="$ROOT/dist/universal-apple-darwin"
    mkdir -p "$universal_dir"
    "$lipo_cmd" -create       "$ROOT/dist/x86_64-apple-darwin/$BIN_NAME"       "$ROOT/dist/aarch64-apple-darwin/$BIN_NAME"       -output "$universal_dir/$BIN_NAME"
    chmod +x "$universal_dir/$BIN_NAME"
    archive="$ROOT/dist/packages/${PROJECT_NAME}-universal-apple-darwin.tar.gz"
    tar -C "$universal_dir" -czf "$archive" .
    (cd "$ROOT/dist/packages" && compute_sha256 "$(basename "$archive")" > "$(basename "$archive").sha256")
    built+=("universal-apple-darwin")
    echo "    $archive"
  else
    echo "warn: universal macOS skipped; install llvm-lipo or run on macOS" >&2
  fi
fi

echo "Built: ${built[*]}"
echo "Upload-ready files: $ROOT/dist/packages/"

