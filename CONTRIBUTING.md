# 贡献指南

## 提交插件（最简流程）

1. **Fork** 本仓库并 clone。
2. **复制示例**：`cp -r examples/hello community/<你的插件名>`
3. 改 `Cargo.toml`：`name`、`[lib] name`（产物名 = `lib<lib_name>.so`）
4. 改 `describe.json`：`id`（须与目录名一致）、`name`、`description`、
   建议补 `homepage`（**用户会看到它并据此判断是否安装**）
5. **本地自测**：
   ```bash
   cargo build --release --locked
   cargo test
   bash ../../Scripts/security_gate.sh ../..        # 与 CI 同一套闸门
   python3 ../../Scripts/validate_describe.py ../..
   ```
6. **提 PR**。CI 会跑闸门 + 构建 + 单测 + schema + ABI 快照。

> **只提交源码**，不要提交 `.so`。构建与签名由 CI 完成。

## 会被拒绝的情况

| 情况 | 原因 |
|---|---|
| 有 `build.rs` | 构建期执行任意代码，可窃取 CI 签名私钥 |
| 设了 `panic = "abort"` | 使 panic 兜底失效 |
| 入口未用 `guard_str` 闭包形态 | panic 会 abort 整个宿主（连同所有备份任务） |
| `path_fields` 申请 `/` 等敏感路径 | 它会进入 Landlock 白名单，等于把沙箱开到该目录 |
| 未提交 `Cargo.lock` | CI 用 `--locked` 构建，缺它无法保证依赖树一致 |
| 凭据写在明文/宿主配置 | 必须存自己的数据目录并用宿主 `seal` 加密 |

## 人工 review 看什么

CI 只挡可机械判定的问题。**依赖不做白名单**（按名字分不出好坏，且很多常见库
本身就带 `build.rs`），所以依赖是否合理由 reviewer 判断。维护者会重点看：

- **依赖清单**：CI 会列出全部依赖，非常见依赖与非 crates.io 来源会被标出。
  请确认每一个都"用得上且来源可信"。
- 网络请求发往哪里？传了什么？（插件**可访问任意网络**，这是最大的数据外泄面）
- 读写了哪些文件？是否超出声明的 `path_fields`？
- 凭据如何处理？是否有可能随日志/告警外泄？
- 是否试图绕过沙箱（如调用 `FN_KZWR_NO_SANDBOX` 相关逻辑）？

## 关于 `panic`

**这是最容易被忽略、后果最严重的一条**。请务必阅读 README 的
「为什么入口必须是闭包形态」一节 —— panic 写在 `extern "C" fn` 函数体里会
**直接 abort 宿主进程**，一次崩溃杀掉所有正在执行的备份。

## 发布（维护者）

打 tag（`v*`）即触发 `release.yml`：双架构构建 → 签名 → 建 Release → 更新索引。
撤销有问题插件见 README 的「撤销」一节。
