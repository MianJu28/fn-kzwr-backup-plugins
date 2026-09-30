//! 酷族备份 · 外置插件示例 —— **稳定 C ABI（v1）**
//!
//! 稳定 C ABI 是唯一的插件机制（`plugins/example-rdirect/` 的 Rust 直连已删除）。
//! | 依赖宿主 crate | 不需要（只依赖 `fn-kzwr-plugin-sdk`） | 需要（`fn-kzwr-backup`） |
//! | 宿主升级后 | **无需重编插件** | 必须重编（Rust ABI 不稳定） |
//! | 能力 | 增强类：UI 卡片 / 动作接口 / 体检 / 事件 | 全部（含自定义备份目标） |
//!
//! 构建：`bash Scripts/build_plugins.sh` → `libfn_kzwr_plugin_example.so`
//! 安装：放进 `$TRIM_PKGETC/plugins/`，在「插件」页开启「外置插件加载」后重启应用。
//!
//! ## 本示例演示的两个要点
//!
//! 1. **动作接口**：`describe.ui.blocks` 里的 `button` → `action_json`（`/hello`、`/stats`）。
//! 2. **表单读/写（`echo` 回显契约）**：`/greeting` 一个路径同时服务
//!    - **GET** = 读：返回 `{"value": …, "hint": …}`，供前端回显（宿主在信封里给 `method`）
//!    - **POST** = 写：把 `body.value` 存进插件自己的进程内状态，回 `{"success": true}`
//!
//!    只声明 `value`（静态默认值）而不实现 `echo` 的话，表单**永远显示默认值** ——
//!    用户改完再打开就"看不到自己设的值"。密钥类字段则应只回 `configured` 布尔，
//!    **绝不回明文**（见 `docs/PLUGIN_ABI.md` §4.2）。

use std::os::raw::c_char;

use fn_kzwr_plugin_sdk as sdk;
use serde_json::{json, Value};

/// describe_json：元信息 + UI 卡片（`ui.blocks` 用前端已支持的通用 schema）
extern "C" fn describe() -> *mut c_char {
    safe(|| {
        json!({
            "id": "example",
            "name": "示例外置插件（稳定 ABI）",
            "version": env!("CARGO_PKG_VERSION"),
            "kind": "enhance",
            "description": "通过稳定 C ABI（.so 动态库）加载的示例插件：宿主升级后无需重新编译",
            "caps": { "account": false, "quota": false, "recycle_bin": false, "notify": false },
            "ui": {
                "title": "示例外置插件（稳定 ABI）",
                "blocks": [
                    {
                        "type": "tips",
                        "text": "这张卡片来自外置插件，且插件只依赖**稳定 C ABI 契约**：宿主升级后不需要重新编译插件。"
                    },
                    {
                        "type": "metric",
                        "label": "加载机制",
                        "value": "稳定 C ABI v1",
                        "hint": "宿主按 ABI 版本 + 结构体长度校验，不依赖 Rust ABI"
                    },
                    {
                        // `echo: "/greeting"` = 渲染时 GET 该路径取**真实值**回填。
                        // `value` 只是描述里的静态默认值（插件并不知道自己存过什么），
                        // 不写 echo 的话表单永远显示这个默认值。
                        "type": "text",
                        "field": "value",
                        "label": "打招呼用的称呼",
                        "value": "世界",
                        "placeholder": "如：世界",
                        "action": "/greeting",
                        "button": "保存",
                        "echo": "/greeting"
                    },
                    {
                        "type": "button",
                        "label": "打个招呼",
                        "action": "/hello",
                        "danger": false,
                        "confirm": null
                    },
                    {
                        "type": "button",
                        "label": "读取配置快照（目标/任务数）",
                        "action": "/stats",
                        "danger": false,
                        "confirm": null
                    }
                ]
            }
        })
        .to_string()
    })
}

/// available_json：本示例不依赖配置，始终可用
extern "C" fn available(_cfg: *const c_char) -> *mut c_char {
    safe(|| json!({ "available": true, "reason": null }).to_string())
}

