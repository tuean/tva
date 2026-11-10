#!/usr/bin/env bash
# 打包成 TVA.app。
#
# macOS 的麦克风/语音识别权限是按 app 身份授权的：
# - 用稳定的自签名证书签名（scripts/setup-signing.sh 创建）-> 授权一次就记住；
# - ad-hoc 签名（--sign -）没有稳定身份，每次重新编译都会被当成新 app 重新询问。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${1:-release}"
BIN="$ROOT/target/$PROFILE/tva"
APP="$ROOT/dist/TVA.app"
IDENTITY="${TVA_SIGN_IDENTITY:-TVA Local Signing}"

cd "$ROOT"
cargo build -p tva-app --profile "$PROFILE"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/tva"
cp "$ROOT/resources/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/resources/TVA.icns" "$APP/Contents/Resources/TVA.icns"

if security find-identity -v -p codesigning 2>/dev/null | grep -q "$IDENTITY"; then
    codesign --force --sign "${IDENTITY}" --timestamp=none "$APP"
    echo "已用 [${IDENTITY}] 签名（权限只需授权一次）"
else
    codesign --force --sign - --timestamp=none "$APP" >/dev/null 2>&1 || true
    echo "警告: 未找到签名身份 [${IDENTITY}]，已用 ad-hoc 签名。"
    echo "      ad-hoc 签名下每次重新编译都会重新弹权限，建议先运行:"
    echo "      ./scripts/setup-signing.sh"
fi

echo "已生成: $APP"
echo "运行:   open '$APP'"
