//! 插件自管配置（多账号 + 按账号阈值）
//!
//! ## 存储位置（ADR-021：宿主不再代存）
//! 配置由**插件自己**保管：写进能力表给的 `own_data_dir`（见 [`crate::store`]），
//! 整份内容经宿主 `seal` 加密后落盘 —— 密钥在宿主手里，插件拿不到。
//!
//! **兼容**：拿不到私有目录（老宿主 / 未绑定能力表）时退回**声明式回写**，
//! 由宿主 `plugin_data` 代存（`self_config`）。两条路径的**键值布局完全相同**，
//! 因此上层逻辑不必区分。
//!
//! ## 键值布局
//!
//! | 键 | 含义 |
//! |----|------|
//! | `accounts` | JSON 数组 `[{"id","name","token"}]`（整串存一个键：token 随之加密） |
//! | `percent-<id>` | 该账号的空间用量预警阈值（%），缺省 90 |
//! | `percent` | **遗留**全局阈值（迁移前只有单 token 时代）；作为未单独设置账号的兜底 |
//! | `token` | **遗留**单 token；首次读到即迁移成一个名为「默认账号」的账号后删除 |
//!
//! 阈值默认 90%：这是迁移确立的行为（原内置固定 85%，多账号后按账号可各自设置）。

use serde_json::Value;

/// 一个酷族账号
#[derive(Debug, Clone)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub token: String,
}

/// 空间预警阈值默认值（%）
pub const DEFAULT_PERCENT: u64 = 90;

/// 遗留账号（单 token 迁移而来）的固定 id
pub const LEGACY_ACCOUNT_ID: &str = "legacy";

/// 从 `cfg.self_config` 读出的插件视角状态
#[derive(Debug, Default)]
pub struct Snapshot {
    /// 已配置的账号（含明文 token，**绝不写日志**）
    pub accounts: Vec<Account>,
    /// 按账号阈值 `percent-<id>` → 百分比（宿主键名规则不允许点号）
    ///
    /// **`Some(0)` 与「没有这个键」语义不同**：0 = 用户显式关闭该账号的预警；
    /// 缺键 = 未单独设置，回落到 `default_percent`。因此写入侧绝不能把 0 存成删键。
    pub percents: std::collections::BTreeMap<String, u64>,
    /// 全局兜底阈值（遗留 `percent` 键；`None` = 未设置 → 用 [`DEFAULT_PERCENT`]，
    /// `Some(0)` = 显式关闭所有未单独设置账号的预警）
    pub default_percent: Option<u64>,
    /// 遗留单 token（尚未迁移）
    pub legacy_token: Option<String>,
}

impl Snapshot {
    /// 从**插件自己的配置存储**读取（ADR-021 的正式路径）
    ///
    /// 拿不到私有目录时返回 `None` —— 调用方据此退回声明式回写（老宿主兼容）。
    pub fn from_store() -> Option<Self> {
        let store = crate::store::Store::open()?;
        Some(Self::from_value(&store.load()))
    }

    /// 从 `cfg`（宿主快照）构造
    ///
    /// 优先读插件自管存储；**老宿主**（无能力表/无私有目录）才退回
    /// `cfg.self_config`（宿主代存的旧路径）。
    pub fn from_cfg(cfg: &Value) -> Self {
        if let Some(s) = Self::from_store() {
            return s;
        }
        // 兼容路径：宿主代存（`self_config`）
        Self::from_value(
            cfg.get("self_config")
                .unwrap_or(&Value::Object(Default::default())),
        )
    }

    /// 从一份**键值表**构造（自管存储与 `self_config` 共用同一布局）
    ///
    /// 解析容错：`accounts` 里的非法条目（缺 id / 缺 token）逐条跳过而不是整体失败
    /// —— 手工编辑过配置文件的场景下，一个坏条目不该让全部账号消失。
    pub fn from_value(sc: &Value) -> Self {
        let get = |k: &str| -> Option<String> {
            sc.get(k).and_then(|v| v.as_str()).map(str::to_string)
        };
        let mut out = Snapshot::default();
        if let Some(s) = get("accounts") {
            if let Ok(arr) = serde_json::from_str::<Value>(&s) {
                if let Some(list) = arr.as_array() {
                    for it in list {
                        let Some(id) = it.get("id").and_then(|x| x.as_str()).map(str::to_string)
                        else {
                            continue;
                        };
                        let Some(token) =
                            it.get("token").and_then(|x| x.as_str()).map(str::to_string)
                        else {
                            continue;
                        };
                        if token.trim().is_empty() {
                            continue;
                        }
                        let name = it
                            .get("name")
                            .and_then(|x| x.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("账号 {}", short_id(&id)));
                        out.accounts.push(Account { id, name, token });
                    }
                }
            }
        }
        if let Some(p) = get("percent") {
            if let Ok(n) = p.trim().parse::<u64>() {
                out.default_percent = Some(n);
            }
        }
        // 按账号阈值：`percent-<id>`（宿主键名规则不允许点号）
        if let Some(sc) = sc.as_object() {
            for (k, v) in sc {
                if let Some(id) = k.strip_prefix("percent-") {
                    if id.is_empty() {
                        continue;
                    }
                    if let Some(s) = v.as_str() {
                        if let Ok(n) = s.trim().parse::<u64>() {
                            out.percents.insert(id.to_string(), clamp_percent(n));
                        }
                    }
                }
            }
        }
        out.legacy_token = get("token").filter(|s| !s.trim().is_empty());
        // 遗留单 token：迁移成第一个账号（回写在需要落盘的动作里带上）
        if out.accounts.is_empty() {
            if let Some(t) = out.legacy_token.clone() {
                out.accounts.push(Account {
                    id: LEGACY_ACCOUNT_ID.to_string(),
                    name: "默认账号".to_string(),
                    token: t,
                });
            }
        }
        out
    }

