//! `describe_json`：插件元信息 + 界面声明（UI Schema）
//!
//! 内容全部来自 **`describe.json`**（本目录上一级），这里只做版本占位符替换。
//!
//! 为什么是**独立 JSON 文件**而不是 Rust 字面量：宿主侧有一个契约测试
//! （`backend/src/plugin/contract_tests.rs`）会用**真实的 `AbiDescribe`/`UiBlock`
//! 类型**反序列化同一个文件。若声明写在 Rust 代码里，宿主测试就只能复制一份字面量，
//! 两边迟早漂移；共享一份文件则「插件说什么」和「宿主能否读懂」永远校验同一份内容。
//!
//! 界面全部走**通用区块**（宿主 `UiBlock`），前端不需要为本插件写任何专属组件：
//! - `accounts`：多账号增删改查 + 按账号阈值（`edit_action`）
//! - `metric` + `action`：云端空间实时用量（GET `/space`，多账号时为整体求和）
//! - `button`：手动清空回收站（`danger` + 二次确认）
//!
//! 注意：**不再声明** `text`/`number` 的 token、阈值输入块 —— 那些是单 token 时代的
//! 遗留界面，凭据改由 `accounts` 区块管理（后端仍保留 `/token`、`/quota` 动作以兼容
//! 旧脚本调用）。

/// 本插件 id（同时是宿主 `plugin_data` 的命名空间键）
pub const PLUGIN_ID: &str = "kzwr";

/// 原始声明（含 `{{version}}` 占位符）
const DESCRIBE_RAW: &str = include_str!("../describe.json");

/// `describe_json` 的内容：注入 crate 版本后返回
pub fn describe_json() -> String {
    DESCRIBE_RAW.replace("{{version}}", env!("CARGO_PKG_VERSION"))
}
