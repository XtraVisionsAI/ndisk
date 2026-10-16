#!/bin/bash
# 构建 Rust 核心并生成 Swift 绑定，产物供 apple/NdiskKit 使用：
#   apple/NdiskKit/Frameworks/NdiskFFI.xcframework        静态库 + 头文件 + modulemap
#   apple/NdiskKit/Sources/NdiskKit/Generated/ndisk_ffi.swift
#
# 用法：apple/scripts/build-rust.sh [--release] [--universal]
#   默认 debug、仅本机架构；--universal 同时构建 arm64 与 x86_64 并 lipo 合并。
# 也可作为 Xcode 构建前置脚本运行：会读取 CONFIGURATION / ARCHS。
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
PKG="$ROOT/apple/NdiskKit"
OUT="$ROOT/target/apple"
PROFILE=debug
UNIVERSAL=0
for a in "$@"; do
  case $a in
    --release) PROFILE=release ;;
    --universal) UNIVERSAL=1 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done
# Xcode 环境
if [ "${CONFIGURATION:-}" = "Release" ]; then PROFILE=release; fi
if [ -n "${ARCHS:-}" ] && [[ "$ARCHS" == *x86_64* ]] && [[ "$ARCHS" == *arm64* ]]; then UNIVERSAL=1; fi

# Xcode 运行脚本时 PATH 很精简，补上 cargo
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
# 与 Xcode 工程的部署目标一致，避免链接警告
export MACOSX_DEPLOYMENT_TARGET=13.0

case "$(uname -m)" in arm64) NATIVE=aarch64-apple-darwin ;; *) NATIVE=x86_64-apple-darwin ;; esac
TARGETS=("$NATIVE")
[ $UNIVERSAL = 1 ] && TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)

CARGO_FLAGS=(-p ndisk-ffi --lib)
[ $PROFILE = release ] && CARGO_FLAGS+=(--release)

LIBS=()
for t in "${TARGETS[@]}"; do
  echo "==> cargo build ($PROFILE, $t)"
  (cd "$ROOT" && cargo build "${CARGO_FLAGS[@]}" --target "$t")
  LIBS+=("$ROOT/target/$t/$PROFILE/libndisk_ffi.a")
done

rm -rf "$OUT" && mkdir -p "$OUT/headers" "$OUT/swift" "$OUT/lib"
if [ ${#LIBS[@]} -gt 1 ]; then
  lipo -create "${LIBS[@]}" -output "$OUT/lib/libndisk_ffi.a"
else
  cp "${LIBS[0]}" "$OUT/lib/libndisk_ffi.a"
fi

echo "==> uniffi-bindgen-swift"
DYLIB="$ROOT/target/$NATIVE/$PROFILE/libndisk_ffi.dylib"
(cd "$ROOT" && cargo run -q -p ndisk-ffi --bin uniffi-bindgen-swift -- \
  "$DYLIB" "$OUT/headers" --headers --modulemap \
  --module-name ndisk_ffiFFI --modulemap-filename module.modulemap)
(cd "$ROOT" && cargo run -q -p ndisk-ffi --bin uniffi-bindgen-swift -- \
  "$DYLIB" "$OUT/swift" --swift-sources)

echo "==> xcframework"
rm -rf "$PKG/Frameworks/NdiskFFI.xcframework"
mkdir -p "$PKG/Frameworks" "$PKG/Sources/NdiskKit/Generated"
xcodebuild -create-xcframework \
  -library "$OUT/lib/libndisk_ffi.a" -headers "$OUT/headers" \
  -output "$PKG/Frameworks/NdiskFFI.xcframework" >/dev/null
cp "$OUT/swift/"*.swift "$PKG/Sources/NdiskKit/Generated/"
echo "==> done: $(lipo -archs "$OUT/lib/libndisk_ffi.a") $PROFILE"