    /// 已配置账号数
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    /// 按 id 取账号
    pub fn find(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// 某账号的阈值：`percent-<id>` → 全局兜底 → [`DEFAULT_PERCENT`]
    pub fn percent_for(&self, id: &str) -> u64 {
        if let Some(n) = self.percents.get(id) {
            return clamp_percent(*n);
        }
        clamp_percent(self.default_percent.unwrap_or(DEFAULT_PERCENT))
    }

    /// 生效的兜底阈值：全局 `percent` → [`DEFAULT_PERCENT`]
    ///
    /// 供 `/accounts` 回显「未单独设置阈值的账号实际用哪个值」。必须用**实际值**
    /// 而不是编译期常量：用户改过全局阈值后返回常量会误导调用方。
    pub fn effective_default_percent(&self) -> u64 {
        clamp_percent(self.default_percent.unwrap_or(DEFAULT_PERCENT))
    }

    /// 账号列表序列化成 `accounts` 键的值
    pub fn accounts_json(list: &[Account]) -> String {
        let arr: Vec<Value> = list
            .iter()
            .map(|a| serde_json::json!({"id": a.id, "name": a.name, "token": a.token}))
            .collect();
        Value::Array(arr).to_string()
    }
}

/// 某账号的阈值键名（宿主键名规则：仅 `[A-Za-z0-9_-]`，**不能有点号**）
pub fn percent_key(id: &str) -> String {
    format!("percent-{id}")
}

/// 阈值合法区间（0 或 >100 = 关闭预警，与迁移前一致）
pub fn clamp_percent(n: u64) -> u64 {
    if n > 100 {
        100
    } else {
        n
    }
}

/// 生成新账号 id（无 rand 依赖：纳秒时间 + 现有数量 + 名称哈希）
pub fn new_id(name: &str, existing: usize) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let h = name
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let mut id = format!("a{:x}{:x}", now ^ h, existing);
    // 键名规则（宿主校验）：ASCII 字母/数字/_/-，长度 ≤64
    id.retain(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if id.len() > 52 {
        id.truncate(52); // `percent-` 前缀 8 字符 + id ≤ 56 < 64（宿主键名上限）
    }
    id
}

/// id 的短显示形式（用作缺省账号名）
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// 配置写入器：**优先写插件自管存储**，老宿主退回声明式回写
///
/// 两条路径的键值布局相同，所以上层只调 `set_kv` / `remove_key`，
/// 由本类型决定「落到自己的文件」还是「交给宿主代存」。
///
/// 之所以保留声明式路径：老宿主（未下发能力表 / 无 `own_data_dir`）上插件仍要能用，
/// 那时只能靠返回值里的 `config` 声明让宿主代存。
#[derive(Debug, Default)]
pub struct Writeback {
    set: Vec<(String, String)>,
    remove: Vec<String>,
    /// 自管存储（拿到私有目录时有值）；`None` = 退回声明式回写
    store: Option<crate::store::Store>,
}

impl Writeback {
    /// 打开写入器：优先自管存储，拿不到则准备走声明式回写
    pub fn open() -> Self {
        Self {
            store: crate::store::Store::open(),
            ..Default::default()
        }
    }

    pub fn set_kv(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.set.push((k.into(), v.into()));
    }
    pub fn remove_key(&mut self, k: impl Into<String>) {
        self.remove.push(k.into());
    }
    /// 是否走自管存储（诊断/测试用）
    pub fn is_self_managed(&self) -> bool {
        self.store.is_some()
    }

    /// 落盘：自管存储则「读—改—写」整份配置；否则返回 `config` 声明交给宿主
    ///
    /// 返回 `Some(json)` 表示**需要**宿主代存（老宿主路径）；
    /// 返回 `None` 表示已自行落盘（新路径）或无事可做。
    pub fn commit(&self) -> Option<Value> {
        if self.set.is_empty() && self.remove.is_empty() {
            return None;
        }
        if let Some(store) = &self.store {
            // 读—改—写：整份配置加密落盘（避免只写局部导致其余键丢失）
            let mut kv = store.load();
            let Some(obj) = kv.as_object_mut() else {
                return self.declarative();
            };
            for (k, v) in &self.set {
                obj.insert(k.clone(), Value::String(v.clone()));
            }
            for k in &self.remove {
                obj.remove(k);
            }
            if !store.save(&kv) {
                // 落盘失败：退回声明式回写，至少让宿主代存一次（不丢用户数据）
                return self.declarative();
            }
            return None;
        }
        self.declarative()
    }

