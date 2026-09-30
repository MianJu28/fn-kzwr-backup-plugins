#!/usr/bin/env bash
#
# 构建全部插件（official / community / examples 统一处理）
#
# 用法：
#   Scripts/build_all.sh [--check] [--target <triple>] [--out <dir>]
#
#   --check          构建 + 跑单测（CI 用）
#   --target <triple> 交叉编译目标（release.yml 用，与宿主同架构）
#   --arch <arch>    产物加架构后缀（如 -x86_64），**双架构构建时必须给**，
#                    否则两个架构的同名 .so 会互相覆盖
#   --out <dir>      产物目录（默认 dist/plugins）
#   --state <file>   增量状态文件（默认**仓库根** build-state.json）；配合 --incremental
#   --incremental    **按提交跳过**：源码指纹未变的插件不再构建（需 --state）
#   --force          忽略增量，强制重建全部
#
# 为什么统一通配而不写死插件清单：新增插件不必改脚本（少一处会漂移的地方）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE=build
TARGET=""
ARCH=""
INCREMENTAL=0
FORCE=0
STATE=""
OUT="$ROOT/dist/plugins"

while [ $# -gt 0 ]; do
  case "$1" in
    --check)  MODE=check; shift ;;
    --target) TARGET="$2"; shift 2 ;;
    --out)    OUT="$2"; shift 2 ;;
    --arch)   ARCH="$2"; shift 2 ;;
    --state)  STATE="$2"; shift 2 ;;
    --incremental) INCREMENTAL=1; shift ;;
    --force)  FORCE=1; shift ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
done

mkdir -p "$OUT"

# 发现插件 crate：三个目录下声明了 cdylib 的 crate（跳过 sdk 自身）
mapfile -t CRATES < <(
  find "$ROOT/official" "$ROOT/community" "$ROOT/examples" -name Cargo.toml \
    -not -path '*/target/*' 2>/dev/null | sort |
  while IFS= read -r f; do
    grep -qE '^\s*crate-type\s*=.*cdylib' "$f" && dirname "$f"
  done
)

if [ "${#CRATES[@]}" -eq 0 ]; then
  echo "==> 未发现任何插件 crate"; exit 0
fi

# ── 增量：按插件源码指纹跳过未变的 ──────────────────────────────────────
#
# 状态文件记录「插件 id → { hash, arch, version }」：只有当**该插件**的源码指纹
# （含 sdk/ 快照/工具链）与上次**同架构**构建一致、且产物仍在时，才跳过构建与签名。
#
# 为什么按插件而不是整个仓库 commit：一次提交常只动一个插件，按仓库判断会让
# 所有插件（双架构 = 4 次编译）都重编，浪费 CI 时间。
# ⚠️ 状态放在**仓库根**而非 dist/：它受版本管理（release.yml 会提交回仓库），
#    而 dist/ 是构建产物目录 —— 任何人清理产物都会把它一起删掉。
[ -z "$STATE" ] && STATE="$ROOT/build-state.json"

# 计算当前指纹（JSON）
HASHES="$(python3 "$ROOT/Scripts/plugin_hash.py" --root "$ROOT" --json 2>/dev/null || echo '{}')"

state_get() { # state_get <plugin-id> <field>
  python3 - "$STATE" "$1" "$2" <<'PY' 2>/dev/null || echo ""
import json, sys
try:
    d = json.load(open(sys.argv[1]))
    print(d.get("plugins", {}).get(sys.argv[2], {}).get(sys.argv[3], ""))
except Exception:
    print("")
PY
}
hash_get() { # hash_get <plugin-id> <field>
  python3 - "$1" "$2" <<PY 2>/dev/null || echo ""
import json, sys
d = json.loads('''$HASHES''')
print(d.get("plugins", {}).get(sys.argv[1], {}).get(sys.argv[2], ""))
PY
}

