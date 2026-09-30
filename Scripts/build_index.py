#!/usr/bin/env python3
"""生成插件市场索引（index.json）

输入布局（由 Scripts/build_all.sh + sign_all.sh 产出）：
    dist/plugins/<lib*.so>          已签名插件
    dist/plugins/<lib*.so>.sig      对应签名
    dist/meta/<lib*.so>.json        该产物的架构与 glibc 基线（sign_all.sh 生成）

插件元信息来自源码，而非产物（产物里没有可读元数据）：
    official/<id>/describe.json 或 community/<id>/describe.json
    （examples/ **不入索引** —— 示例是给开发者看的，不该出现在用户的市场列表里）

用法：
    Scripts/build_index.py --dist dist --out index.json [--bump]

    --bump  在已有 index.json 的 catalog_version 基础上 +1（反回滚要求单调递增）；
            不加则沿用已有值或从 1 开始。
"""
import argparse
import hashlib
import json
import os
import re
import sys
from datetime import datetime, timezone


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _arch_from_name(so_name):
    """从 `lib<name>-<arch>.so` 解析架构；无后缀返回 None"""
    if not so_name.endswith(".so"):
        return None
    body = so_name[:-3]
    for a in ("x86_64", "aarch64", "arm64", "amd64"):
        if body.endswith("-" + a):
            return "aarch64" if a == "arm64" else ("x86_64" if a == "amd64" else a)
    return None


def parse_cargo(crate_dir):
    """从 Cargo.toml 取 [lib] name 与 package version

    用 `[lib] name` 而非从插件 id 猜：产物名是 `lib<lib_name>.so`，
    而所有插件都以 `fn_kzwr_plugin_` 开头，靠子串匹配会把 A 的产物算到 B 头上
    （实测踩过：kzwr 的索引里混进了 example 的 .so）。
    """
    toml = os.path.join(crate_dir, "Cargo.toml")
    lib_name, version, min_host = "", "0.0.0", ""
    if not os.path.isfile(toml):
        return lib_name, version, min_host
    section = ""
    for line in open(toml):
        line = line.strip()
        if line.startswith("["):
            section = line.strip("[]").strip()
            continue
        m = re.match(r'name\s*=\s*"([^"]+)"', line)
        if m and section == "lib":
            lib_name = m.group(1)
        m = re.match(r'version\s*=\s*"([^"]+)"', line)
        if m and section == "package":
            version = m.group(1)
    return lib_name, version, min_host