    /// 声明式回写形态：`{"set":{…},"remove":[…]}`（老宿主路径）
    fn declarative(&self) -> Option<Value> {
        if self.set.is_empty() && self.remove.is_empty() {
            return None;
        }
        let mut map = serde_json::Map::new();
        for (k, v) in &self.set {
            map.insert(k.clone(), Value::String(v.clone()));
        }
        let mut j = serde_json::Map::new();
        if !map.is_empty() {
            j.insert("set".to_string(), Value::Object(map));
        }
        if !self.remove.is_empty() {
            j.insert(
                "remove".to_string(),
                Value::Array(self.remove.iter().map(|k| Value::String(k.clone())).collect()),
            );
        }
        Some(Value::Object(j))
    }
}

#[cfg(test)]
mod tests {
    //! 配置存储的**新路径**（自管 + 加密）与**兼容路径**（声明式回写）行为
    use super::*;

    /// 声明式回写形态（老宿主路径）必须与旧契约**逐字节兼容**
    ///
    /// 老宿主靠解析 `{"set":{…},"remove":[…]}` 落库；格式变了老宿主就读不到，
    /// 所以这条测试钉住 JSON 形状。
    #[test]
    fn declarative_writeback_keeps_legacy_shape() {
        let mut wb = Writeback::default(); // store=None ⇒ 走声明式
        wb.set_kv("percent", "77");
        wb.set_kv("accounts", "[]");
        wb.remove_key("token");
        let j = wb.commit().expect("应有回写声明");
        assert_eq!(j["set"]["percent"], "77");
        assert_eq!(j["set"]["accounts"], "[]");
        assert_eq!(j["remove"][0], "token");
    }

    /// 无事可做时**不产生**回写声明（避免空 set/remove 触发无意义落盘）
    #[test]
    fn empty_writeback_emits_nothing() {
        let wb = Writeback::default();
        assert!(wb.commit().is_none(), "空回写不应产生声明");
    }

    /// 键值布局解析：`from_value` 吃的是**键值表**（自管存储与 self_config 同布局）
    #[test]
    fn snapshot_parses_shared_layout() {
        let kv = serde_json::json!({
            "accounts": r#"[{"id":"a1","name":"主账号","token":"tok-1"}]"#,
            "percent": "55",
            "percent-a1": "77"
        });
        let snap = Snapshot::from_value(&kv);
        assert_eq!(snap.account_count(), 1);
        assert_eq!(snap.find("a1").unwrap().token, "tok-1");
        assert_eq!(snap.percent_for("a1"), 77, "按账号阈值优先");
        assert_eq!(snap.percent_for("other"), 55, "未单独设置回落全局");
        assert_eq!(snap.effective_default_percent(), 55);
    }

    /// `Some(0)`（显式关闭）与「缺键」（未设置）必须可区分
    ///
    /// 这是踩过的坑：曾用「删键」表达 0，导致「关闭预警」被当成「未设置」而回落默认值。
    #[test]
    fn explicit_zero_differs_from_unset() {
        let kv = serde_json::json!({ "percent": "0" });
        let snap = Snapshot::from_value(&kv);
        assert_eq!(snap.default_percent, Some(0), "显式 0 应被记住");
        assert_eq!(snap.percent_for("nobody"), 0, "0 = 关闭该账号预警");
        // 完全没设置 → 用默认 90
        let snap2 = Snapshot::from_value(&serde_json::json!({}));
        assert_eq!(snap2.default_percent, None);
        assert_eq!(snap2.percent_for("nobody"), DEFAULT_PERCENT);
    }

    /// 非法账号条目逐条跳过，不影响其余账号（手工编辑配置的场景）
    #[test]
    fn malformed_account_entries_are_skipped_not_fatal() {
        let kv = serde_json::json!({
            "accounts": r#"[{"id":"ok","name":"好","token":"t"},{"id":"no-token"},{"token":"no-id"},{"id":"blank","token":"  "}]"#
        });
        let snap = Snapshot::from_value(&kv);
        assert_eq!(snap.account_count(), 1, "只应保留合法条目");
        assert_eq!(snap.accounts[0].id, "ok");
    }

    /// 遗留单 token 自动迁移成「默认账号」
    #[test]
    fn legacy_token_migrates_into_default_account() {
        let kv = serde_json::json!({ "token": "old-token" });
        let snap = Snapshot::from_value(&kv);
        assert_eq!(snap.account_count(), 1);
        assert_eq!(snap.accounts[0].id, LEGACY_ACCOUNT_ID);
        assert_eq!(snap.accounts[0].token, "old-token");
    }
}