built=0
skipped=0
BUILT_KEYS=()
for d in "${CRATES[@]}"; do
  name="${d#$ROOT/}"
  pid="$(basename "$d")"
  cur_hash="$(hash_get "$pid" hash)"
  cur_ver="$(hash_get "$pid" version)"
  # 该插件在当前架构下的产物文件名（用于判断产物是否还在）
  so_name="lib$( [ -f "$d/Cargo.toml" ] && sed -n '/^\[lib\]/,/^\[/p' "$d/Cargo.toml" | sed -n 's/^name *= *"\(.*\)"/\1/p' | head -1 )"
  [ -n "$so_name" ] || so_name="$pid"
  want="${so_name}.so"
  [ -n "$ARCH" ] && want="${so_name}-${ARCH}.so"

  # 状态键含架构：同一插件在 x86_64 与 aarch64 下是**两条独立记录**
  # （产物名不同、字节不同），合并时不能互相覆盖。
  state_key="${pid}-${ARCH}"
  if [ "$INCREMENTAL" = "1" ] && [ "$FORCE" != "1" ] && [ -n "$cur_hash" ]; then
    prev_hash="$(state_get "$state_key" hash)"
    if [ "$prev_hash" = "$cur_hash" ] && [ -f "$OUT/$want" ]; then
      echo "==> 跳过 $name（源码未变，hash ${cur_hash}，架构 ${ARCH:-native}）"
      skipped=$((skipped + 1))
      BUILT_KEYS+=("$state_key")
      continue
    fi
  fi

  echo "==> 构建 $name${TARGET:+（目标 $TARGET）}"
  # --offline 优先（NAS/CI 缓存命中时更快）；失败再回退联网
  if ! ( cd "$d" && cargo build --release --locked ${TARGET:+--target "$TARGET"} --offline ) 2>/tmp/build_err_$$; then
    if ! ( cd "$d" && cargo build --release --locked ${TARGET:+--target "$TARGET"} ); then
      echo "构建失败：$name" >&2
      tail -40 /tmp/build_err_$$ >&2 || true
      rm -f /tmp/build_err_$$
      exit 1
    fi
  fi
  rm -f /tmp/build_err_$$

  if [ "$MODE" = check ]; then
    echo "    单测…"
    ( cd "$d" && cargo test ${TARGET:+--target "$TARGET"} --offline ) \
      || { echo "单测失败：$name" >&2; exit 1; }
  fi

  # 产物：<target>/release/lib*.so（未指定 target 时是 release/）
  sub="release"
  [ -n "$TARGET" ] && sub="$TARGET/release"
  found=0
  for so in "$d/target/$sub"/lib*.so; do
    [ -f "$so" ] || continue
    # ⚠️ 双架构产物**同名**（都是 lib<name>.so）。直接 cp 到同一目录会互相覆盖，
    #    最终只剩最后一个架构的产物 —— 必须带架构后缀区分。
    base="$(basename "$so")"
    if [ -n "$ARCH" ]; then
      cp "$so" "$OUT/${base%.so}-$ARCH.so"
    else
      cp "$so" "$OUT/$base"
    fi
    found=1
  done
  [ "$found" -eq 1 ] || { echo "未找到 $name 的 .so 产物" >&2; exit 1; }
  built=$((built + 1))
  BUILT_KEYS+=("$state_key")
done

# ── 回写增量状态（仅 --incremental）────────────────────────────────────
# 只记录**本次确实产出**的插件，未参与本次构建的保持原状（避免误标"已构建"）。
if [ "$INCREMENTAL" = "1" ]; then
  mkdir -p "$(dirname "$STATE")"
  # 只更新**本次涉及**的键（BUILT_KEYS），保留其余 —— 否则跑 x86_64 那一趟
  # 会把仓库里 aarch64 的记录抹掉，导致 ARM 下次全量重编。
  #
  # 两个矩阵任务各自产出后，由 publish job 的"合并各架构的增量状态"步骤合并。
  KEYS="$(printf '%s\n' "${BUILT_KEYS[@]:-}" | grep -v '^$' || true)"
  python3 - "$STATE" "$ARCH" "$KEYS" <<PY
import json, sys
state_path, arch, keys_blob = sys.argv[1], sys.argv[2], sys.argv[3]
hashes = json.loads('''$HASHES''')
keys = [k for k in keys_blob.splitlines() if k.strip()]

try:
    state = json.load(open(state_path))
except Exception:
    state = {"plugins": {}}
state.setdefault("plugins", {})

for pid, info in hashes.get("plugins", {}).items():
    key = f"{pid}-{arch}"
    if key not in keys:
        continue  # 本次未涉及（未构建也未跳过），保持原状
    state["plugins"][key] = {
        "hash": info["hash"],
        "version": info["version"],
        "arch": arch,
    }

json.dump(state, open(state_path, "w"), ensure_ascii=False, indent=2, sort_keys=True)
print(f"增量状态已写入 {state_path}（本次更新 {len(keys)} 条，共 {len(state['plugins'])} 条）")
PY
fi

echo
echo "✅ 已构建 $built 个，跳过 $skipped 个 → $OUT"
ls -1 "$OUT" 2>/dev/null | head -20
