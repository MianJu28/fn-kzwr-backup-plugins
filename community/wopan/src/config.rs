//! 插件自管配置（ADR-021）：存进 `own_data_dir`，敏感字段用宿主 `seal` 加密
//!
//! 为什么需要它：宿主的**插件级动作/体检拿不到目标凭据**（`cfg` 只是 AppConfig
//! 快照，不含插件配置，更不含 `TargetConfig` 的密码）。而「用户信息」「清空回收站」
//! 这些是**插件页**上的操作，此时并没有目标实例可借用。
//!
//! 于是插件把 H5 令牌**自己**存一份（用宿主密钥加密，插件拿不到密钥本身），
//! 插件页的动作/体检据此构造实例。目标配置里的令牌仍然独立存在，两者互不影响。
//!
//! 存储格式（`<own_data_dir>/wopan.json`）：
//! ```json
//! { "token_sealed": "<宿主 seal 的密文>", "client_id": "1001000035", "phone": "130..." }
//! ```
//! ⚠️ 令牌**只以密文形式落盘**；`unseal` 失败一律当「无配置」，绝不回退明文。

use fn_kzwr_plugin_sdk as sdk;
use serde_json::{json, Value};
use std::path::PathBuf;

/// 配置文件名
const FILE: &str = "wopan.json";

/// 插件自管配置
#[derive(Debug, Clone, Default)]
pub struct SelfConfig {
    /// H5 令牌（明文，仅在内存）
    pub token: String,
    /// clientId
    pub client_id: String,
    /// 手机号（仅用于显示/发码，非敏感）
    pub phone: String,
}

fn config_path() -> Option<PathBuf> {
    let dir = sdk::host::own_data_dir()?;
    Some(PathBuf::from(dir).join(FILE))
}

impl SelfConfig {
    /// 读取配置（无配置/解密失败 → 空配置）
    pub fn load() -> Self {
        let Some(p) = config_path() else {
            return Self::default();
        };
        let Ok(raw) = std::fs::read_to_string(&p) else {
            return Self::default();
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            return Self::default();
        };
        // 令牌是密文：unseal 失败 ⇒ 视为无令牌（绝不把密文当明文用）
        let token = v
            .get("token_sealed")
            .and_then(|s| s.as_str())
            .and_then(sdk::host::unseal)
            .unwrap_or_default();
        SelfConfig {
            token,
            client_id: v
                .get("client_id")
                .and_then(|s| s.as_str())
                .unwrap_or(crate::protocol::H5_CLIENT_ID)
                .to_string(),
            phone: v.get("phone").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        }
    }

    /// 保存配置（令牌加密后落盘；先写临时文件再改名，避免半个文件）
    pub fn save(&self) -> Result<(), String> {
        let p = config_path().ok_or_else(|| "宿主未提供 own_data_dir（能力表不可用）".to_string())?;
        let sealed = if self.token.is_empty() {
            String::new()
        } else {
            sdk::host::seal(&self.token)
                .ok_or_else(|| "宿主 seal 不可用，拒绝明文落盘令牌".to_string())?
        };
        let body = json!({
            "token_sealed": sealed,
            "client_id": self.client_id,
            "phone": self.phone,
        })
        .to_string();
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, body).map_err(|e| format!("写临时配置失败: {e}"))?;
        std::fs::rename(&tmp, &p).map_err(|e| format!("落定配置失败: {e}"))?;
        Ok(())
    }

    /// 是否已有可用令牌
    pub fn has_token(&self) -> bool {
        !self.token.is_empty()
    }

    /// 用本配置构造一个实例（供插件页动作/体检使用）
    pub fn instance(&self) -> Option<crate::instance::Wopan> {
        if !self.has_token() {
            return None;
        }
        let j = json!({
            "username": self.phone,
            "password": self.token,
            "url": "",
            "config": { "client_id": self.client_id, "space_type": "0" },
        })
        .to_string();
        crate::instance::parse_target(&j).ok()
    }

    /// 从登录动作的返回里记录令牌
    pub fn set_token(&mut self, token: &str) {
        self.token = token.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无能力表（未 host_bind）时：load 应安全返回空配置，不 panic
    #[test]
    fn load_without_host_is_empty() {
        let c = SelfConfig::load();
        assert!(!c.has_token(), "无宿主能力表时不应有令牌");
    }

    /// 令牌为空时不应要求 seal（否则无 token 的保存会失败）
    #[test]
    fn save_without_token_is_safe_shape() {
        let c = SelfConfig { token: String::new(), client_id: "x".into(), phone: "y".into() };
        assert!(!c.has_token());
        // 没有 own_data_dir 时应报错而不是 panic
        let r = c.save();
        assert!(r.is_err(), "无 own_data_dir 时应返回错误");
    }
}
