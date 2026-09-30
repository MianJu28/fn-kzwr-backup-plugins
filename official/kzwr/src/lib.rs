//! 酷族账号增强插件（外置 cdylib）
//!
//! 迁移前这些功能写在宿主核心里（`backend/src/plugin/builtin/kzwr.rs` +
//! `backend/src/infra/kzwr_api/`），配置也存在核心的 `cfg.kzwr.*`。本 crate 把它们
//! **全部**接了过来：厂商 HTTP 协议、凭据、阈值、回收站清理、空间预警，核心里不再
//! 留任何 kzwr 专属代码或配置字段。
//!
//! ## 与宿主的交互方式（严格 C ABI v1）
//!
//! | 方向 | 载体 |
//! |------|------|
//! | 宿主 → 插件 | `describe_json` / `available_json(cfg)` / `action_json(action, req)` / `health_json(cfg)` / `event_json(event, cfg)` |
//! | 插件 → 宿主 | **返回值里的声明式字段**：`alerts` / `resolve` / `audit`；以及**能力表回调**（`host_bind` 之后）：日志/审计/告警/进度/定时/`seal` |
//!
//! ## 配置存储（ADR-021：宿主不再代存）
//! 账号与阈值由**插件自己**保管：写进能力表给的 `own_data_dir`，
//! 整份内容经宿主 `seal` 加密（密钥在宿主手里，插件拿不到）。
//! 见 [`crate::store`]。
//!
//! **老宿主兼容**：拿不到私有目录时退回声明式 `config` 回写（宿主 `plugin_data` 代存），
//! 两条路径的键值布局相同，上层逻辑不区分。
//!
//! 凭据**永不**经过前端：`/accounts` 只回 `configured`，绝不回传 token。
//!
//! ## 阻塞式 HTTP 的位置
//!
//! 宿主把每次 FFI 调用都放在 `spawn_blocking` 线程上执行，所以插件可以安全地对自己的
//! tokio runtime 调 `block_on`（见 [`rt`]）。这是「无回调」前提下最简单可靠的写法。

mod actions;
mod api;
mod log;
mod rt;
mod state;
mod store;
mod ui;

use std::os::raw::c_char;

use serde_json::Value;

use fn_kzwr_plugin_sdk as sdk;

/// FFI 边界统一包装：函数体产出一个 JSON 字符串，这里负责变成 C 字符串。
///
/// **绝不让 panic 越过 C 边界**（那会 abort 宿主进程，等于插件把整个应用搞崩）：
/// 一切 panic 都被捕获成 `{"success":false,"error":…}`，宿主看到的是普通失败响应。
fn guard_str<F: FnOnce() -> String + std::panic::UnwindSafe>(f: F) -> *mut c_char {
    let out = std::panic::catch_unwind(f).unwrap_or_else(|e| {
        let msg = e
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| e.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "未知 panic".to_string());
        format!(
            "{{\"success\":false,\"error\":\"插件内部错误（已捕获，宿主进程安全）：{msg}\"}}"
        )
    });
    sdk::to_c_string(out)
}

// ── describe ──────────────────────────────────────────────────────────────
extern "C" fn describe() -> *mut c_char {
    guard_str(ui::describe_json)
}

// ── available ─────────────────────────────────────────────────────────────
/// 是否「可用」= 至少配置了一个账号
///
/// 与迁移前语义一致（当时看 `cfg.kzwr.access_token` 是否存在）。注意：这只影响
/// 卡片上的「已启用/未启用」徽标；**路由分发只看是否被禁用**，所以未配置时首次
/// 添加账号的界面照样可用。
extern "C" fn available(cfg: *const c_char) -> *mut c_char {
    guard_str(|| {
        let cfg = parse_cfg(unsafe { sdk::from_c_str(cfg) });
        let n = state::Snapshot::from_cfg(&cfg).account_count();
        format!("{{\"available\":{},\"accounts\":{n}}}", n > 0)
    })
}

