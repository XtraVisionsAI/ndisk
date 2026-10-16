#!/bin/bash
# 命令行构建 App 与扩展。未配置签名时用 --unsigned 只做编译检查。
#   apple/scripts/build-app.sh [--unsigned] [--release]
set -euo pipefail
cd "$(dirname "$0")/.."
CONF=Debug; SIGN=()
for a in "$@"; do
  case $a in
    --unsigned) SIGN=(CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY=) ;;
    --release) CONF=Release ;;
  esac
done
RUST_FLAGS=(); [ $CONF = Release ] && RUST_FLAGS=(--release --universal)
scripts/build-rust.sh ${RUST_FLAGS[@]+"${RUST_FLAGS[@]}"}
[ -d NDisk.xcodeproj ] || scripts/gen-project.sh
xcodebuild -project NDisk.xcodeproj -scheme NDisk -configuration $CONF \
  -derivedDataPath ../target/xcode ${SIGN[@]+"${SIGN[@]}"} build
