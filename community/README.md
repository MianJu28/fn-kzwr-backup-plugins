# community/ — 社区插件

把你开发的插件放在这里：`community/<插件名>/`。

```bash
cp -r ../examples/hello community/my-plugin
cd community/my-plugin
# 改 Cargo.toml 的 name 与 [lib] name；改 describe.json 的 id/name/description
cargo build --release --locked && cargo test
```

提 PR 后 CI 会自动跑安全闸门、构建、单测、`describe.json` 校验与 ABI 快照比对。
详见仓库根目录的 [CONTRIBUTING.md](../CONTRIBUTING.md)。

> **只提交源码**，不要提交 `.so` —— 构建与签名由 CI 完成，
> 这样"你 review 的源码"与"用户装到的字节"由 CI 强制同一。

<!-- ci-test-marker: 本行仅用于验证 PR 触发 ci.yml，验证通过后随分支删除 -->