// ── action ────────────────────────────────────────────────────────────────
/// 动作分发：`action` 为去掉 `/api/p/kzwr/` 前缀的路径，`request` = `{"body","cfg","method"}`
///
/// GET 用于回显（`echo` / `metric` / 列表），POST 用于写入。未识别的动作返回
/// `{"success":false,"error":"未知动作 …"}` —— **不返回空指针**，让前端能看到原因。
extern "C" fn action(action: *const c_char, request: *const c_char) -> *mut c_char {
    let action = unsafe { sdk::from_c_str(action) };
    let request = unsafe { sdk::from_c_str(request) };
    guard_str(move || match rt::block_on(run_action(&action, &request)) {
        Some(s) => s,
        None => "{\"success\":false,\"error\":\"插件内部错误：runtime 不可用\"}".to_string(),
    })
}

/// 实际的分发表（返回 JSON 文本；`None` 仅在 runtime 建不起来时出现）
async fn run_action(action: &str, request: &str) -> Option<String> {
    let rv: Value = serde_json::from_str(request).unwrap_or_else(|_| serde_json::json!({}));
    let cfg = rv.get("cfg").cloned().unwrap_or_else(|| Value::Object(Default::default()));
    let method = rv
        .get("method")
        .and_then(|x| x.as_str())
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let body = rv.get("body").cloned().unwrap_or_else(|| Value::Object(Default::default()));
    let client = api::Client::new();
    // 宿主已剥掉前导 `/`，这里再规范化一次，容忍 `/accounts/` 这类写法
    let key = action.trim_matches('/').to_string();
    let out: actions::ActResult = match (key.as_str(), method.as_str()) {
        // 账号 CRUD（UiBlock::Accounts 的数据契约）
        ("accounts", "GET") => actions::accounts_list(&client, &cfg, &body).await,
        ("accounts/add", "POST") => actions::accounts_add(&client, &cfg, &body).await,
        ("accounts/update", "POST") => actions::accounts_update(&client, &cfg, &body).await,
        ("accounts/remove", "POST") => actions::accounts_remove(&cfg, &body).await,
        ("accounts/percent", "POST") => actions::accounts_percent(&cfg, &body).await,
        ("accounts/percent", "GET") => actions::accounts_percent_echo(&cfg, &body).await,

        // 查询
        ("user", "GET") => actions::user(&client, &cfg, &body).await,
        ("space", "GET") => actions::space(&client, &cfg, &body).await,
        ("trash", "GET") => actions::trash_overview(&cfg, &body).await,

        // 回收站
        ("trash/empty", "POST") => actions::trash_empty(&cfg, &body).await,

        // 遗留兼容入口（迁移前的单 token / 全局阈值）
        ("token", "POST") => actions::token_save(&client, &cfg, &body).await,
        ("token", "GET") => actions::token_echo(&cfg).await,
        ("quota", "POST") => actions::quota_save(&cfg, &body).await,
        ("quota", "GET") => actions::quota_echo(&cfg).await,

        // 显式触发一次巡检（前端「检查」按钮；返回值里带 alerts/resolve 声明）
        ("check", "POST") | ("check", "GET") => actions::on_patrol(&cfg).await,

        _ => {
            return Some(
                serde_json::json!({
                    "success": false,
                    "error": format!("未知动作：{method} /{key}"),
                    "known": ["accounts", "accounts/add", "accounts/update", "accounts/remove",
                              "accounts/percent", "user", "space", "trash", "trash/empty",
                              "token", "quota", "check"],
                })
                .to_string(),
            )
        }
    };
    Some(out.into_value().to_string())
}

// ── health ────────────────────────────────────────────────────────────────
extern "C" fn health(cfg: *const c_char) -> *mut c_char {
    let cfg = parse_cfg(unsafe { sdk::from_c_str(cfg) });
    guard_str(move || rt::block_on(actions::health(&cfg)))
}

