#!/usr/bin/env bash
# 一次性创建本地自签名代码签名证书（"TVA Local Signing"）。
#
# 为什么需要它：ad-hoc 签名（codesign --sign -）没有稳定身份，macOS 用 cdhash
# 识别 app，每次重新编译 cdhash 就变，于是麦克风/语音识别权限会被当成新 app
# 反复询问。用一个固定的证书签名后，权限只需授权一次。
#
# 幂等：已存在就跳过。
set -euo pipefail

IDENTITY="${TVA_SIGN_IDENTITY:-TVA Local Signing}"
DIR="${HOME}/.tva-signing"

if security find-identity -v -p codesigning 2>/dev/null | grep -q "$IDENTITY"; then
    echo "签名身份已存在: $IDENTITY"
    exit 0
fi

echo "创建自签名证书: $IDENTITY"
mkdir -p "$DIR"
cd "$DIR"

openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
    -keyout tva.key -out tva.crt \
    -subj "/CN=$IDENTITY/O=TVA" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" \
    -addext "basicConstraints=critical,CA:false" >/dev/null 2>&1

# Apple 的 security 工具读不了 OpenSSL 3 默认的 PBES2，必须用老的 PBE。
openssl pkcs12 -export -out tva.p12 -inkey tva.key -in tva.crt \
    -passout pass:tva -name "$IDENTITY" \
    -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES -macalg sha1 >/dev/null 2>&1

security import tva.p12 -k "$HOME/Library/Keychains/login.keychain-db" \
    -P tva -T /usr/bin/codesign -A >/dev/null

# 标记为受信任的代码签名证书（用户域，不需要管理员权限）。
security add-trusted-cert -r trustRoot -p codeSign \
    -k "$HOME/Library/Keychains/login.keychain-db" tva.crt

if security find-identity -v -p codesigning | grep -q "$IDENTITY"; then
    echo "完成。之后 ./scripts/bundle.sh 会自动用它签名。"
else
    echo "失败：证书没有被识别为有效的代码签名身份。" >&2
    exit 1
fi
