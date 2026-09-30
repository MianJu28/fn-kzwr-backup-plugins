# fn-kzwr-backup-plugins

**酷族备份**的官方插件仓库：承载插件源码、签名索引与发布流程。

- 用户侧：备份应用的「插件市场」从这里读取**签名索引**，安装插件。
- 开发者侧：往这里提 PR 提交插件**源码**，由 CI 构建并签名后发布。

> **本仓库不接收二进制**。发布者只提交源码 —— 这样"你 review 的源码"与
> "用户装到的字节"由 CI 强制同一，杜绝「开源一套、编译另一套」。

---

## 目录结构

```
├── index.json / index.json.sig   # CI 生成并签名的市场索引（勿手工编辑）
├── abi-layout.txt                # ABI 字段快照（与主仓库同内容，见下）
├── sdk/                          # 插件 SDK（稳定 C ABI 的**权威副本**）
├── official/                     # 官方维护的插件
├── community/                    # 社区插件（PR 提交到这里）
├── examples/                     # 示例（**不入索引**，供开发者复制起步）
└── Scripts/                      # 构建 / 签名 / 索引 / 校验
```

---

## 用户：怎么装插件

1. 打开备份应用 →「插件」页 →「插件市场」→ 点「开启」。
2. 选择插件 → 「安装」。安装前会弹出确认框，列出：
   - **源码地址**（可点击，请自行查看）
   - 发布者与审核人
   - 该插件与备份程序**同进程**运行、**可访问任意网络**

**我们能保证什么**：你装到的字节，就是索引里那个 commit 由 CI 构建并签名的那份。
**我们不保证什么**：插件不含恶意代码。沙箱能挡住磁盘上的密钥库与主口令，
但**挡不住同进程内存读取** —— 所以请只装你信任的插件。

---

## 开发者：怎么提交插件

```bash
git clone https://github.com/<you>/fn-kzwr-backup-plugins && cd fn-kzwr-backup-plugins

# 从示例起步
cp -r examples/hello community/my-plugin && cd community/my-plugin
#  改 Cargo.toml 的 name 与 [lib] name
#  改 describe.json 的 id / name / description

# 本地自测
cargo build --release --locked && cargo test

# 提 PR（**只提源码**）
git checkout -b add-my-plugin && git add community/my-plugin
git commit -m "feat: 新增插件 my-plugin" && git push -u origin add-my-plugin
```

PR 会自动跑：

| 检查 | 说明 |
|---|---|
| `security_gate.sh` | 记录依赖清单、禁 `build.rs`、禁 `panic="abort"`、`path_fields` 不得申请敏感路径 |
| 构建 + 单测 | 逐个插件，`--locked` |
| `validate_describe.py` | `describe.json` 结构与前端依赖字段 |
| `check_abi_snapshot.py` | SDK 与 `abi-layout.txt` 一致 |

合入后由维护者打 tag，CI 构建 → 签名 → 发 Release → 更新索引。

### 硬性要求

1. **依赖需在 PR 说明用途**（不再硬性限名单：合法插件用什么都可能合理。
   但冷门依赖与非 crates.io 来源会被 CI 标出，请给出理由）
2. **禁止 `build.rs`**（构建期执行任意代码，可窃取 CI 密钥）
3. **禁止 `panic = "abort"`**（会让 panic 兜底失效）
4. **入口必须用 `sdk::guard_str(|| …)` 的闭包形态**
5. 凭据必须存自己的数据目录并用宿主能力表 `seal` 加密
6. 提交 `Cargo.lock`

### ⚠️ 为什么入口必须是闭包形态（务必读）

Rust 的 `extern "C" fn` 带 `nounwind`：**panic 若发生在它的函数体里，
任何 `catch_unwind` 都拦不住**，运行时直接 `panic_cannot_unwind` → **abort 整个宿主进程**
（已实测：宿主 exit 134）。而备份程序 release 构建下自身也无法兜住这类 panic。

正确写法 —— 函数体是**普通闭包**：

```rust
extern "C" fn describe() -> *mut c_char {
    sdk::guard_str(|| json!({ /* … */ }).to_string())   // ✅ panic 可被捕获
}

extern "C" fn bad() -> *mut c_char {
    panic!("...");                                      // ❌ 直接 abort 宿主
}
```

一次 panic 会连同**所有正在进行的备份任务**一起杀掉，所以这条不是风格建议。

---

## 维护者：发布流程

```bash
# 构建全部插件（official/community/examples）
#   --arch 双架构构建**必须给**：两个架构的 .so 同名，不带后缀会互相覆盖
#   --incremental 源码指纹未变的插件跳过（不重复构建）
Scripts/build_all.sh --arch x86_64 --incremental

# 签名（CI 从 Secrets 注入私钥后运行）
#   --incremental 已签名且未被重建的产物跳过（不重复签名）
PLUGIN_SIGN_KEY_B64=... bash Scripts/sign_all.sh --sign --arch x86_64 --incremental

# 生成并签名索引（catalog_version 单调递增 —— 反回滚要求）
python3 Scripts/build_index.py --dist dist --out index.json --bump
bash Scripts/sign_index.sh index.json
```