// ── event ─────────────────────────────────────────────────────────────────
/// 生命周期事件：`startup` / `patrol` / `after_backup` / `reload`
extern "C" fn event(event: *const c_char, cfg: *const c_char) -> *mut c_char {
    let event = unsafe { sdk::from_c_str(event) };
    let cfg = parse_cfg(unsafe { sdk::from_c_str(cfg) });
    guard_str(move || {
        let out = rt::block_on(async {
            match event.as_str() {
                "startup" => Some(actions::on_startup(&cfg).await),
                "patrol" => Some(actions::on_patrol(&cfg).await),
                "reload" => Some(actions::on_reload(&cfg).await),
                "after_backup" => Some(actions::on_after_backup(&cfg).await),
                _ => None,
            }
        });
        match out {
            Some(r) => r.into_value().to_string(),
            // 未知事件返回空对象（不是错误；宿主不关心返回值）
            None => "{}".to_string(),
        }
    })
}

/// 解析宿主传入的配置快照（非法/为空时退化成空对象，插件按「未配置」处理）
fn parse_cfg(raw: String) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        crate::log::warn(format!("宿主配置快照解析失败，按未配置处理：{e}"));
        Value::Object(Default::default())
    })
}

sdk::export_plugin_v1!(describe, available, action, health, Some(event));

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg_with(pairs: &[(&str, &str)]) -> Value {
        let mut m = serde_json::Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), Value::String((*v).to_string()));
        }
        json!({"self_config": Value::Object(m)})
    }

    /// 从 C 字符串取回 String 并释放（测试专用）
    fn take(p: *mut c_char) -> String {
        assert!(!p.is_null(), "插件返回了空指针");
        let s = unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { sdk::free_c_string(p) };
        s
    }

    #[test]
    fn describe_is_valid_json_with_accounts_block() {
        let v: Value = serde_json::from_str(&ui::describe_json()).unwrap();
        assert_eq!(v["id"], "kzwr");
        assert_eq!(v["kind"], "enhance");
        let blocks = v["ui"]["blocks"].as_array().unwrap();
        let types: Vec<&str> = blocks.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert!(types.contains(&"accounts"), "缺少通用账号区块: {types:?}");
        assert!(types.contains(&"metric"));
        assert!(types.contains(&"tips"));
        assert!(types.contains(&"button"));
    }

    #[test]
    fn abi_table_exports_all_five_entries() {
        let t = unsafe { &*fn_kzwr_plugin_abi_v1() };
        assert_eq!(t.abi, sdk::ABI_VERSION);
        assert_eq!(t.size as usize, std::mem::size_of::<sdk::KzwrPluginAbi>());
        assert!(t.event_json.is_some(), "kzwr 必须实现生命周期事件");
        // describe 可直接调用（无配置依赖）
        let v: Value = serde_json::from_str(&take((t.describe_json)())).unwrap();
        assert_eq!(v["id"], "kzwr");
    }

    #[test]
    fn available_false_without_accounts_true_with() {
        let empty = sdk::to_c_string(json!({"self_config": {}}).to_string());
        let v: Value = serde_json::from_str(&take(available(empty))).unwrap();
        assert_eq!(v["available"], false);
        let full = sdk::to_c_string(
            cfg_with(&[("accounts", r#"[{"id":"a1","token":"t"}]"#)])
                .to_string(),
        );
        let v: Value = serde_json::from_str(&take(available(full))).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["accounts"], 1);
    }

    #[test]
    fn accounts_list_never_leaks_tokens_and_skips_network() {
        // 不带 fresh → 不发网络请求（离线可测）；带 token 也不得回传明文
        let cfg = cfg_with(&[(
            "accounts",
            r#"[{"id":"a1","name":"主账号","token":"SUPER-SECRET"}]"#,
        )]);
        let req = json!({"body": {}, "cfg": cfg, "method": "GET"}).to_string();
        let out = rt::block_on(run_action("accounts", &req)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["accounts"][0]["name"], "主账号");
        assert_eq!(v["accounts"][0]["configured"], true);
        assert_eq!(v["accounts"][0]["meta"]["threshold"], state::DEFAULT_PERCENT);
        assert!(!out.contains("SUPER-SECRET"), "账号列表泄漏了明文 token：{out}");
    }

    #[test]
    fn accounts_list_reports_effective_default_percent() {
        // 用户改过全局阈值 → /accounts 的 default_percent 必须是**实际值**，
        // 不能是编译期常量 90（真机上踩到：设了 55 却回显 90）。
        let cfg = cfg_with(&[
            ("accounts", r#"[{"id":"a1","name":"主账号","token":"t1"}]"#),
            ("percent", "55"),
        ]);
        let req = json!({"body": {}, "cfg": cfg, "method": "GET"}).to_string();
        let v: Value = serde_json::from_str(&rt::block_on(run_action("accounts", &req)).unwrap()).unwrap();
        assert_eq!(v["default_percent"], 55, "应回显全局覆盖值");
        // 未单独设置的账号也跟随全局值
        assert_eq!(v["accounts"][0]["meta"]["threshold"], 55);

        // 没有任何设置时回落到默认 90
        let cfg2 = cfg_with(&[(
            "accounts",
            r#"[{"id":"a1","name":"主账号","token":"t1"}]"#,
        )]);
        let req2 = json!({"body": {}, "cfg": cfg2, "method": "GET"}).to_string();
        let v2: Value =
            serde_json::from_str(&rt::block_on(run_action("accounts", &req2)).unwrap()).unwrap();
        assert_eq!(v2["default_percent"], state::DEFAULT_PERCENT);
    }

    #[test]
    fn add_then_remove_writes_back_accounts_json() {
        // 无网络环境下 add 会先校验 token 而失败 —— 这里直接测 CRUD 的回写骨架：
        // 用 remove 验证「删除后重写 accounts + 清掉该账号阈值键」。
        let cfg = cfg_with(&[(
            "accounts",
            r#"[{"id":"a1","name":"一","token":"t1"},{"id":"a2","name":"二","token":"t2"}]"#,
        )]);
        let req = json!({"body": {"id": "a2"}, "cfg": cfg, "method": "POST"}).to_string();
        let v: Value = serde_json::from_str(&rt::block_on(run_action("accounts/remove", &req)).unwrap())
            .unwrap();
        assert_eq!(v["success"], true);
        let set = v["config"]["set"].as_object().unwrap();
        let kept: Value = serde_json::from_str(set["accounts"].as_str().unwrap()).unwrap();
        assert_eq!(kept.as_array().unwrap().len(), 1);
        assert_eq!(kept[0]["id"], "a1");
        assert_eq!(v["config"]["remove"].as_array().unwrap()[0], "percent-a2");
        // 审计声明存在（删除账号必须留痕）
        assert_eq!(v["audit"][0]["action"], "kzwr.accounts.remove");
    }

    #[test]
    fn percent_writeback_key_uses_hyphen_not_dot() {
        let cfg = cfg_with(&[("accounts", r#"[{"id":"a1","name":"一","token":"t1"}]"#)]);
        let req = json!({"body": {"id": "a1", "percent": 88}, "cfg": cfg, "method": "POST"}).to_string();
        let v: Value = serde_json::from_str(&rt::block_on(run_action("accounts/percent", &req)).unwrap()).unwrap();
        assert_eq!(v["success"], true);
        let keys: Vec<&String> = v["config"]["set"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert!(keys.contains(&&"percent-a1".to_string()), "实际键名 {keys:?}");
        assert!(!v.to_string().contains("percent.a1"));
    }

    #[test]
    fn unknown_action_returns_error_not_null() {
        let req = json!({"body": {}, "cfg": {}, "method": "GET"}).to_string();
        let v: Value = serde_json::from_str(&rt::block_on(run_action("nope", &req)).unwrap()).unwrap();
        assert_eq!(v["success"], false);
        assert!(v["error"].as_str().unwrap().contains("未知动作"));
    }

    #[test]
    fn malformed_request_does_not_panic_across_ffi() {
        // 非法 JSON / 空指针都要给出可用的字符串，不能崩
        let a = sdk::to_c_string("user");
        let p = action(a, std::ptr::null());
        let v: Value = serde_json::from_str(&take(p)).unwrap();
        // 无 cfg → 按未配置处理
        assert!(v["success"].as_bool() == Some(false) || v["configured"].as_bool() == Some(false));
    }

    #[test]
    fn legacy_token_migrates_to_accounts_on_read() {
        let cfg = cfg_with(&[("token", "OLD-TOKEN")]);
        let snap = state::Snapshot::from_cfg(&cfg);
        assert_eq!(snap.accounts.len(), 1);
        assert_eq!(snap.accounts[0].id, state::LEGACY_ACCOUNT_ID);
        assert_eq!(snap.accounts[0].token, "OLD-TOKEN");
        // 列表首次读取应带 accounts + 删除 token 的回写
        let req = json!({"body": {}, "cfg": cfg, "method": "GET"}).to_string();
        let v: Value = serde_json::from_str(&rt::block_on(run_action("accounts", &req)).unwrap()).unwrap();
        assert_eq!(v["config"]["remove"].as_array().unwrap()[0], "token");
        assert!(v["config"]["set"]["accounts"].is_string());
    }

    #[test]
    fn percent_key_fits_host_key_rules() {
        let id = state::new_id("张三", 3);
        let key = state::percent_key(&id);
        assert!(key.len() <= 64, "键名超长：{key}");
        assert!(
            key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "键名含宿主禁止的字符：{key}"
        );
        assert!(!key.contains('.'));
    }

    /// 0 = **显式关闭**预警，必须落盘成 `percent-<id> = "0"`，不能表达成「删键」
    ///
    /// 删键的含义是「未单独设置」→ 回落到全局兜底值。真机上踩到：用户设 0 关闭，
    /// 列表回显立刻变回全局的 55，而且下一轮巡检又开始报警。
    #[test]
    fn zero_percent_disables_and_is_persisted_explicitly() {
        let cfg = cfg_with(&[
            ("accounts", r#"[{"id":"a1","name":"一","token":"t1"}]"#),
            ("percent", "55"),
            ("percent-a1", "88"),
        ]);
        let req = json!({"body": {"id": "a1", "percent": 0}, "cfg": cfg, "method": "POST"}).to_string();
        let v: Value =
            serde_json::from_str(&rt::block_on(run_action("accounts/percent", &req)).unwrap()).unwrap();
        assert_eq!(v["success"], true);
        let set = v["config"]["set"].as_object().unwrap();
        assert_eq!(set["percent-a1"].as_str(), Some("0"), "0 必须显式写入");
        assert!(
            v["config"]["remove"].is_null() || v["config"]["remove"].as_array().unwrap().is_empty(),
            "0 不得通过删键表达： {:?}", v["config"]
        );
        // 写 0 → 读 0，不被全局 55 覆盖
        let after = cfg_with(&[
            ("accounts", r#"[{"id":"a1","name":"一","token":"t1"}]"#),
            ("percent", "55"),
            ("percent-a1", "0"),
        ]);
        assert_eq!(
            state::Snapshot::from_cfg(&after).percent_for("a1"),
            0,
            "已关闭的账号读回应是 0"
        );
    }

    /// 越界阈值必须**报错**，不能悄悄夹到 100 还回「成功」
    #[test]
    fn out_of_range_percent_is_rejected_not_clamped() {
        let cfg = cfg_with(&[("accounts", r#"[{"id":"a1","name":"一","token":"t1"}]"#)]);
        for bad in [101u64, 150, 9999] {
            let req = json!({"body": {"id": "a1", "percent": bad}, "cfg": cfg, "method": "POST"})
                .to_string();
            let v: Value =
                serde_json::from_str(&rt::block_on(run_action("accounts/percent", &req)).unwrap())
                    .unwrap();
            assert_eq!(v["success"], false, "{bad} 应被拒绝：{v}");
            assert!(v["config"].is_null(), "被拒绝的写入不得回显 config：{v}");
        }
        // 100 合法（边界）
        let req = json!({"body": {"id": "a1", "percent": 100}, "cfg": cfg, "method": "POST"}).to_string();
        let v: Value =
            serde_json::from_str(&rt::block_on(run_action("accounts/percent", &req)).unwrap()).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["config"]["set"]["percent-a1"].as_str(), Some("100"));
    }

    #[test]
    fn per_account_threshold_precedence() {
        let cfg = cfg_with(&[
            (
                "accounts",
                r#"[{"id":"a1","name":"一","token":"t1"},{"id":"a2","name":"二","token":"t2"}]"#,
            ),
            ("percent", "80"),
            ("percent-a2", "95"),
        ]);
        let snap = state::Snapshot::from_cfg(&cfg);
        assert_eq!(snap.percent_for("a1"), 80, "无单独设置应落到全局兜底");
        assert_eq!(snap.percent_for("a2"), 95, "应按账号覆盖");
        // 未知账号仍落到全局兜底（80）：兜底键的语义就是「未单独设置的账号都用它」
        assert_eq!(snap.percent_for("nope"), 80, "未知账号应落到全局兜底");
        // 没有任何兜底键时才用编译期默认 90
        let bare = state::Snapshot::from_cfg(&cfg_with(&[("accounts", r#"[{"id":"a9","token":"t"}]"#)]));
        assert_eq!(bare.percent_for("a9"), state::DEFAULT_PERCENT);
        assert_eq!(bare.percent_for("a9"), 90);
    }

    #[test]
    fn health_shape_is_object_with_checks_and_alerts() {
        let cfg = cfg_with(&[("accounts", r#"[{"id":"a1","name":"一","token":"t"}]"#)]);
        let raw = rt::block_on(actions::health(&cfg));
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert!(v["checks"].is_array());
        assert!(v["alerts"].is_array());
        assert!(v["resolve"].is_array());
        // 网络不可用时不得报「token 无效」（避免误告警），也不得有明细为空的项
        for c in v["checks"].as_array().unwrap() {
            assert!(c["key"].is_string() && c["status"].is_string());
        }
    }

    #[test]
    fn unconfigured_health_is_a_bare_array_for_backward_compat() {
        let v: Value = serde_json::from_str(&rt::block_on(actions::health(&json!({})))).unwrap();
        assert!(v.is_array());
        assert_eq!(v[0]["status"], "warn");
    }

    #[test]
    fn after_backup_without_task_returns_zero_count() {
        let cfg = json!({"self_config": {"accounts": r#"[{"id":"a1","token":"t"}]"#}, "tasks": [], "after_backup_task": null});
        let v: Value = serde_json::from_str(&rt::block_on(run_event("after_backup", &cfg))).unwrap();
        assert_eq!(v["count"], 0);
        assert!(v["reason"].is_string(), "应说明为何没清: {v}");
    }

    #[test]
    fn after_backup_task_without_flag_returns_zero_count() {
        let cfg = json!({
            "self_config": {"accounts": r#"[{"id":"a1","token":"t"}]"#},
            "tasks": [{"id": "t1", "name": "日常", "empty_recycle_bin": false, "recycle_max_gb": 0, "recycle_min_age_days": 0}],
            "after_backup_task": "t1",
        });
        let v: Value = serde_json::from_str(&rt::block_on(run_event("after_backup", &cfg))).unwrap();
        assert_eq!(v["count"], 0);
    }

    #[test]
    fn unknown_event_is_not_an_error() {
        let p = event(
            sdk::to_c_string("whatever"),
            sdk::to_c_string("{}"),
        );
        let v: Value = serde_json::from_str(&take(p)).unwrap();
        assert_eq!(v, json!({}));
    }

    /// 直接跑事件（绕开 runtime 包装，便于注入 cfg）
    async fn run_event(name: &str, cfg: &Value) -> String {
        match name {
            "after_backup" => actions::on_after_backup(cfg).await.into_value().to_string(),
            "patrol" => actions::on_patrol(cfg).await.into_value().to_string(),
            "startup" => actions::on_startup(cfg).await.into_value().to_string(),
            _ => actions::on_reload(cfg).await.into_value().to_string(),
        }
    }
}
