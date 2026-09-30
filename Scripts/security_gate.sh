#!/usr/bin/env bash
#
# 插件 PR 安全闸门（**可机械判定**的检查，不替代人工 review）
#
# 这里只做「一眼能判、无需读逻辑」的检查。真正拦住恶意插件的是**人工 review** ——
# 本脚本的定位是**提高作恶成本 + 挡住手误**，不是安全保证。
#
# ## 为什么**不做**依赖白名单（2026-09-29 修正）
#
# 初版按依赖名硬性白名单（community 只许 SDK + serde_json）。实测后放弃：
#
# 1. **按名字分不出好坏**：`reqwest`（联网）与 `serde_json`（解析）都是合法库，
#    合法插件用什么都有可能合理 —— 硬白名单只会逼贡献者申请例外，最终形同虚设。
# 2. **它在防"构建期代码执行"上是无效的**：白名单内的 `serde_json`、`libc`、`ring`
#    **全都自带 `build.rs`**（构建期执行代码）。既然放行的依赖本身就会执行构建脚本，
#    那"禁插件自带 build.rs、却放行带 build.rs 的依赖"并不能提高安全性。
# 3. **真正的防线是人读 Cargo.toml + 读源码**：所以把依赖清单**显式列出来**给 reviewer。
#
# 因此改为：**记录**全部依赖（含版本与来源），非常见依赖 / 非 crates.io 来源给**提示**，
# 但**不拦截**。判断权交给人。
#
# 检查项（每项都对应一次真实事故或已知攻击面）：
#   1. crate-type 必须含 cdylib            —— 否则宿主根本加载不了
#   2. 禁止 build.rs                        —— 构建期执行任意代码（可偷 CI 密钥）
#   3. 禁止 panic = "abort"                 —— 会让插件侧 catch_unwind 也失效
#   4. 依赖清单**记录**（不硬性白名单）      —— 列出依赖供人工 review 判断
#   5. 必须提交 Cargo.lock                  —— 配合 --locked 锁死依赖树
#   6. 入口必须用 guard_str 闭包形态        —— 防 panic 跨越 FFI 边界（见 §7.8）
#   7. path_fields 不得申请敏感路径         —— 否则等于把沙箱白名单开到根目录
#
# 用法：Scripts/security_gate.sh [仓库根]
set -uo pipefail

ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
fail=0
note() { printf '  %s\n' "$*"; }
bad()  { printf '::error::%s\n' "$*" >&2; fail=1; }

# 常见基础依赖：命中则不额外提示（**不代表它们安全**，只是无需特别关注）。
# 注意 `serde_json`/`libc`/`ring` 等本身就带 build.rs —— 所以这里只是"眼熟名单"。
COMMON_DEPS='fn-kzwr-plugin-sdk|serde|serde_json|tokio|reqwest|chrono|libc|log|tracing|anyhow|thiserror|url|base64|sha2|hex|rand|futures|bytes|async-trait|percent-encoding|http|http-body|hyper|ring|blake3|age|tempfile|regex|once_cell|lazy_static|parking_lot|toml|semver|uuid|time|humantime|croner|tokio-util|tokio-stream|rustls|webpki-roots|encoding_rs|mime|idna|itertools|either|pin-project-lite|smallvec|bitflags|memchr|itoa|ryu|zerocopy|zerofrom|stable_deref_trait|form_urlencoded|ipnet|tower|tower-http|tracing-subscriber|rusqlite|serde_derive|tokio-macros'

# 遍历所有插件 crate（有 Cargo.toml 且声明 cdylib 的目录，跳过 sdk 本身）
mapfile -t CRATES < <(
  find "$ROOT/official" "$ROOT/community" "$ROOT/examples" -name Cargo.toml -not -path '*/target/*' 2>/dev/null | sort
)
if [ "${#CRATES[@]}" -eq 0 ]; then
  note "未发现插件 crate（跳过）"
  exit 0
fi