/// 示例用的进程内状态（演示「插件自己保管配置」）
///
/// 真实插件应把配置持久化到**自己的私有数据目录**（ADR-021：宿主不再代存配置，
/// 敏感内容经宿主能力表 `seal`/`unseal` 加密）；
/// 这里用 `Mutex<Option<String>>` 只为把「读回自己写过的值」这条链路演示清楚。
static GREETING: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// action_json：`action` + `request_json`（= `{"body":…,"cfg":…,"method":…}`）→ 任意 JSON
///
/// **`method` 用来区分读与写**：同一个 action 路径在 GET（渲染回显）与
/// POST（用户点保存）时都会被调用，插件必须自己分流，否则 GET 会被当成写入。
extern "C" fn action(action: *const c_char, request: *const c_char) -> *mut c_char {
    let action = unsafe { sdk::from_c_str(action) };
    let request: Value = unsafe { sdk::from_c_str(request) }
        .parse()
        .unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("POST")
        .to_ascii_uppercase();
    safe(move || match action.as_str() {
        // ── 读/写同一个路径：GET 回显、POST 保存 ──────────────────────
        //
        // `describe.ui.blocks` 里该字段声明了 `echo: "/greeting"`，
        // 前端渲染时会 GET 这里拿真实值 —— 所以 GET 分支**必须只读**。
        "greeting" => {
            if method == "GET" {
                let v = GREETING.lock().ok().and_then(|g| g.clone());
                json!({
                    // 已保存 → 回真实值；没存过 → 回 null，前端用静态默认值兜底
                    "value": v,
                    "hint": if GREETING.lock().map(|g| g.is_some()).unwrap_or(false) {
                        "已保存（改完记得点保存）"
                    } else {
                        "还没设置过，填一个试试"
                    },
                })
                .to_string()
            } else {
                let v = request
                    .get("body")
                    .and_then(|b| b.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let trimmed = v.trim().to_string();
                match GREETING.lock() {
                    Ok(mut g) => {
                        *g = if trimmed.is_empty() { None } else { Some(trimmed.clone()) };
                        sdk::json::ok_message(if trimmed.is_empty() {
                            "已清除"
                        } else {
                            "已保存"
                        })
                    }
                    Err(_) => sdk::json::error("插件内部状态不可用"),
                }
            }
        }
        "hello" => sdk::json::ok_message("你好，来自稳定 C ABI 插件的问候 👋"),
        "stats" => {
            let cfg = request.get("cfg").cloned().unwrap_or(Value::Null);
            let targets = cfg
                .get("targets")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let tasks = cfg
                .get("tasks")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let host = cfg
                .get("host_version")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            sdk::json::ok_message(&format!(
                "宿主 {host}：当前 {targets} 个目标、{tasks} 个任务（插件未重编译即可读到）"
            ))
        }
        other => sdk::json::error(&format!("未知动作：{other}")),
    })
}

/// health_json：「一键体检」自检项
extern "C" fn health(_cfg: *const c_char) -> *mut c_char {
    safe(|| {
        json!([{
            "key": "example_abi",
            "title": "示例外置插件（稳定 ABI）",
            "status": "ok",
            "detail": "稳定 C ABI v1 已加载；动作接口 /api/p/example/hello 可用",
            "hint": null
        }])
        .to_string()
    })
}

/// event_json（可选）：生命周期事件
extern "C" fn event(event: *const c_char, _cfg: *const c_char) -> *mut c_char {
    let event = unsafe { sdk::from_c_str(event) };
    safe(move || match event.as_str() {
        "startup" => json!({ "ok": true, "note": "插件已随宿主启动" }).to_string(),
        other => json!({ "ok": true, "event": other }).to_string(),
    })
}

/// 统一的 panic 兜底：插件内 panic 不允许跨越 FFI 边界
fn safe(f: impl FnOnce() -> String) -> *mut c_char {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(s) => sdk::to_c_string(s),
        Err(_) => sdk::to_c_string(sdk::json::error("插件内部错误（panic 已拦截）")),
    }
}

// 导出稳定 C ABI v1 入口（含生命周期事件）
sdk::export_plugin_v1!(describe, available, action, health, Some(event));


