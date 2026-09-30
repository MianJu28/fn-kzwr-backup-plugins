//! **插件自管配置存储**（宿主不再代存，ADR-021）
//!
//! ## 为什么改
//! 此前 kzwr 的账号/阈值由**宿主代存**（`plugin_data["kzwr"]`，宿主 age 加密）。
//! 现在宿主不再代存插件配置，插件把键值写进自己的能力表给的 **`own_data_dir`**。
//!
//! ## 安全：敏感内容必须经宿主 `seal` 加密
//! 账号里含 **access-token**（凭据）。直接写盘就是明文落盘 —— 只靠目录权限保护，
//! 目录一旦被读走凭据即泄漏。因此本模块**整份配置**用 `sdk::host::seal` 加密后落盘，
//! 密钥留在宿主手里（插件拿不到），与迁移前「宿主 age 加密」的安全等级一致。
//!
//! ## 兼容：没有能力表时退回声明式回写
//! 老宿主（未下发能力表 / 无 `seal`）上，本模块**不落盘**，
//! 由调用方继续用返回值里的 `config` 声明交给宿主代存 —— 保证插件在新旧宿主上都能用。
//!
//! ## 落盘格式
//! `config.json` = `{"v":1,"sealed":"<base64 密文>"}`；密文解开后是一个 JSON 对象，
//! 即此前 `self_config` 的那份键值表（`accounts` / `percent-<id>` / `percent`）。
//! 用外层信封而不是「直接存密文」是为了将来能改加密方案而不破坏识别。

use std::path::PathBuf;

use serde_json::Value;

/// 配置文件名（在插件私有目录内）
const FILE: &str = "config.json";

/// 插件私有配置存储
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// 打开存储：拿不到私有目录（老宿主 / 未绑定）→ `None`
    ///
    /// `None` 时调用方应退回**声明式回写**（宿主代存）。
    pub fn open() -> Option<Self> {
        let dir = fn_kzwr_plugin_sdk::host::own_data_dir()?;
        Some(Self {
            dir: PathBuf::from(dir),
        })
    }

    fn path(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    /// 读取配置（键值表）；不存在/解密失败/格式损坏 → 空对象
    ///
    /// **解密失败一律当空配置**，绝不把密文当明文用（见 SDK `unseal` 的契约）。
    pub fn load(&self) -> Value {
        let Ok(text) = std::fs::read_to_string(self.path()) else {
            return Value::Object(Default::default());
        };
        let Ok(env) = serde_json::from_str::<Value>(&text) else {
            crate::log::warn("配置文件不是合法 JSON，按空配置处理");
            return Value::Object(Default::default());
        };
        let Some(sealed) = env.get("sealed").and_then(|s| s.as_str()) else {
            // 没有 sealed 字段：可能是明文旧格式 —— 不接受（避免「以为加密了其实是明文」）
            crate::log::warn("配置文件缺少 sealed 字段，按空配置处理");
            return Value::Object(Default::default());
        };
        match fn_kzwr_plugin_sdk::host::unseal(sealed) {
            Some(plain) => serde_json::from_str(&plain).unwrap_or_else(|_| {
                crate::log::warn("配置解密后不是合法 JSON，按空配置处理");
                Value::Object(Default::default())
            }),
            None => {
                // 可能是换过宿主口令（配置无法解开）——明确告知，而不是静默当成「没配置」
                crate::log::warn("配置解密失败（宿主口令是否变更？），按空配置处理");
                Value::Object(Default::default())
            }
        }
    }

    /// 保存配置（加密后落盘）；返回是否成功
    pub fn save(&self, kv: &Value) -> bool {
        let plain = kv.to_string();
        let Some(sealed) = fn_kzwr_plugin_sdk::host::seal(&plain) else {
            crate::log::warn("配置加密失败（宿主 seal 不可用？），本次不落盘");
            return false;
        };
        let env = serde_json::json!({ "v": 1, "sealed": sealed });
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            crate::log::warn(format!("创建插件数据目录失败：{e}"));
            return false;
        }
        // 先写临时文件再改名：避免写到一半崩溃留下半个文件（下次读会解密失败）
        let tmp = self.dir.join(format!("{FILE}.tmp"));
        if let Err(e) = std::fs::write(&tmp, env.to_string()) {
            crate::log::warn(format!("写临时配置文件失败：{e}"));
            return false;
        }
        if let Err(e) = std::fs::rename(&tmp, self.path()) {
            crate::log::warn(format!("替换配置文件失败：{e}"));
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        true
    }
}
