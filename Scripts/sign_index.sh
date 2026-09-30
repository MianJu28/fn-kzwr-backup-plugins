#!/usr/bin/env bash
#
# 签名索引（index.json → index.json.sig）
#
# 机制与插件签名完全一致（同一把官方私钥、同样是 64 字节裸 Ed25519 签名）：
# 宿主用内置 OFFICIAL_PUBKEYS 验证索引，因此"索引不可伪造"与"插件不可伪造"
# 由同一条信任链保证。
#
# ⚠️ 只应在 release.yml 内运行（PR 工作流不引用本脚本）。
#
# 用法：Scripts/sign_index.sh index.json
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INDEX="${1:-index.json}"
KEY_FILE="${SIGN_KEY:-${PLUGIN_KEYS:-$ROOT/.keys}/sign.key}"

[ -f "$INDEX" ] || { echo "索引不存在：$INDEX" >&2; exit 1; }
[ -f "$KEY_FILE" ] || { echo "私钥不存在：$KEY_FILE（CI 应先从 Secrets 落盘）" >&2; exit 1; }
command -v openssl >/dev/null 2>&1 || { echo "找不到 openssl" >&2; exit 1; }

# 对**原始字节**签名（宿主也是读原始字节验签；不要先 parse 再序列化）
openssl pkeyutl -sign -rawin -inkey "$KEY_FILE" -in "$INDEX" -out "$INDEX.sig" \
  || { echo "签名失败：$INDEX" >&2; exit 1; }

size="$(stat -c%s "$INDEX.sig" 2>/dev/null || echo '?')"
echo "已签名索引：$INDEX -> $INDEX.sig（$size 字节）"
[ "$size" = "64" ] || { echo "警告：Ed25519 签名应为 64 字节，实际 $size" >&2; exit 1; }
