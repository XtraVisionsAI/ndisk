#!/bin/bash
# 生成 NDisk.xcodeproj（需要 XcodeGen：brew install xcodegen）。
set -euo pipefail
cd "$(dirname "$0")/.."
xcodegen generate --spec project.yml
