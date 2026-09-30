//! 动作实现：`/api/p/kzwr/<action>` → [`crate::entry::action_json`] 的分发目标
//!
//! 路由形状尽量保持与迁移前的宿主内置路由一致（`/user` `/space` `/token` `/quota`
//! `/trash/empty`），这样前端与既有用法不需要跟着改；在此之上**新增**多账号所需的
//! `/accounts*` 与阈值/清理动作。
//!
//! 每个动作的返回值都可以带声明式副作用（见 `docs/PLUGIN_ABI.md`）：
//! - `alerts`：要报的告警（宿主去重落库）
//! - `resolve`：要消解的告警前缀（占用回落时清掉旧的空间预警）
//! - `config`：要写回的自配置（宿主加密落盘）
//! - `audit`：要记录的审计条目

use serde_json::{json, Value};

use crate::api::{self, Client};
use crate::state::{Account, Snapshot, Writeback};
use fn_kzwr_plugin_sdk as sdk;

/// 动作执行结果：返回给前端的 JSON + 声明式副作用
pub struct ActResult {
    pub body: Value,
    pub wb: Writeback,
    /// 要报的告警（level, message）
    pub alerts: Vec<(String, String)>,
    /// 要消解的告警前缀
    pub resolve: Vec<String>,
    /// 要写的审计条目（action, detail, ok）
    pub audit: Vec<(String, String, bool)>,
}

impl ActResult {
    fn ok(body: Value) -> Self {
        Self {
            body,
            wb: Writeback::open(),
            alerts: Vec::new(),
            resolve: Vec::new(),
            audit: Vec::new(),
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self::ok(json!({"success": false, "error": msg.into()}))
    }
    fn warn(&mut self, level: &str, msg: impl Into<String>) {
        self.alerts.push((level.to_string(), msg.into()));
    }
    fn write(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.wb.set_kv(k, v);
    }
    fn drop_key(&mut self, k: impl Into<String>) {
        self.wb.remove_key(k);
    }
    fn audit(&mut self, action: &str, detail: impl Into<String>, ok: bool) {
        self.audit.push((action.to_string(), detail.into(), ok));
    }

    /// 把声明式副作用并入返回 JSON（宿主侧 `apply_side_effects` 解析这些字段）
    pub fn into_value(mut self) -> Value {
        let mut out = match self.body.take() {
            Value::Object(m) => m,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("result".to_string(), other);
                m
            }
        };
        if !self.alerts.is_empty() {
            out.insert(
                "alerts".to_string(),
                Value::Array(
                    self.alerts
                        .iter()
                        .map(|(level, message)| json!({"level": level, "message": message}))
                        .collect(),
                ),
            );
        }
        if !self.resolve.is_empty() {
            out.insert(
                "resolve".to_string(),
                Value::Array(self.resolve.iter().cloned().map(Value::String).collect()),
            );
        }
        if let Some(c) = self.wb.commit() {
            out.insert("config".to_string(), c);
        }
        if !self.audit.is_empty() {
            out.insert(
                "audit".to_string(),
                Value::Array(
                    self.audit
                        .iter()
                        .map(|(action, detail, ok)| json!({"action": action, "detail": detail, "ok": ok}))
                        .collect(),
                ),
            );
        }
        Value::Object(out)
    }
}

/// 查询串里的布尔（宿主把 GET query 原样塞进 body，值都是字符串）
fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Some(Value::Number(n)) => n.as_u64().map(|x| x > 0).unwrap_or(false),
        _ => false,
    }
}

/// token 无效/过期时的统一提示
const TOKEN_INVALID: &str =
    "access-token 无效或已过期（酷族返回未登录状态），请在浏览器重新登录后从 Cookie 复制";

/// 空间预警告警文案前缀（用于「占用回落」时消解同前缀告警）
const QUOTA_ALERT_PREFIX: &str = "云端存储空间已用";

/// 查询单个账号的登录态与空间
struct MemberInfo {
    login: bool,
    total: u64,
    used: u64,
    plan: Option<String>,
    err: Option<String>,
}

async fn fetch_member(client: &Client, token: &str) -> MemberInfo {
    match client.get_member(token).await {
        Ok(v) if !api::member_is_login(&v) => MemberInfo {
            login: false,
            total: 0,
            used: 0,
            plan: None,
            err: None,
        },
        Ok(v) => {
            let (total, used) = api::member_space(&v);
            let plan = v
                .get("data")
                .and_then(|d| d.get("plan"))
                .and_then(|x| x.as_str())
                .map(str::to_string);
            MemberInfo {
                login: true,
                total,
                used,
                plan,
                err: None,
            }
        }
        Err(e) => MemberInfo {
            login: false,
            total: 0,
            used: 0,
            plan: None,
            err: Some(e),
        },
    }
}

