#!/usr/bin/env python3
"""计算每个插件的**源码指纹**（用于"已构建签名的跳过，不重复构建"）

## 为什么按插件分别算，而不是用整个仓库的 commit

一次提交往往只动一个插件。若按整仓库 commit 判断，改一个插件会让**所有**插件
重新构建 + 重新签名（双架构下等于 4 次编译 + 4 次签名），纯属浪费。
按插件指纹则只重建真正变了的那几个。

## 指纹包含什么（任何一项变化都必须重编）

manifest 里每个插件的哈希由这些内容共同决定：

1. **插件自身目录**下所有文件（排除 target/）—— 源码、Cargo.toml、describe.json
2. **`sdk/` 整个目录** —— SDK 是 ABI 契约；它一变，所有插件的产物都可能不再适用
3. **`abi-layout.txt`** —— 布局快照变了说明 ABI 语义可能变
4. **`rust-toolchain.toml`** —— 工具链变了，产物字节可能不同

> 特别注意第 2 条：**SDK 变更会让全部插件缓存失效**，这是刻意的 ——
> SDK 携带 `ABI_VERSION` 与 `repr(C)` 布局，任何改动都必须重编全部插件，
> 否则会出现"插件表与宿主/新 SDK 不一致"的静默内存错位。

## 用法

    Scripts/plugin_hash.py            # 人类可读
    Scripts/plugin_hash.py --json     # 机器可读（CI 用）
"""
import argparse
import hashlib
import json
import os
import re
import sys

TIERS = ("official", "community", "examples")
# 参与指纹的仓库级共享输入
SHARED = ("sdk", "abi-layout.txt", "rust-toolchain.toml")


def hash_tree(root, exclude_dirs=("target", "dist", ".git")):
    """对目录下所有文件（按相对路径排序）计算确定性哈希"""
    h = hashlib.sha256()
    entries = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in exclude_dirs)
        for fn in sorted(filenames):
            full = os.path.join(dirpath, fn)
            rel = os.path.relpath(full, root)
            entries.append((rel, full))
    for rel, full in sorted(entries):
        h.update(rel.encode("utf-8"))
        h.update(b"\0")
        try:
            with open(full, "rb") as f:
                h.update(hashlib.sha256(f.read()).digest())
        except OSError:
            h.update(b"<unreadable>")
        h.update(b"\0")
    return h.hexdigest()


def package_version(crate_dir):
    toml = os.path.join(crate_dir, "Cargo.toml")
    if not os.path.isfile(toml):
        return "0.0.0"
    section = ""
    for line in open(toml):
        line = line.strip()
        if line.startswith("["):
            section = line.strip("[]").strip()
            continue
        if section == "package":
            m = re.match(r'version\s*=\s*"([^"]+)"', line)
            if m:
                return m.group(1)
    return "0.0.0"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=".")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()
    root = os.path.abspath(args.root)

    # 共享输入的指纹（SDK / 快照 / 工具链）
    shared_parts = []
    for name in SHARED:
        p = os.path.join(root, name)
        if os.path.isdir(p):
            shared_parts.append(f"{name}:{hash_tree(p)}")
        elif os.path.isfile(p):
            with open(p, "rb") as f:
                shared_parts.append(f"{name}:{hashlib.sha256(f.read()).hexdigest()}")
    shared = hashlib.sha256("|".join(shared_parts).encode()).hexdigest()

    out = {}
    for tier in TIERS:
        base = os.path.join(root, tier)
        if not os.path.isdir(base):
            continue
        for pid in sorted(os.listdir(base)):
            crate = os.path.join(base, pid)
            if not os.path.isfile(os.path.join(crate, "Cargo.toml")):
                continue
            own = hash_tree(crate)
            # 最终指纹 = 插件自身 + 共享输入（任一变化都重编）
            final = hashlib.sha256(f"{tier}|{pid}|{own}|{shared}".encode()).hexdigest()
            out[pid] = {
                "hash": final[:16],  # 取前 16 位：够用且便于人读
                "version": package_version(crate),
                "tier": tier,
                "path": f"{tier}/{pid}",
            }

    if args.json:
        json.dump({"shared": shared[:16], "plugins": out}, sys.stdout, ensure_ascii=False, indent=2)
        print()
    else:
        print(f"共享输入指纹（sdk/快照/工具链）：{shared[:16]}")
        for pid, v in out.items():
            print(f"  {pid:<20} {v['hash']}  v{v['version']}  ({v['tier']})")


if __name__ == "__main__":
    main()