### 增量构建：怎么判断"要不要重建"

`Scripts/plugin_hash.py` 为**每个插件**算源码指纹，由三部分共同决定：

| 输入 | 变了会怎样 |
|---|---|
| 插件自身目录（排除 `target/`） | **只**重建该插件 |
| `sdk/`（整个目录） | **全部**插件重建 |
| `abi-layout.txt` / `rust-toolchain.toml` | **全部**插件重建 |

第二、三条是刻意的：SDK 携带 `ABI_VERSION` 与 `repr(C)` 布局，
它一变，所有插件的产物都可能失效 —— 不重编会出现**静默内存错位**。

状态存在**仓库根 `build-state.json`**，并**提交回仓库** —— 它是增量的**权威基线**：

- `release.yml` 每次发布后会 `git commit` 该文件（连同 `index.json`）
- 缓存（`actions/cache`）只用于**加速产物复用**，被逐出只会退化为全量重建，**不会出错**
- 键为 `{插件id}-{架构}`：同一插件在 x86_64 与 aarch64 下是**两条独立记录**，
  两个矩阵任务各自产出后由 publish job 合并

> 放在仓库根而非 `dist/`：后者是构建产物目录，任何人清理产物都会把状态一起删掉。

`release.yml` 已把上述步骤串起来：tag → 双架构构建 → 签名 → Release → 提交索引。

### SDK 同步（自动）

`sdk/` 由 **`sync-sdk.yml` 每天定时从主仓库拉取** —— SDK 在主仓库
（`fn-kzwr-backup/plugins/sdk/`）开发，本仓库只是发布者接触它的入口。

| 情况 | 工作流行为 |
|---|---|
| SDK 无变化 | 什么都不做 |
| SDK 有变化、ABI 版本**未变** | 校验快照一致 → 自动提交 |
| SDK **ABI 版本升版** | **拒绝自动提交**，开 issue 要求人工处理 |

最后一条是刻意的：宿主对 `abi` 是**精确相等**判定，ABI 一升，
**已发布的全部插件都立即失效** —— 必须由人确认宿主 `C_ABI_VERSION` 已同步、
并重编全部插件后再合入，不能让机器人静默提交。

也可以手动触发（`workflow_dispatch`）立即同步。

### 撤销有问题的插件

编辑 `index.json` 的 `revoked` 数组后重新签名发布：

```json
"revoked": [
  { "file_name": "libbad.so", "sha256_so": "<完整 sha256>",
    "reason": "含恶意代码", "severity": "block" }
]
```

`severity: "block"` = 宿主拒绝加载并告警；`"warn"` = 仅提示。

---

## 安全模型（诚实说明）

| 我们保证 | 我们不保证 | 为什么 |
|---|---|---|
| 索引与制品由官方私钥签名 | 插件代码经过安全审计 | 签名只证明"谁发布的" |
| 你装到的字节 = 该 commit 由 CI 构建 | 插件不含恶意逻辑 | 机械闸门挡不住"能编译的恶意代码" |
| 篡改会被 sha256 + 验签发现 | 插件读不到同进程内存 | 沙箱只限文件系统与网络端口 |

**最后一道防线是人工 review + 用户自行查看源码**。本仓库的 CI 挡的是
*可机械判定*的问题（编译失败、依赖越界、ABI 不符），不是安全认证。

### 签名私钥

- 存放：GitHub Secrets 的 `PLUGIN_SIGN_KEY_B64`（base64 的 Ed25519 私钥）
- 只在 `release.yml` 注入；**`ci.yml` 绝不引用**（PR 来自不可信来源）
- 绝不入库（`.gitignore` 已排除 `.keys/`）
- 泄露后的处置：轮换宿主内置 `OFFICIAL_PUBKEYS` 并**随宿主版本发布**

### 缓存纪律（防投毒）

| 工作流 | 缓存 | 原因 |
|---|---|---|
| `ci.yml`（PR） | `actions/cache/restore` + **不调 `save`** | PR **不得写缓存** |
| `release.yml` | `actions/cache`，前缀 `release-` | 与 PR 的 `ci-` 前缀**不重叠** |

> 注意：`lookup-only: true` **不能**实现只读（官方文档：*Does not change save cache behavior*）。
> 真正只读要拆成 `restore` + 不调 `save`。

---

## 与主仓库的关系

`abi-layout.txt` 是两仓库的**共同契约**：本仓库 CI 比对 `sdk/src/lib.rs` ↔ 快照，
主仓库 CI 比对 `backend/src/plugin/abi.rs` ↔ 快照。任一侧漂移都会在自己的 CI 变红。

**改 ABI 表时**：同步更新两仓库的 `abi-layout.txt`；若是破坏性变更，
还要 +1 `C_ABI_VERSION` 并通知主仓库（宿主对 ABI 是**精确相等**判定，
升版后旧插件全部失效，必须与新宿主同步发布）。

---

## 许可

MIT（与主仓库一致）
