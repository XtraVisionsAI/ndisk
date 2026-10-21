#!/bin/bash
# 构建可分发的 macOS 版本：Release 通用二进制 → Developer ID 签名 → 公证 → DMG。
#
#   apple/scripts/release.sh                  # 正式：Developer ID 签名 + 公证（App 与 DMG 各一次）
#   apple/scripts/release.sh --dev            # 演练：开发证书导出、打 DMG，跳过公证
#   apple/scripts/release.sh --skip-notarize  # Developer ID 签名但不公证（检查签名用）
#
# 前提（正式发布）：
#   - Xcode → Settings → Accounts 已登录团队账号，团队可用 Developer ID Application 证书
#     （Xcode 会按需创建 / 下载证书与描述文件；共享钥匙串 entitlement 需要描述文件）
#   - 公证凭据已存入钥匙串（只需一次）：
#       xcrun notarytool store-credentials ndisk-notary \
#         --apple-id <Apple ID> --team-id <Team ID> --password <app 专用密码>
#     配置名可用 NDISK_NOTARY_PROFILE 覆盖。
#
#   - Sparkle 更新签名密钥已生成（只需一次，私钥存于钥匙串，务必备份）：
#       <Sparkle>/bin/generate_keys            # 打印公钥，填入 Config/NDisk.xcconfig 的 NDISK_SPARKLE_PUBLIC_KEY
#       <Sparkle>/bin/generate_keys -x 备份文件 # 导出私钥备份
#   - 每次发布前递增 Config/NDisk.xcconfig 的 MARKETING_VERSION 与 CURRENT_PROJECT_VERSION
#     （Sparkle 按 CURRENT_PROJECT_VERSION 比较新旧）
#
# 产物：target/release-app/NDisk-<版本>.dmg 及其 .sha256；正式模式另有 appcast.xml，
#       与 DMG 一起上传到 GitHub Release v<版本>（SUFeedURL 指向 latest/download/appcast.xml）
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(cd .. && pwd)
OUT="$ROOT/target/release-app"
PROFILE_NAME=${NDISK_NOTARY_PROFILE:-ndisk-notary}
MODE=release; NOTARIZE=1
for a in "$@"; do
  case $a in
    --dev) MODE=dev; NOTARIZE=0 ;;
    --skip-notarize) NOTARIZE=0 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

step() { printf '\n==> %s\n' "$*"; }

TEAM=$(sed -n 's/^NDISK_TEAM_ID *= *//p' Config/Signing.local.xcconfig 2>/dev/null | tr -d '[:space:]')
[ -n "$TEAM" ] || { echo "NDISK_TEAM_ID missing in apple/Config/Signing.local.xcconfig" >&2; exit 1; }
VERSION=$(sed -n 's/^MARKETING_VERSION *= *//p' Config/NDisk.xcconfig | tr -d '[:space:]')
BUILD=$(sed -n 's/^CURRENT_PROJECT_VERSION *= *//p' Config/NDisk.xcconfig | tr -d '[:space:]')
SUFFIX=$([ $MODE = dev ] && echo "-dev" || echo "")
PUBKEY=$(sed -n 's/^NDISK_SPARKLE_PUBLIC_KEY *= *//p' Config/NDisk.xcconfig Config/Signing.local.xcconfig 2>/dev/null | tr -d '[:space:]' | tail -1)
if [ $MODE = release ] && [ -z "$PUBKEY" ]; then
  echo "NDISK_SPARKLE_PUBLIC_KEY is empty: released builds could never receive updates (see header)" >&2; exit 1
fi
DMG="$OUT/NDisk-$VERSION$SUFFIX.dmg"

rm -rf "$OUT" && mkdir -p "$OUT"
step "generate project"
scripts/gen-project.sh >/dev/null