for toml in "${CRATES[@]}"; do
  d="$(dirname "$toml")"
  name="$(basename "$d")"
  echo "── 检查 $name"

  # 1) crate-type 必须含 cdylib
  if ! grep -qE '^\s*crate-type\s*=.*cdylib' "$toml"; then
    bad "$name: Cargo.toml 的 [lib] crate-type 必须包含 \"cdylib\"（否则宿主无法 dlopen）"
  fi

  # 2) 禁止 build.rs（构建期任意代码执行）
  if [ -f "$d/build.rs" ]; then
    bad "$name: 不允许 build.rs —— 它在**构建期执行任意代码**，可窃取 CI 密钥或投毒产物"
  fi

  # 3) 禁止 panic = "abort"
  if grep -qE 'panic\s*=\s*"abort"' "$toml"; then
    bad "$name: 不允许 panic = \"abort\" —— 它会让插件侧 catch_unwind 失效，插件 panic 即拖垮宿主"
  fi

  # 4) 依赖清单：**记录 + 提示**，不拦截（理由见文件头）
  deps="$(
    awk '
      /^\[dependencies\]/ {in_d=1; next}
      /^\[/ {in_d=0}
      in_d && /^[A-Za-z0-9_-]+[[:space:]]*=/ {print}
    ' "$toml"
  )"
  if [ -n "$deps" ]; then
    echo "    依赖清单（供人工 review）："
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      dep="$(printf '%s' "$line" | sed -E 's/^([A-Za-z0-9_-]+).*/\1/')"
      mark=""
      if ! printf '%s' "$dep" | grep -qE "^($COMMON_DEPS)$"; then
        mark="  ← 非常见依赖，请确认用途与来源"
      fi
      case "$line" in
        *git*=*|*registry*=*) mark="$mark  ← ⚠️ 非 crates.io 来源" ;;
      esac
      printf '      %s%s\n' "$line" "$mark"
    done <<< "$deps"
    # 依赖**自身**的 build.rs 无法靠"插件不许有 build.rs"约束（那只管插件自己的脚本）。
    # 很多常见库（serde_json/libc/ring）都带 build.rs，且会在构建期执行 —— 明确提醒。
    printf '::notice::%s 的依赖在构建期可能执行各自的 build.rs（常见库多有），需一并 review\n' "$name"
  fi

  # 5) 必须提交 Cargo.lock
  if [ ! -f "$d/Cargo.lock" ]; then
    bad "$name: 缺少 Cargo.lock —— CI 用 --locked 构建，缺它无法保证依赖树一致"
  fi

  # 6) 入口必须是 guard_str 闭包形态（防 panic 跨越 FFI；详见 SDK 的 guard_str 文档）
  #    说明：这条**只做启发式提示**（不 fail），因为闭包形态有多种等价写法，
  #    机械判死会误伤；但缺了它就必须人工重点看 panic 处理。
  if ! grep -qE 'guard_str|catch_unwind' "$d/src"/*.rs 2>/dev/null; then
    printf '::warning::%s: 未发现 guard_str/catch_unwind —— 请人工确认 panic 不会跨越 FFI 边界（宿主 release 下会 abort 整个进程）\n' "$name" >&2
  fi

  # 7) path_fields 不得申请根目录或宿主敏感路径
  for json in "$d"/*.json; do
    [ -f "$json" ] || continue
    if grep -q 'path_fields' "$json"; then
      # 提取 path_fields 数组里的字符串，逐个检查
      python3 - "$json" "$name" <<'PY' || fail=1
import json, sys
path, name = sys.argv[1], sys.argv[2]
try:
    d = json.load(open(path))
except Exception:
    sys.exit(0)  # schema 由 validate_describe.py 负责

def walk(o):
    if isinstance(o, dict):
        if 'path_fields' in o and isinstance(o['path_fields'], list):
            yield from o['path_fields']
        for v in o.values():
            yield from walk(v)
    elif isinstance(o, list):
        for v in o:
            yield from walk(v)

FORBIDDEN = ('/', '/etc', '/usr', '/proc', '/root', '/var', '/vol1')
for f in walk(d):
    if not isinstance(f, str):
        continue
    if f in FORBIDDEN or f.startswith('/etc') or f.startswith('/vol1'):
        print(f'::error::{name}: path_fields 申请了敏感路径 {f!r} —— '
              f'它会进入 Landlock 白名单，等于把沙箱开到该目录', file=sys.stderr)
        sys.exit(1)
PY
    fi
  done
done

if [ "$fail" -ne 0 ]; then
  echo
  echo "安全闸门未通过 —— 请修正上述问题后重新提交。" >&2
  exit 1
fi
echo
echo "✅ 安全闸门全部通过（注意：这只挡可机械判定的问题，不等于代码安全）"