def find_describes(root):
    """收集 official/ 与 community/ 下的 describe.json（排除 examples/）"""
    out = {}
    for tier in ("official", "community"):
        base = os.path.join(root, tier)
        if not os.path.isdir(base):
            continue
        for name in sorted(os.listdir(base)):
            d = os.path.join(base, name, "describe.json")
            if os.path.isfile(d):
                out[name] = (tier, d, os.path.join(base, name))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dist", default="dist")
    ap.add_argument("--out", default="index.json")
    ap.add_argument("--root", default=".")
    ap.add_argument("--bump", action="store_true")
    ap.add_argument("--abi", type=int, default=2, help="宿主 C_ABI_VERSION")
    ap.add_argument("--release-tag", default=os.environ.get("GITHUB_REF_NAME", ""))
    ap.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    args = ap.parse_args()

    plugins_dir = os.path.join(args.dist, "plugins")
    meta_dir = os.path.join(args.dist, "meta")
    if not os.path.isdir(plugins_dir):
        sys.exit(f"找不到产物目录：{plugins_dir}（先跑 Scripts/build_all.sh）")

    describes = find_describes(args.root)

    # 已有索引：用于 catalog_version 递增
    prev_version = 0
    if os.path.isfile(args.out):
        try:
            prev_version = int(json.load(open(args.out)).get("catalog_version", 0))
        except Exception:
            prev_version = 0
    catalog_version = prev_version + 1 if args.bump else (prev_version or 1)

    # 按插件 id 分组：产物文件名 → 插件 id 需要从 .so 的入口符号推断，
    # 但那要 dlopen。这里改用**更简单可靠**的约定：文件名须包含
    # describe.json 里的 crate 名（build_all.sh 产出的 lib<name>.so）。
    sha = os.environ.get("GITHUB_SHA", "")[:7]
    generated_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    plugins = []
    for pid, (tier, dpath, crate_dir) in describes.items():
        try:
            raw = open(dpath).read()
        except Exception as e:
            print(f"跳过 {pid}：describe.json 读取失败 {e}", file=sys.stderr)
            continue

        lib_name, pkg_version, _ = parse_cargo(crate_dir)
        # 替换 describe.json 的版本占位符（插件在运行时用 env!("CARGO_PKG_VERSION") 做同样的事）
        raw = raw.replace("{{version}}", pkg_version)
        try:
            d = json.loads(raw)
        except Exception as e:
            print(f"跳过 {pid}：describe.json 解析失败 {e}", file=sys.stderr)
            continue

        if not lib_name:
            print(f"跳过 {pid}：Cargo.toml 缺少 [lib] name，无法定位产物", file=sys.stderr)
            continue
        # 产物可能带架构后缀（`lib<name>-<arch>.so`，双架构构建时必需）或不带
        # （单架构本地构建）。两种都要认；同一插件按架构各出一条 version 记录。
        stem = f"lib{lib_name}"

        versions = []
        for so_name in sorted(os.listdir(plugins_dir)):
            if not so_name.endswith(".so"):
                continue
            # **精确匹配**产物名（见 parse_cargo 的说明：不能用子串）
            # 允许 `lib<name>.so` 与 `lib<name>-<arch>.so` 两种形态
            if so_name == f"{stem}.so":
                pass
            elif so_name.startswith(f"{stem}-") and so_name.endswith(".so"):
                pass
            else:
                continue
            so_path = os.path.join(plugins_dir, so_name)
            sig_path = so_path + ".sig"
            if not os.path.isfile(sig_path):
                print(f"跳过 {so_name}：缺少 .sig（未签名）", file=sys.stderr)
                continue

            meta = {}
            mpath = os.path.join(meta_dir, so_name + ".json")
            if os.path.isfile(mpath):
                meta = json.load(open(mpath))

            rel = f"dist/plugins/{so_name}"
            url = rel
            sig_url = rel + ".sig"
            if args.repo and args.release_tag:
                base = f"https://github.com/{args.repo}/releases/download/{args.release_tag}"
                url = f"{base}/{so_name}"
                sig_url = f"{base}/{so_name}.sig"

            versions.append(
                {
                    "version": d.get("version") or pkg_version,
                    "released_at": generated_at,
                    "file_name": so_name,
                    "url": url,
                    "sig_url": sig_url,
                    "sha256_so": sha256_file(so_path),
                    "sha256_sig": sha256_file(sig_path),
                    "size_bytes": os.path.getsize(so_path),
                    "abi": args.abi,
                    "arch": meta.get("arch") or _arch_from_name(so_name) or "x86_64",
                    "glibc_min": meta.get("glibc_min", ""),
                    "min_host_version": d.get("min_host_version", ""),
                    "source_commit": sha,
                    "yanked": False,
                }
            )

        if not versions:
            print(f"跳过 {pid}：没有匹配的已签名产物", file=sys.stderr)
            continue

        plugins.append(
            {
                "id": pid,
                "name": d.get("name", pid),
                "description": (d.get("description") or "")[:200],
                "homepage": d.get("homepage", ""),
                "license": d.get("license", ""),
                "publisher": d.get("publisher", ""),
                "reviewed_by": d.get("reviewed_by", ""),
                "origin": tier,
                "kind": d.get("kind", "enhance"),
                "tags": d.get("tags", []),
                "versions": versions,
            }
        )

    index = {
        "schema": 1,
        "catalog_version": catalog_version,
        "generated_at": generated_at,
        "commit": sha,
        "host_abi": args.abi,
        "plugins": plugins,
        "revoked": [],  # 撤销条目由维护者手工追加后重新签名
    }

    with open(args.out, "w") as f:
        json.dump(index, f, ensure_ascii=False, indent=2)
        f.write("\n")

    print(f"已生成 {args.out}：{len(plugins)} 个插件，catalog_version={catalog_version}")


if __name__ == "__main__":
    main()
