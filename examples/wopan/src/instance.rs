//! 实例状态：一个联通云盘目标
//!
//! `target_open` 用 `target_json` 构造。凭据取自宿主传入的 `username`/`password`：
//!   - `username` → 手机号
//!   - `password` → H5 渠道的 access_token（同时是 wohome 通道的加密密钥）
//!
//! ⚠️ token 等同数据密钥，只留在内存，不写日志、不落盘。

use crate::protocol::{h5_secret, Channel, BASE_URL, H5_CLIENT_ID};
use serde_json::{json, Value};
use std::sync::Mutex;

/// 一次写入的句柄
///
/// 上传能力**尚未实现**（联通云盘上传接口资料缺失），故 `write_begin` 一律失败，
/// 句柄类型保留为空结构，待上传协议确认后填充。
pub struct WriteH {
    pub rel: String,
    pub total: u64,
    pub fed: u64,
}

/// 一次读取的句柄：把整个文件读进内存后分块吐给宿主
pub struct ReadH {
    pub data: Vec<u8>,
    pub pos: usize,
}

pub struct Wopan {
    /// 手机号（`username`）
    pub phone: String,
    /// H5 access_token（`password`）；同时是 wohome 通道密钥
    pub token: String,
    /// clientId（H5 默认）
    pub client_id: String,
    /// 该 clientId 对应的客户端密钥
    pub secret_key: String,
    /// 备份根目录（云盘内路径，如 `/酷族备份`）
    pub root: String,
    /// 空间类型：0=个人云 1=家庭云 4=隐私空间
    pub space_type: String,
    /// 最近一次错误
    pub last_error: Mutex<String>,
    /// HTTP 客户端（blocking；插件不自己做异步）
    pub http: reqwest::blocking::Client,
}

impl Wopan {
    pub fn err(&self, s: String) {
        if let Ok(mut g) = self.last_error.lock() {
            *g = s;
        }
    }