/// 从请求体/查询串里挑出目标账号：`?account=<id>` 或 `{"account":"<id>"}`；
/// 没指定时只有一个账号就默认用它。
fn pick_account<'a>(snap: &'a Snapshot, req: &Value) -> Option<&'a Account> {
    let id = req
        .get("account")
        .and_then(|x| x.as_str())
        .or_else(|| req.get("body").and_then(|b| b.get("account")).and_then(|x| x.as_str()))
        .or_else(|| req.get("query").and_then(|q| q.get("account")).and_then(|x| x.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(id) = id {
        return snap.find(id);
    }
    if snap.accounts.len() == 1 {
        return snap.accounts.first();
    }
    None
}

/// 本插件支持的账号数上限
pub const MAX_ACCOUNTS: usize = 20;

// ══════════════════════════════════════════════════════════════════════════
// 账号管理（`UiBlock::Accounts` 的数据契约）
// ══════════════════════════════════════════════════════════════════════════

/// `GET /accounts`：账号列表（**永不回传明文 token**）
///
/// 默认**不发网络请求**：设置页要在离线/慢网下也能秒开并管理账号。
/// 带 `?fresh=1` 时顺带逐账号查一次登录态与占用，把结果塞进 `meta`
/// （`space` / `percent` / `state` / `plan`），供列表直接显示实时值。
pub async fn accounts_list(client: &Client, cfg: &Value, body: &Value) -> ActResult {
    let snap = Snapshot::from_cfg(cfg);
    let fresh = is_truthy(body.get("fresh"));
    let mut items = Vec::new();
    for a in &snap.accounts {
        let threshold = snap.percent_for(&a.id);
        let mut meta = serde_json::Map::new();
        meta.insert("threshold".to_string(), json!(threshold));
        if fresh {
            let info = fetch_member(client, &a.token).await;
            if info.login {
                let pct = if info.total > 0 {
                    (info.used as f64 / info.total as f64 * 100.0).round() as u64
                } else {
                    0
                };
                meta.insert(
                    "space".to_string(),
                    Value::String(format!(
                        "{} / {}",
                        api::human_bytes(info.used),
                        api::human_bytes(info.total)
                    )),
                );
                meta.insert("percent".to_string(), json!(pct));
                if let Some(p) = &info.plan {
                    meta.insert("plan".to_string(), Value::String(p.clone()));
                }
            } else if let Some(e) = &info.err {
                meta.insert("state".to_string(), Value::String(format!("查询失败：{e}")));
            } else {
                meta.insert("state".to_string(), Value::String(TOKEN_INVALID.to_string()));
            }
        }
        items.push(json!({
            "id": a.id,
            "name": a.name,
            "configured": true,
            "meta": Value::Object(meta),
        }));
    }
    let mut r = ActResult::ok(json!({
        "success": true,
        "accounts": items,
        "multiple": true,
        "max": MAX_ACCOUNTS,
        // 生效的兜底阈值（含全局覆盖），不是编译期常量 —— 否则前端/其它调用方
        // 在用户改过全局阈值后会读到 90 这个假值。
        "default_percent": snap.effective_default_percent(),
    }));
    // 遗留单 token：第一次被读到就固化成 accounts，并删除旧键（一次性迁移）
    if snap.legacy_token.is_some() && !snap.accounts.is_empty() {
        r.write(
            "accounts",
            Snapshot::accounts_json(&snap.accounts),
        );
        r.drop_key("token");
        r.audit(
            "kzwr.accounts.migrate",
            "已把单 access-token 迁移为多账号列表（默认账号）",
            true,
        );
    }
        r
}

/// `POST /accounts/add`：新增账号 `{"name","token"}`
pub async fn accounts_add(client: &Client, cfg: &Value, body: &Value) -> ActResult {
    let name = body.get("name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let token = body.get("token").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if token.is_empty() {
        return ActResult::err("access-token 不能为空");
    }
    let mut snap = Snapshot::from_cfg(cfg);
    if snap.accounts.len() >= MAX_ACCOUNTS {
        return ActResult::err(format!("最多支持 {MAX_ACCOUNTS} 个账号"));
    }
    // 同一 token 不允许重复添加（酷族按 token 认账号）
    if snap.accounts.iter().any(|a| a.token == token) {
        return ActResult::err("该 access-token 已存在于账号列表中");
    }
    // 先验 token 再落盘：避免把无效凭据写进配置后用户毫无感知
    let info = fetch_member(client, &token).await;
    if let Some(e) = &info.err {
        return ActResult::err(format!("校验 access-token 失败：{e}"));
    }
    if !info.login {
        return ActResult::err(TOKEN_INVALID);
    }
    let id = crate::state::new_id(&name, snap.accounts.len());
    let display = if name.is_empty() { format!("账号 {}", crate::state::short_id(&id)) } else { name };
    snap.accounts.push(Account { id: id.clone(), name: display.clone(), token });
    let mut r = ActResult::ok(json!({"success": true, "id": id, "message": format!("已添加账号「{display}」")}));
    r.write("accounts", Snapshot::accounts_json(&snap.accounts));
    // 遗留单 token 与多账号互斥：既然已建列表，就删掉旧键
    if snap.legacy_token.is_some() {
        r.drop_key("token");
    }
    r.audit("kzwr.accounts.add", format!("新增酷族账号「{display}」"), true);
    r
}

/// `POST /accounts/update`：改名和/或换 token `{"id","name","token"}`（token 空 = 不改）
pub async fn accounts_update(client: &Client, cfg: &Value, body: &Value) -> ActResult {
    let id = body.get("id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let name = body.get("name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let token = body.get("token").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if id.is_empty() {
        return ActResult::err("缺少账号 id");
    }
    let mut snap = Snapshot::from_cfg(cfg);
    let Some(idx) = snap.accounts.iter().position(|a| a.id == id) else {
        return ActResult::err(format!("账号不存在：{id}"));
    };
    if !token.is_empty() {
        let info = fetch_member(client, &token).await;
        if let Some(e) = &info.err {
            return ActResult::err(format!("校验 access-token 失败：{e}"));
        }
        if !info.login {
            return ActResult::err(TOKEN_INVALID);
        }
        if snap.accounts.iter().any(|a| a.id != id && a.token == token) {
            return ActResult::err("该 access-token 已被另一个账号使用");
        }
        snap.accounts[idx].token = token;
    }
    if !name.is_empty() {
        snap.accounts[idx].name = name.clone();
    }
    let display = snap.accounts[idx].name.clone();
    let mut r = ActResult::ok(json!({"success": true, "message": format!("已更新账号「{display}」")}));
    r.write("accounts", Snapshot::accounts_json(&snap.accounts));
    if snap.legacy_token.is_some() {
        r.drop_key("token");
    }
    r.audit("kzwr.accounts.update", format!("更新酷族账号「{display}」"), true);
    r
}

/// `POST /accounts/remove`：删除账号 `{"id"}`
pub async fn accounts_remove(cfg: &Value, body: &Value) -> ActResult {
    let id = body.get("id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if id.is_empty() {
        return ActResult::err("缺少账号 id");
    }
    let mut snap = Snapshot::from_cfg(cfg);
    let Some(pos) = snap.accounts.iter().position(|a| a.id == id) else {
        return ActResult::err(format!("账号不存在：{id}"));
    };
    let name = snap.accounts[pos].name.clone();
    snap.accounts.remove(pos);
    let mut r = ActResult::ok(json!({"success": true, "message": format!("已删除账号「{name}」")}));
    r.write("accounts", Snapshot::accounts_json(&snap.accounts));
    r.drop_key(crate::state::percent_key(&id));
    r.audit("kzwr.accounts.remove", format!("删除酷族账号「{name}」"), true);
    r
}

/// `POST /accounts/percent`：设置某账号的空间预警阈值 `{"id","percent"}`
pub async fn accounts_percent(cfg: &Value, body: &Value) -> ActResult {
    let id = body.get("id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if id.is_empty() {
        return ActResult::err("缺少账号 id");
    }
    let snap = Snapshot::from_cfg(cfg);
    if snap.find(&id).is_none() {
        return ActResult::err(format!("账号不存在：{id}"));
    }
    let Some(raw) = body.get("percent").or_else(|| body.get("value")) else {
        return ActResult::err("缺少 percent");
    };
    let n = raw
        .as_u64()
        .or_else(|| raw.as_str().and_then(|s| s.trim().parse::<u64>().ok()));
    let Some(n) = n else { return ActResult::err("阈值必须是 0-100 的整数") };
    // 越界一律**拒绝**，不悄悄夹到 100：夹住会让提示与实际存储自相矛盾
    // （用户填 150 → 看到「已设为 150%」→ 实际存的是 100）。
    if n > 100 {
        return ActResult::err(format!("阈值必须是 0-100 的整数（当前 {n}）"));
    }
    let mut r = ActResult::ok(json!({"success": true, "percent": n, "message": format!("阈值已设为 {n}%（0 = 关闭预警）")}));
    // **0 必须显式写入，不能删键**：删键的含义是「未单独设置」= 回落到全局兜底值，
    // 那样用户以为关闭了预警，实际又按全局阈值开始报警。
    r.write(crate::state::percent_key(&id), n.to_string());
    r.audit("kzwr.quota.save", format!("账号 {id} 空间预警阈值设为 {n}%"), true);
    r
}

/// `GET /accounts/percent`：回显某账号阈值（`?account=<id>`）
pub async fn accounts_percent_echo(cfg: &Value, body: &Value) -> ActResult {
    let snap = Snapshot::from_cfg(cfg);
    let id = body
        .get("account")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() {
        return ActResult::ok(json!({"value": crate::state::DEFAULT_PERCENT, "configured": snap.default_percent.is_some()}));
    }
    if snap.find(&id).is_none() {
        return ActResult::err(format!("账号不存在：{id}"));
    }
    let p = snap.percent_for(&id);
    ActResult::ok(json!({
        "value": p,
        "configured": snap.percents.contains_key(&id) || snap.default_percent.is_some(),
        "hint": if p == 0 { "当前已关闭预警（填 1-100 可开启）" } else { "0 = 关闭该账号的预警" },
    }))
}

// ══════════════════════════════════════════════════════════════════════════
// 单账号查询与遗留兼容
// ══════════════════════════════════════════════════════════════════════════

/// `GET /user`：账号信息（迁移前 `/api/p/kzwr/user` 的形状 + 多账号扩展）
pub async fn user(client: &Client, cfg: &Value, body: &Value) -> ActResult {
    let snap = Snapshot::from_cfg(cfg);
    if snap.accounts.is_empty() {
        return ActResult::ok(json!({
            "success": false,
            "configured": false,
            "error": "未配置 access-token：请先在下方添加酷族账号",
        }));
    }
    // 指定账号（或只有 1 个账号时默认它）→ 单账号详情
    if let Some(a) = pick_account(&snap, &json!({"body": body})) {
        let info = fetch_member(client, &a.token).await;
        if !info.login {
            let mut r = ActResult::ok(json!({
                "success": false,
                "configured": true,
                "account": a.id,
                "name": a.name,
                "error": info.err.clone().unwrap_or_else(|| TOKEN_INVALID.to_string()),
                "login": false,
            }));
            if info.err.is_none() {
                r.warn("warn", TOKEN_INVALID);
            }
            return r;
        }
        let pct = if info.total > 0 {
            (info.used as f64 / info.total as f64 * 100.0).round() as u64
        } else {
            0
        };
        return ActResult::ok(json!({
            "success": true,
            "configured": true,
            "account": a.id,
            "name": a.name,
            "login": true,
            "total": info.total,
            "used": info.used,
            "percent": pct,
            "plan": info.plan,
            "threshold": snap.percent_for(&a.id),
        }));
    }
    // 多账号且未指定 → 汇总
    let mut per = Vec::new();
    for a in &snap.accounts {
        let info = fetch_member(client, &a.token).await;
        let pct = if info.total > 0 {
            (info.used as f64 / info.total as f64 * 100.0).round() as u64
        } else {
            0
        };
        per.push(json!({
            "id": a.id,
            "name": a.name,
            "login": info.login,
            "total": info.total,
            "used": info.used,
            "percent": pct,
            "threshold": snap.percent_for(&a.id),
            "error": info.err,
        }));
    }
    ActResult::ok(json!({"success": true, "configured": true, "count": per.len(), "users": per}))
}

/// `GET /space`：空间用量（`UiBlock::Metric` 的动态值；`?account=<id>` 指定账号）
///
/// 响应保持迁移前的 `{value,hint,percent,error}` 形状，另加 `accounts` 汇总。
pub async fn space(client: &Client, cfg: &Value, body: &Value) -> ActResult {
    let snap = Snapshot::from_cfg(cfg);
    if snap.accounts.is_empty() {
        return ActResult::ok(json!({
            "value": "未配置",
            "hint": "添加酷族账号后可查看云端空间",
            "percent": 0,
            "error": null,
        }));
    }
    // 多账号且未指定：整体（各账号求和）+ 每项明细
    if pick_account(&snap, &json!({"body": body})).is_none() && snap.accounts.len() > 1 {
        let mut total_all = 0u64;
        let mut used_all = 0u64;
        let mut items = Vec::new();
        let mut invalid = 0usize;
        for a in &snap.accounts {
            let info = fetch_member(client, &a.token).await;
            if !info.login {
                invalid += 1;
                items.push(json!({"name": a.name, "error": info.err.or_else(|| Some(TOKEN_INVALID.to_string()))}));
                continue;
            }
            total_all += info.total;
            used_all += info.used;
            let pct = if info.total > 0 { (info.used as f64 / info.total as f64 * 100.0).round() as u64 } else { 0 };
            items.push(json!({"name": a.name, "value": format!("{} / {}", api::human_bytes(info.used), api::human_bytes(info.total)), "percent": pct}));
        }
        let pct = if total_all > 0 { (used_all as f64 / total_all as f64 * 100.0).round() as u64 } else { 0 };
        return ActResult::ok(json!({
            "value": format!("{} / {}", api::human_bytes(used_all), api::human_bytes(total_all)),
            "hint": format!("共 {} 个账号 · 已用 {pct}%{}", snap.accounts.len(), if invalid > 0 { format!("（{invalid} 个读取失败）") } else { String::new() }),
            "percent": pct,
            "accounts": items,
            "error": null,
        }));
    }
    let Some(a) = pick_account(&snap, &json!({"body": body})) else {
        return ActResult::ok(json!({"value": "未配置", "hint": "添加酷族账号后可查看云端空间", "percent": 0, "error": null}));
    };
    let info = fetch_member(client, &a.token).await;
    if info.login {
        let pct = if info.total > 0 { (info.used as f64 / info.total as f64 * 100.0).round() as u64 } else { 0 };
        ActResult::ok(json!({
            "value": format!("{} / {}", api::human_bytes(info.used), api::human_bytes(info.total)),
            "hint": format!(
                "已用 {pct}%{}",
                info.plan.as_ref().map(|p| format!(" · {p}")).unwrap_or_default()
            ),
            "percent": pct,
            "account": a.id,
            "error": null,
        }))
    } else {
        ActResult::ok(json!({
            "value": "登录态失效",
            "hint": info.err.unwrap_or_else(|| TOKEN_INVALID.to_string()),
            "percent": 0,
            "account": a.id,
            "error": null,
        }))
    }
}

/// `POST /token`：**遗留兼容** —— 等价于新增（或替换唯一）账号
///
/// 迁移前的前端/脚本会 POST `/token`。新链路走 `/accounts/add`；这里保留入口，
/// 避免外部用法一次改完。
pub async fn token_save(client: &Client, cfg: &Value, body: &Value) -> ActResult {
    let token = body
        .get("access_token")
        .or_else(|| body.get("token"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if token.is_empty() {
        return ActResult::err("access-token 不能为空");
    }
    let snap = Snapshot::from_cfg(cfg);
    let name = match snap.accounts.first() {
        Some(a) if snap.accounts.len() == 1 => a.name.clone(),
        _ => "默认账号".to_string(),
    };
    accounts_add(client, cfg, &json!({"name": name, "token": token})).await
}

/// `GET /token`：回显是否已配置（密钥**永不回传明文**）
pub async fn token_echo(cfg: &Value) -> ActResult {
    let snap = Snapshot::from_cfg(cfg);
    let configured = snap.account_count() > 0;
    ActResult::ok(json!({
        "value": null,
        "configured": configured,
        "count": snap.account_count(),
        "hint": if configured {
            format!("已配置 {} 个账号（增删改请在账号列表操作）", snap.account_count())
        } else {
            "未配置；浏览器登录酷族后从 Cookie 复制 access-token".to_string()
        },
    }))
}

/// `POST /quota`：**遗留兼容** —— 设置全局兜底阈值
pub async fn quota_save(cfg: &Value, body: &Value) -> ActResult {
    let raw = body.get("percent").or_else(|| body.get("value"));
    let Some(n) = raw.and_then(|v| v.as_u64()).or_else(|| raw.and_then(|v| v.as_str()).and_then(|s| s.trim().parse::<u64>().ok())) else {
        return ActResult::err("percent 必须是 0-100 的整数");
    };
    let snap = Snapshot::from_cfg(cfg);
    if n > 100 {
        return ActResult::err(format!("percent 必须是 0-100 的整数（当前 {n}）"));
    }
    let mut r = ActResult::ok(json!({"success": true, "percent": n, "message": format!("兜底阈值已设为 {n}%（0 = 关闭预警）")}));
    // 同 `/accounts/percent`：0 表示「显式关闭」，必须写进 `percent` 键。
    // 旧实现用 `drop_key` 表达 0，与「从未设置过」无法区分 → 关闭无效。
    r.write("percent", n.to_string());
    // 只有一个账号时把该账号的阈值也一并设上：否则用户改了「阈值」却发现
    if snap.accounts.len() == 1 {
        if let Some(a) = snap.accounts.first() {
            r.write(crate::state::percent_key(&a.id), n.to_string());
        }
    }
    r.audit("kzwr.quota.save", format!("空间预警阈值设为 {n}%"), true);
    r
}

/// `GET /quota`：回显阈值（兜底值；按账号阈值看 `/accounts/percent`）
pub async fn quota_echo(cfg: &Value) -> ActResult {
    let snap = Snapshot::from_cfg(cfg);
    let p = snap.default_percent.unwrap_or(crate::state::DEFAULT_PERCENT);
    ActResult::ok(json!({
        "value": p,
        "configured": snap.default_percent.is_some(),
        "hint": if snap.default_percent.is_some() { "作为未单独设置阈值账号的兜底值" } else { "尚未设置，未单独配置的账号使用默认 90%" },
    }))
}

// ══════════════════════════════════════════════════════════════════════════
// 回收站
// ══════════════════════════════════════════════════════════════════════════

/// 一轮清理的统计
#[derive(Default)]
struct TrashOutcome {
    emptied: usize,
    kept: usize,
    unknown_age: usize,
    total_bytes: u64,
    reason: Option<String>,
}

/// 带门槛的清空：先查回收站占用，未达 `max_gb` 直接跳过；按 `min_age_days` 保留新条目
async fn empty_trash_gated(
    client: &Client,
    token: &str,
    max_gb: u64,
    min_age_days: u64,
) -> Result<TrashOutcome, String> {
    let mut out = TrashOutcome::default();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    for round in 0..200u32 {
        let v = client.get_trash(token, 1).await?;
        if round == 0 {
            crate::log::debug(format!(
                "回收站首页原始响应: {}",
                api::truncate_json(&v, 1200)
            ));
        }
        let items = api::trash_items(&v);
        if items.is_empty() {
            if round == 0 {
                out.reason = Some("回收站为空".to_string());
            }
            return Ok(out);
        }
        // 占用门槛只在首轮判断（首页采样；后续轮次已在执行清理）
        if round == 0 {
            out.total_bytes = items.iter().filter_map(api::trash_item_size).sum();
            if max_gb > 0 && out.total_bytes < max_gb * 1024 * 1024 * 1024 {
                out.kept = items.len();
                out.reason = Some(format!(
                    "回收站占用 {} MB 不足 {} GB，本次未清空",
                    out.total_bytes / (1024 * 1024),
                    max_gb
                ));
                return Ok(out);
            }
        }
        // 年龄门槛：解析不出时间的条目保守保留
        let mut deletable: Vec<Value> = Vec::new();
        for it in &items {
            match api::trash_item_deleted_ms(it) {
                Some(ts) => {
                    let age_days = (now_ms - ts).max(0) / 86_400_000;
                    if min_age_days == 0 || age_days as u64 >= min_age_days {
                        deletable.push(it.clone());
                    } else {
                        out.kept += 1;
                    }
                }
                None => {
                    if min_age_days == 0 {
                        deletable.push(it.clone());
                    } else {
                        out.unknown_age += 1;
                        out.kept += 1;
                    }
                }
            }
        }
        if deletable.is_empty() {
            out.reason = Some(if out.unknown_age > 0 {
                format!("本页无可删除条目（{} 项无法解析删除时间，已保守保留）", out.unknown_age)
            } else {
                "剩余条目均未达到最小保留天数".to_string()
            });
            return Ok(out);
        }
        let n = deletable.len();
        api::delete_trash_items(client, token, &deletable)
            .await
            .map_err(|e| format!("删除回收站条目失败: {e}"))?;
        out.emptied += n;
    }
    Ok(out)
}

/// `POST /trash/empty`：手动清空回收站
///
/// 请求体可选 `{"account":"<id>"}`；**不指定则清空全部账号**（迁移前只有一个
/// token，现在必须显式，否则用户以为清了其实只清了一个）。
pub async fn trash_empty(cfg: &Value, body: &Value) -> ActResult {
    let client = Client::new();
    let snap = Snapshot::from_cfg(cfg);
    if snap.accounts.is_empty() {
        return ActResult::err("未配置 access-token：请先添加酷族账号");
    }
    let targets: Vec<&Account> = match pick_account(&snap, &json!({"body": body})) {
        Some(a) => vec![a],
        None => snap.accounts.iter().collect(),
    };
    let mut total = 0usize;
    let mut details = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    // 多账号逐个清空可能耗时较久（每账号要翻页删条目），上报进度让用户看到进展。
    // 无能力表时 `progress` 是安全空操作，不影响老宿主上的行为。
    let n = targets.len();
    sdk::host::progress("清空回收站", 0, n as u64, "开始");
    for (i, a) in targets.iter().enumerate() {
        sdk::host::progress(
            "清空回收站",
            i as u64,
            n as u64,
            &format!("正在处理账号「{}」", a.name),
        );
        match empty_trash_gated(&client, &a.token, 0, 0).await {
            Ok(o) => {
                total += o.emptied;
                details.push(json!({
                    "account": a.id,
                    "name": a.name,
                    "emptied": o.emptied,
                    "kept": o.kept,
                    "reason": o.reason,
                }));
            }
            Err(e) => errors.push(format!("{}：{e}", a.name)),
        }
    }
    sdk::host::progress("清空回收站", n as u64, n as u64, "完成");
    let mut r = if errors.is_empty() {
        ActResult::ok(json!({
            "success": true,
            "emptied": total,
            "message": if total == 0 {
                "回收站已为空，无需清理".to_string()
            } else {
                format!("已物理删除 {total} 个条目")
            },
            "accounts": details,
        }))
    } else {
        ActResult::ok(json!({
            "success": total > 0,
            "emptied": total,
            "accounts": details,
            "error": format!("部分账号清理失败：{}", errors.join("；")),
        }))
    };
    r.audit(
        "kzwr.trash.empty",
        format!("手动清空云端回收站：{} 个账号，删除 {total} 项", targets.len()),
        errors.is_empty(),
    );
    r
}

/// `GET /trash`：回收站概览（每个账号一页的条目数与占用）
pub async fn trash_overview(cfg: &Value, body: &Value) -> ActResult {
    let client = Client::new();
    let snap = Snapshot::from_cfg(cfg);
    if snap.accounts.is_empty() {
        return ActResult::err("未配置 access-token：请先添加酷族账号");
    }
    let targets: Vec<&Account> = match pick_account(&snap, &json!({"body": body})) {
        Some(a) => vec![a],
        None => snap.accounts.iter().collect(),
    };
    let mut items = Vec::new();
    for a in targets {
        match client.get_trash(&a.token, 1).await {
            Ok(v) => {
                let list = api::trash_items(&v);
                let bytes: u64 = list.iter().filter_map(api::trash_item_size).sum();
                items.push(json!({
                    "account": a.id,
                    "name": a.name,
                    "count": list.len(),
                    "bytes": bytes,
                    "size": api::human_bytes(bytes),
                    "page": v.get("data").and_then(|d| d.get("page")).cloned().unwrap_or(Value::Null),
                    "totalPage": v.get("data").and_then(|d| d.get("totalPage")).cloned().unwrap_or(Value::Null),
                    "items": list,
                }));
            }
            Err(e) => items.push(json!({"account": a.id, "name": a.name, "error": e})),
        }
    }
    ActResult::ok(json!({"success": true, "accounts": items}))
}

// ══════════════════════════════════════════════════════════════════════════
// 生命周期事件
// ══════════════════════════════════════════════════════════════════════════

/// 一次空间巡检：逐账号查登录态与占用，产出告警/消解声明
///
/// 迁移前这里是宿主内置逻辑（含「token 失效后 restore_saved_kzwr_token」）。
/// 现在插件**不改配置**，只报告：token 只是失效而不是被证明错误，保留原值让用户
/// 重新登录即可恢复，比静默清掉凭据更稳。
async fn patrol_quota(cfg: &Value, note_startup: bool) -> ActResult {
    let client = Client::new();
    let snap = Snapshot::from_cfg(cfg);
    let mut r = ActResult::ok(json!({"success": true, "checked": snap.accounts.len()}));
    if snap.accounts.is_empty() {
        r.body = json!({"success": true, "checked": 0, "reason": "未配置账号"});
        return r;
    }
    let mut invalid_names: Vec<String> = Vec::new();
    for a in &snap.accounts {
        let info = fetch_member(&client, &a.token).await;
        if !info.login {
            if info.err.is_some() {
                // 网络问题不报「token 失效」：等下一次巡检，避免误告警
                crate::log::debug(format!("账号 {} 巡检请求失败：{}", a.name, info.err.unwrap_or_default()));
            } else {
                invalid_names.push(a.name.clone());
            }
            continue;
        }
        let threshold = snap.percent_for(&a.id);
        let pct = if info.total > 0 {
            (info.used as f64 / info.total as f64 * 100.0).round() as u64
        } else {
            0
        };
        if threshold == 0 || threshold > 100 || info.total == 0 {
            continue;
        }
        let prefix = format!("{QUOTA_ALERT_PREFIX}「{}」", a.name);
        if pct >= threshold {
            r.warn(
                "warn",
                format!(
                    "{prefix} {}%（{} / {}），达到预警阈值 {}%，请及时清理以免备份失败",
                    pct,
                    api::human_bytes(info.used),
                    api::human_bytes(info.total),
                    threshold
                ),
            );
        } else {
            // 占用回落 → 消解该账号之前的空间预警（消息提醒里不留旧账）
            r.resolve.push(prefix);
        }
    }
    if !invalid_names.is_empty() {
        r.warn("warn", format!("{}：{}", TOKEN_INVALID, invalid_names.join("、")));
    }
    if note_startup && !snap.accounts.is_empty() {
        r.body = json!({"success": true, "checked": snap.accounts.len(), "note": "启动校验完成"});
    }
    r
}

/// `startup`：启动时校验各账号 token 并做一次空间巡检
pub async fn on_startup(cfg: &Value) -> ActResult {
    patrol_quota(cfg, true).await
}

/// `patrol`：定时巡检（宿主定时器/备份后触发）
pub async fn on_patrol(cfg: &Value) -> ActResult {
    patrol_quota(cfg, false).await
}

/// `reload`：配置变更。插件每次调用都从 `cfg.self_config` 现读，无内存态需要刷新。
pub async fn on_reload(cfg: &Value) -> ActResult {
    let snap = Snapshot::from_cfg(cfg);
    ActResult::ok(json!({"success": true, "accounts": snap.accounts.len(), "reloaded": true}))
}

/// `after_backup`：按**刚完成那个任务**的保留策略清理回收站
///
/// 门槛来自快照里该任务的 `recycle_max_gb` / `recycle_min_age_days`（多任务各自不同）。
/// 宿主只在任务勾选了 `empty_recycle_bin` 时才 fan-out 本事件，这里再校验一次
/// 以保持健壮（老配置没有任务 id 时退回遍历全部启用任务）。
pub async fn on_after_backup(cfg: &Value) -> ActResult {
    let client = Client::new();
    let snap = Snapshot::from_cfg(cfg);
    let mut r = ActResult::ok(json!({"success": true, "count": 0u64}));
    if snap.accounts.is_empty() {
        r.body = json!({"success": true, "count": 0u64, "reason": "未配置账号"});
        return r;
    }
    // 找触发任务（回写：找不到就扫启用任务里开了开关的）
    let task_id = cfg.get("after_backup_task").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let tasks = cfg.get("tasks").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let matched: Vec<&Value> = tasks
        .iter()
        .filter(|t| {
            let id = t.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let want = t.get("empty_recycle_bin").and_then(|x| x.as_bool()).unwrap_or(false);
            want && (task_id.is_empty() || id == task_id)
        })
        .collect();
    if matched.is_empty() {
        r.body = json!({"success": true, "count": 0u64, "reason": "本次任务未要求清空回收站"});
        return r;
    }
    let mut emptied = 0u64;
    let mut reasons: Vec<String> = Vec::new();
    for t in &matched {
        let max_gb = t.get("recycle_max_gb").and_then(|x| x.as_u64()).unwrap_or(0);
        let min_age = t.get("recycle_min_age_days").and_then(|x| x.as_u64()).unwrap_or(0);
        let tname = t.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        for a in &snap.accounts {
            match empty_trash_gated(&client, &a.token, max_gb, min_age).await {
                Ok(o) => {
                    if o.emptied > 0 {
                        reasons.push(format!(
                            "{}{}：删除 {} 项",
                            tname,
                            if snap.accounts.len() > 1 { format!(" / {}", a.name) } else { String::new() },
                            o.emptied
                        ));
                    } else if let Some(rs) = &o.reason {
                        reasons.push(format!(
                            "{}{}：{rs}",
                            tname,
                            if snap.accounts.len() > 1 { format!(" / {}", a.name) } else { String::new() }
                        ));
                    }
                    emptied += o.emptied as u64;
                }
                Err(e) => {
                    crate::log::warn(format!("任务 {tname} 账号 {} 清理回收站失败: {e}", a.name));
                    reasons.push(format!("{tname} {}：失败 {e}", a.name));
                }
            }
        }
    }
    if emptied > 0 || !reasons.is_empty() {
        r.audit(
            "kzwr.trash.auto",
            format!(
                "备份后自动清理回收站：删除 {emptied} 项{}",
                if reasons.is_empty() { String::new() } else { format!("（{}）", reasons.join("；")) }
            ),
            true,
        );
    }
    r.body = json!({
        "success": true,
        "count": emptied,
        "detail": reasons.join("；"),
    });
    r
}

/// `health_json`：一键体检（数组 + 声明式告警）
pub async fn health(cfg: &Value) -> String {
    let client = Client::new();
    let snap = Snapshot::from_cfg(cfg);
    if snap.accounts.is_empty() {
        return json!([{
            "key": "kzwr",
            "title": "增强功能",
            "status": "warn",
            "detail": "未配置 access-token（可选）：无存储空间信息与回收站清理",
            "hint": "如需存储空间预警/清空回收站：浏览器登录酷族 → F12 → Application → Cookies → www.kzwr.com → 复制 access-token，在「插件」页添加账号",
        }])
        .to_string();
    }
    let mut out = Vec::new();
    let mut alerts: Vec<Value> = Vec::new();
    let mut resolve: Vec<Value> = Vec::new();
    for a in &snap.accounts {
        let info = fetch_member(&client, &a.token).await;
        let threshold = snap.percent_for(&a.id);
        let prefix = format!("{QUOTA_ALERT_PREFIX}「{}」", a.name);
        if !info.login {
            out.push(json!({
                "key": format!("kzwr.{}", a.id),
                "title": format!("增强功能 · {}", a.name),
                "status": "fail",
                "detail": info.err.clone().unwrap_or_else(|| TOKEN_INVALID.to_string()),
                "hint": "重新从浏览器 Cookie 复制 access-token 后更新该账号",
            }));
            if info.err.is_none() {
                alerts.push(json!({"level": "warn", "message": format!("{}：{}", TOKEN_INVALID, a.name)}));
                resolve.push(Value::String(prefix));
            }
            continue;
        }
        let pct = if info.total > 0 {
            (info.used as f64 / info.total as f64 * 100.0).round() as u64
        } else {
            0
        };
        out.push(json!({
            "key": format!("kzwr.{}", a.id),
            "title": format!("增强功能 · {}", a.name),
            "status": "ok",
            "detail": format!("access-token 有效；空间已用 {}%", pct),
            "hint": null,
        }));
        let over = info.total > 0 && threshold > 0 && threshold <= 100 && pct >= threshold;
        out.push(json!({
            "key": format!("quota.{}", a.id),
            "title": format!("云端空间 · {}", a.name),
            "status": if over { "warn" } else { "ok" },
            "detail": format!(
                "{} / {}（{}%）{}",
                api::human_bytes(info.used),
                api::human_bytes(info.total),
                pct,
                if threshold > 0 { format!("，预警阈值 {}%", threshold) } else { "，未启用预警".to_string() }
            ),
            "hint": if over { Value::String("空间接近上限，建议清理回收站或扩容".to_string()) } else { Value::Null },
        }));
        if over {
            alerts.push(json!({"level": "warn", "message": format!(
                "{prefix} {}%（{} / {}），达到预警阈值 {}%，请及时清理以免备份失败",
                pct,
                api::human_bytes(info.used),
                api::human_bytes(info.total),
                threshold
            )}));
        } else if info.total > 0 {
            resolve.push(Value::String(prefix));
        }
    }
    json!({"checks": out, "alerts": alerts, "resolve": resolve}).to_string()
}
