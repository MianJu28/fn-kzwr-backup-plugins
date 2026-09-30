#!/usr/bin/env python3
"""校验各插件的 describe.json（可用宿主类型解析 + 前端依赖的字段齐备）

刻意只做**结构性**校验（字段存在性/类型），不复制宿主 Rust 类型的全部语义 ——
真正的"能否被宿主解析"由主仓库的 contract_tests 用真实类型把关。
这里挡的是"字段名写错 / 枚举 tag 不符"这类会在 PR 阶段就暴露的问题。

用法：Scripts/validate_describe.py [仓库根]
"""
import json
import os
import re
import sys

VALID_KINDS = {"target", "enhance"}
VALID_BLOCK_TYPES = {
    "tips", "metric", "text", "number", "toggle", "button", "accounts",
}


def check(pid, path, tier):
    errs = []
    try:
        raw = open(path).read()
    except Exception as e:
        return [f"读取失败：{e}"]
    # 版本占位符：允许 {{version}}（构建期替换）
    try:
        d = json.loads(raw.replace("{{version}}", "0.0.0"))
    except Exception as e:
        return [f"JSON 解析失败：{e}"]

    for k in ("id", "name"):
        if not str(d.get(k, "")).strip():
            errs.append(f"缺少 {k}")
    if d.get("id") and d["id"] != pid:
        errs.append(f"id 与目录名不符（目录 {pid} vs 声明 {d['id']}）")
    kind = d.get("kind")
    if kind not in VALID_KINDS:
        errs.append(f"kind 必须是 {VALID_KINDS} 之一，实际 {kind!r}")

    ui = d.get("ui")
    if not isinstance(ui, dict):
        errs.append("缺少 ui 对象（否则插件页只有空卡片）")
    else:
        blocks = ui.get("blocks")
        if not isinstance(blocks, list):
            errs.append("ui.blocks 必须是数组")
        else:
            for i, b in enumerate(blocks):
                if not isinstance(b, dict):
                    errs.append(f"ui.blocks[{i}] 不是对象")
                    continue
                t = b.get("type")
                if t not in VALID_BLOCK_TYPES:
                    errs.append(
                        f"ui.blocks[{i}].type={t!r} 不是前端支持的类型 {sorted(VALID_BLOCK_TYPES)}"
                    )
                # 各类型的最小必需字段（前端会直接读）
                need = {
                    "metric": ["label"],
                    "text": ["field", "label", "action"],
                    "number": ["field", "label", "action"],
                    "toggle": ["field", "label", "action"],
                    "button": ["label", "action"],
                    "accounts": ["list", "add", "remove", "credential_field"],
                    "tips": ["text"],
                }.get(t, [])
                for f in need:
                    if f not in b:
                        errs.append(f"ui.blocks[{i}]（{t}）缺少必填字段 {f}")
        # section/order 已从宿主移除：仍带着它们会误导作者
        for stale in ("section", "order", "component"):
            if stale in ui:
                errs.append(f"ui.{stale} 已从宿主移除，请删除（不再被读取）")
    return errs


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else "."
    checked, failed = 0, 0
    for tier in ("official", "community", "examples"):
        base = os.path.join(root, tier)
        if not os.path.isdir(base):
            continue
        for pid in sorted(os.listdir(base)):
            p = os.path.join(base, pid, "describe.json")
            if not os.path.isfile(p):
                continue  # 内联在 lib.rs 的插件没有这个文件，跳过
            checked += 1
            errs = check(pid, p, tier)
            if errs:
                failed += 1
                for e in errs:
                    print(f"::error::{tier}/{pid}: {e}", file=sys.stderr)
            else:
                print(f"  ✅ {tier}/{pid}")
    if checked == 0:
        print("（未发现 describe.json，跳过）")
        return
    if failed:
        sys.exit(1)
    print(f"✅ describe.json 校验通过（{checked} 个）")


if __name__ == "__main__":
    main()
