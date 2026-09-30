#!/usr/bin/env bash
#
# 插件与索引签名工具（**仅在 release.yml 的信任边界内使用**）
#
# 与主仓库 `Scripts/sign_plugin.sh` 同一套机制：openssl 的 Ed25519，
# 签名为 64 字节裸签名；宿主用 `ring` 验签，两侧格式完全兼容。
#
# 私钥来源（按优先级）：
#   1. 环境变量 `SIGN_KEY`（指定路径）
#   2. `PLUGIN_SIGN_KEY_B64`（base64 的私钥；CI 从 Secrets 注入后落盘）
#   3. 默认 `.keys/sign.key`
#
# ⚠️ 本脚本**绝不可**在 PR 触发的工作流里运行（`ci.yml` 不引用它）。
#
# 用法：
#   Scripts/sign_all.sh --pubkey-only          打印公钥（校验私钥可用）
#   Scripts/sign_all.sh --sign [--arch <arch>] 给 dist/plugins/*.so 签名
#   Scripts/sign_all.sh --verify               自检所有 .sig
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KEYS_DIR="${PLUGIN_KEYS:-$ROOT/.keys}"
KEY_FILE="${SIGN_KEY:-$KEYS_DIR/sign.key}"

die() { echo "错误：$*" >&2; exit 1; }
need_openssl() { command -v openssl >/dev/null 2>&1 || die "找不到 openssl（Ed25519 依赖 1.1.1+）"; }

# 从私钥导出 base64 的 32 字节裸公钥（DER SPKI 的后 32 字节）
# 注意：DER 含 0x00，必须走文件/管道，不能用 $(...) 取值（bash 会丢 NUL）
pubkey_b64() {
  local key="$1"
  [ -f "$key" ] || die "私钥不存在：$key"
  local tmp; tmp="$(mktemp)" || die "无法创建临时文件"
  openssl pkey -in "$key" -pubout -outform DER -out "$tmp" 2>/dev/null || { rm -f "$tmp"; die "读取私钥失败：$key"; }
  tail -c 32 "$tmp" | base64 | tr -d '\n'
  rm -f "$tmp"
}

ACTION="${1:-}"; shift || true
ARCH=""
INCREMENTAL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) ARCH="$2"; shift 2 ;;
    --incremental) INCREMENTAL=1; shift ;;
    *) shift ;;
  esac
done

case "$ACTION" in
  --pubkey-only)
    need_openssl
    pubkey_b64 "$KEY_FILE"; echo
    ;;

  --sign)
    need_openssl
    [ -f "$KEY_FILE" ] || die "私钥不存在：$KEY_FILE（CI 应先从 Secrets 落盘）"
    shopt -s nullglob
    sigs=0
    skipped=0
    for so in "$ROOT"/dist/plugins/*.so; do
      [ -f "$so" ] || continue
      # 只处理**本架构**的产物：文件名带 `-<arch>.so` 的须匹配 ARCH；
      # 无架构后缀（单架构本地构建）视为本架构。
      # 不加这个筛选，`--arch x86_64` 会把 aarch64 的产物也标成 x86_64（曾踩过）。
      if [ -n "$ARCH" ]; then
        case "$so" in
          *-"$ARCH".so) ;;
          *.so)
            base="$(basename "$so")"
            case "$base" in
              *-x86_64.so|*-aarch64.so|*-arm64.so|*-amd64.so) continue ;;
            esac
            ;;
        esac
      fi
      # 增量：已有 .sig 且**比 .so 新**（即 .so 未被重建）⇒ 跳过，不重复签名
      #
      # 判据用 mtime 而非哈希：产物刚被 build_all.sh 重建时 mtime 会更新，
      # 而签名文件还是旧的 ⇒ 必须重签。这样"重建即重签、跳过即不签"自然成立。
      if [ "$INCREMENTAL" = "1" ] && [ -f "$so.sig" ] && [ "$so.sig" -nt "$so" ]; then
        skipped=$((skipped + 1))
        continue
      fi
      openssl pkeyutl -sign -rawin -inkey "$KEY_FILE" -in "$so" -out "$so.sig" \
        || die "签名失败：$so"
      sigs=$((sigs + 1))
    done
    [ $((sigs + skipped)) -gt 0 ] || die "dist/plugins 下没有 .so（先跑 Scripts/build_all.sh）"
    echo "已签名 $sigs 个插件（跳过已签名的 $skipped 个）"

    # 记录 glibc 基线：宿主据此在**下载前**拒绝"要求更新 glibc"的插件
    # （CI 镜像的 glibc 会随时间上涨，不记录就会表现为"插件莫名加载不了"）
    if [ -n "$ARCH" ]; then
      mkdir -p "$ROOT/dist/meta"
      for so in "$ROOT"/dist/plugins/*.so; do
        [ -f "$so" ] || continue
        # 同签名循环：只为本架构的产物写 meta
        base="$(basename "$so")"
        case "$base" in
          *-"$ARCH".so) ;;
          *)
            case "$base" in
              *-x86_64.so|*-aarch64.so|*-arm64.so|*-amd64.so) continue ;;
            esac
            ;;
        esac
        gmin="$(objdump -T "$so" 2>/dev/null \
          | grep -oE 'GLIBC_[0-9]+\.[0-9]+' \
          | sed 's/GLIBC_//' | sort -V | tail -1 || true)"
        printf '{"file_name":"%s","arch":"%s","glibc_min":"%s"}\n' \
          "$base" "$ARCH" "${gmin:-}" > "$ROOT/dist/meta/$base.json"
      done
      echo "已记录 glibc 基线（架构 $ARCH）"
    fi
    ;;

  --verify)
    need_openssl
    [ -f "$KEY_FILE" ] || die "私钥不存在：$KEY_FILE"
    pub="$(mktemp)"; openssl pkey -in "$KEY_FILE" -pubout -out "$pub" 2>/dev/null || die "读取私钥失败"
    n=0
    for so in "$ROOT"/dist/plugins/*.so; do
      [ -f "$so" ] || continue
      [ -f "$so.sig" ] || die "缺少签名：$so.sig"
      if openssl pkeyutl -verify -rawin -pubin -inkey "$pub" -in "$so" -sigfile "$so.sig" >/dev/null 2>&1; then
        n=$((n + 1))
      else
        rm -f "$pub"; die "验签失败：$so"
      fi
    done
    rm -f "$pub"
    echo "✅ 验签通过 $n 个插件"
    ;;

  *)
    sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