step "archive (Release, universal) $VERSION ($BUILD)"
xcodebuild -project NDisk.xcodeproj -scheme NDisk -configuration Release \
  -archivePath "$OUT/NDisk.xcarchive" -derivedDataPath "$ROOT/target/xcode-release" \
  -destination 'generic/platform=macOS' -allowProvisioningUpdates archive \
  > "$OUT/archive.log" 2>&1 || { tail -40 "$OUT/archive.log"; echo "archive failed, see $OUT/archive.log" >&2; exit 1; }

step "export ($([ $MODE = dev ] && echo development || echo developer-id))"
METHOD=$([ $MODE = dev ] && echo debugging || echo developer-id)
cat > "$OUT/ExportOptions.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>$METHOD</string>
  <key>teamID</key><string>$TEAM</string>
  <key>signingStyle</key><string>automatic</string>
</dict>
</plist>
PLIST
xcodebuild -exportArchive -archivePath "$OUT/NDisk.xcarchive" -exportPath "$OUT/export" \
  -exportOptionsPlist "$OUT/ExportOptions.plist" -allowProvisioningUpdates \
  > "$OUT/export.log" 2>&1 || { tail -40 "$OUT/export.log"; echo "export failed, see $OUT/export.log" >&2; exit 1; }
APP="$OUT/export/NDisk.app"

step "verify signature"
codesign --verify --deep --strict --verbose=2 "$APP" 2>&1 | tail -2
lipo -archs "$APP/Contents/MacOS/NDisk"
IDENTITY=$(codesign -dvv "$APP" 2>&1 | sed -n 's/^Authority=//p' | head -1)
echo "signed by: $IDENTITY"

notarize() {
  xcrun notarytool submit "$1" --keychain-profile "$PROFILE_NAME" --wait --output-format json \
    > "$OUT/notary-$(basename "$1").json"
  local status; status=$(plutil -extract status raw "$OUT/notary-$(basename "$1").json" 2>/dev/null || echo unknown)
  echo "notary status: $status"
  [ "$status" = "Accepted" ] || { cat "$OUT/notary-$(basename "$1").json"; exit 1; }
}

if [ $NOTARIZE = 1 ]; then
  step "notarize app"
  ditto -c -k --keepParent "$APP" "$OUT/NDisk.zip"
  notarize "$OUT/NDisk.zip"
  xcrun stapler staple "$APP"
fi

step "dmg"
STAGE="$OUT/dmg"
mkdir -p "$STAGE" && ditto "$APP" "$STAGE/NDisk.app" && ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "NDisk $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$DMG" >/dev/null
if [ $MODE = release ]; then
  codesign --sign "$IDENTITY" --timestamp "$DMG"
fi

if [ $NOTARIZE = 1 ]; then
  step "notarize dmg"
  notarize "$DMG"
  xcrun stapler staple "$DMG"
  step "gatekeeper"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
  spctl --assess --type execute --verbose=2 "$APP"
fi

if [ $MODE = release ]; then
  step "appcast"
  SPARKLE_BIN="$ROOT/target/xcode-release/SourcePackages/artifacts/sparkle/Sparkle/bin"
  mkdir -p "$OUT/updates" && cp "$DMG" "$OUT/updates/"
  # 私钥从钥匙串读取；EdDSA 签名与长度写入 enclosure。
  "$SPARKLE_BIN/generate_appcast" \
    --download-url-prefix "https://github.com/XtraVisionsAI/ndisk/releases/download/v$VERSION/" \
    -o "$OUT/appcast.xml" "$OUT/updates"
  grep -q 'sparkle:edSignature' "$OUT/appcast.xml" || { echo "appcast has no EdDSA signature" >&2; exit 1; }
fi

(cd "$OUT" && shasum -a 256 "$(basename "$DMG")" > "$(basename "$DMG").sha256")
step "done"
echo "$DMG"
cat "$DMG.sha256"
[ $MODE = release ] && echo "upload with: gh release create v$VERSION \"$DMG\" \"$OUT/appcast.xml\" --repo XtraVisionsAI/ndisk"