    /// 调用 dispatcher，返回解密后的业务数据
    pub fn dispatch(
        &self,
        cmd: &str,
        payload: Value,
        channel: Channel,
    ) -> Result<Value, String> {
        // wohome 通道用 token 作密钥；api-user 用客户端密钥
        let key: &str = match channel {
            Channel::WoHome => &self.token,
            Channel::ApiUser => &self.secret_key,
        };
        let res_time = now_ms();
        let req_seq = 100000 + (res_time % 89999) as i64;

        let envelope = crate::protocol::build_envelope(
            cmd,
            &payload,
            Some(&self.client_id),
            key,
            channel.as_str(),
            res_time,
            req_seq,
        )?;

        let url = format!("{BASE_URL}{}", channel.path());
        let resp = self
            .http
            .post(&url)
            .json(&envelope)
            .send()
            .map_err(|e| format!("请求失败: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let body: Value = resp.json().map_err(|e| format!("响应不是 JSON: {e}"))?;

        let rsp = body.get("RSP").cloned().unwrap_or(Value::Null);
        let code = rsp.get("RSP_CODE").and_then(|c| c.as_str()).unwrap_or("");
        // 登录态失效要单独识别，便于上层给出明确提示
        if code == crate::protocol::code::SESSION_INVALID {
            return Err("登录态失效（1001），请重新获取令牌".to_string());
        }
        if code == crate::protocol::code::DECRYPT_FAIL {
            return Err("解密失败（9002），clientId 与密钥可能不匹配".to_string());
        }
        if !code.is_empty() && code != crate::protocol::code::OK {
            let desc = rsp.get("RSP_DESC").and_then(|d| d.as_str()).unwrap_or("");
            return Err(format!("业务码 {code}: {desc}"));
        }

        // DATA 可能是密文（按通道密钥解密），也可能已是明文对象
        let data = rsp.get("DATA").cloned().unwrap_or(Value::Null);
        match data.as_str() {
            Some(s) if !s.is_empty() => {
                let plain = crate::protocol::decrypt(s, key)?;
                serde_json::from_str(&plain)
                    .map_err(|e| format!("DATA 解密后不是合法 JSON: {e}"))
            }
            _ => Ok(data),
        }
    }
}

/// 当前毫秒时间戳
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 从 `target_json` 解析出实例配置
pub fn parse_target(json_str: &str) -> Result<Wopan, String> {
    let v: Value = serde_json::from_str(json_str).map_err(|e| format!("target_json 解析失败: {e}"))?;
    let phone = v
        .get("username")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let token = v
        .get("password")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if token.is_empty() {
        return Err("缺少 H5 令牌：请在目标配置里填写 access_token（可用插件页的短信登录获取）".to_string());
    }

    // clientId：默认 H5 渠道；允许 config.client_id 覆盖（应对站点轮换）
    let cfg = v.get("config").cloned().unwrap_or(Value::Null);
    let client_id = cfg
        .get("client_id")
        .and_then(|c| c.as_str())
        .unwrap_or(H5_CLIENT_ID)
        .to_string();
    let secret_key = h5_secret(&client_id).to_string();

    // 备份根目录：url（云盘内路径），留空表示根目录
    let root = v.get("url").and_then(|u| u.as_str()).unwrap_or("").trim().to_string();
    let space_type = cfg
        .get("space_type")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            v.get("config")
                .and_then(|c| c.get("space_type"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "0".to_string());

    Ok(Wopan {
        phone,
        token,
        client_id,
        secret_key,
        root,
        space_type,
        last_error: Mutex::new(String::new()),
        http: reqwest::blocking::Client::builder()
            // 站点对自定义头敏感（实测带 X-CM-SERVICE 等头访问 CDN 会 SSL 错误），
            // 这里只保留最小必要头，避免触发风控。
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .build()
            .map_err(|e| format!("HTTP 客户端创建失败: {e}"))?,
    })
}

/// 云盘内路径拼接：把目标端相对路径挂到 root 下
///
/// ⚠️ 拒绝 `..` 与绝对路径逃逸（与本地目录目标同款防护）
pub fn join_remote(root: &str, rel: &str) -> Option<String> {
    let cleaned = rel.trim_start_matches('/');
    if cleaned.is_empty() {
        return None;
    }
    for seg in cleaned.split('/') {
        if seg == ".." {
            return None;
        }
    }
    let root = root.trim().trim_matches('/');
    Some(if root.is_empty() {
        cleaned.to_string()
    } else {
        format!("{root}/{cleaned}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_token_is_rejected() {
        let j = json!({"username": "13000000000", "password": ""}).to_string();
        assert!(parse_target(&j).is_err(), "空令牌必须被拒绝");
    }

    #[test]
    fn token_present_builds_instance() {
        let j = json!({
            "username": "13000000000",
            "password": "tok",
            "url": "/酷族备份"
        })
        .to_string();
        let w = parse_target(&j).expect("应构造成功");
        assert_eq!(w.phone, "13000000000");
        assert_eq!(w.token, "tok");
        assert_eq!(w.root, "/酷族备份");
        assert_eq!(w.client_id, H5_CLIENT_ID);
        assert_eq!(w.secret_key, h5_secret(H5_CLIENT_ID));
        // 未声明 space_type 时默认个人云
        assert_eq!(w.space_type, "0");
    }

    #[test]
    fn remote_path_join_and_escape() {
        assert_eq!(join_remote("", "a/b.bin").unwrap(), "a/b.bin");
        assert_eq!(join_remote("/酷族备份", "a/b.bin").unwrap(), "酷族备份/a/b.bin");
        assert_eq!(join_remote("/r/", "/x").unwrap(), "r/x");
        // 逃逸一律拒绝
        assert!(join_remote("/r", "../etc/passwd").is_none());
        assert!(join_remote("/r", "a/../..").is_none());
        assert!(join_remote("/r", "").is_none());
    }

    #[test]
    fn now_ms_is_plausible() {
        let t = now_ms();
        assert!(t > 1_700_000_000_000, "毫秒时间戳不合理: {t}");
    }
}
