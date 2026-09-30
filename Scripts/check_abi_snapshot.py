#!/usr/bin/env python3
"""ABI 布局快照比对（插件仓库侧：SDK 源码 ↔ abi-layout.txt）

主仓库有一个对应的比对（宿主 abi.rs ↔ 同一份 abi-layout.txt）。
两仓库各自比对，任一侧漂移都会在自己的 CI 变红 —— 这是"拆仓库后仍能
发现跨 FFI 布局错位"的唯一保障（错位会导致调用跳到错误地址，编译期无感）。

用法：Scripts/check_abi_snapshot.py abi-layout.txt sdk/src/lib.rs
"""
import re
import sys


def snapshot(path):
    """解析快照：[结构体名] 后跟字段名若干"""
    out, cur = {}, None
    for line in open(path):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        m = re.match(r"^\[(.+)\]$", line)
        if m:
            cur = m.group(1)
            out[cur] = []
            continue
        if cur:
            out[cur].append(line)
    return out


def source_fields(path, name):
    src = open(path).read()
    m = re.search(r"pub struct " + name + r"\s*\{(.*?)\n\}", src, re.S)
    if not m:
        return None
    fields = []
    for line in m.group(1).splitlines():
        mm = re.match(r"\s*pub\s+([A-Za-z0-9_]+)\s*:", line)
        if mm:
            fields.append(mm.group(1))
    return fields


def main():
    snap_path, sdk_path = sys.argv[1], sys.argv[2]
    snap = snapshot(snap_path)
    bad = 0
    for name, want in snap.items():
        got = source_fields(sdk_path, name)
        if got is None:
            print(f"::error::SDK 中找不到结构体 {name}", file=sys.stderr)
            bad = 1
            continue
        if got != want:
            print(
                f"::error::{name} 与快照不一致（跨 FFI 布局会错位！）\n"
                f"  快照: {want}\n"
                f"  SDK : {got}\n"
                f"  规则：字段只能**尾部追加**，顺序/名称必须逐字段相同。\n"
                f"  若确为有意变更：同步更新 abi-layout.txt（两仓库同时提交）"
                f"，破坏性变更还需 +1 ABI 版本并通知主仓库。",
                file=sys.stderr,
            )
            bad = 1
        else:
            print(f"  ✅ {name}：{len(want)} 个字段一致")
    if bad:
        sys.exit(1)
    print("✅ ABI 布局快照比对通过")


if __name__ == "__main__":
    main()
