//! 实例状态：一个联通云盘目标
//!
//! `target_open` 用 `target_json` 构造。凭据取自宿主传入的 `username`/`password`：
//!   - `username` → 手机号
//!   - `password` → H5 渠道的 access_token（同时是 wohome 通道的加密密钥）
//!
//! ⚠️ token 等同数据密钥，只留在内存，不写日志、不落盘。

use crate::protocol;
use crate::protocol::{h5_secret, Channel, BASE_URL, H5_CLIENT_ID};
use serde_json::{json, Value};
use std::sync::Mutex;

/// 一次写入（上传）的句柄
///
/// 服务端按**固定 8MB 步长**拼装分片，故这里攒够一个整片才发一片；末片在
/// `write_end` 时作为余数发出。缓冲因此最多 8MB（不会把整个备份读进内存）。
pub struct WriteH {
    /// 云盘内完整路径
    pub remote: String,
    /// 文件名（末段）
    pub file_name: String,
    /// 父目录 id
    pub dir_id: String,
    /// 宿主声明的总字节数
    pub total: u64,
    /// 已接收字节数
    pub fed: u64,
    /// 已成功上传字节数
    pub uploaded: u64,
    /// 未满一片的缓冲
    pub buf: Vec<u8>,
    /// 已发送片数
    pub part_index: u64,
    /// 总分片数（严格 ceil）
    pub total_parts: u64,
    /// `{毫秒时间戳}_{6位随机}`
    pub unique_id: String,
    /// 同批次 32 位随机串
    pub batch_no: String,
    /// token 加密的 fileInfo（明文含 6 字段）
    pub file_info: String,
    /// 上传域名
    pub host: String,
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
    /// 上传域名（`GetZoneInfo` 下发；懒加载缓存）
    pub upload_host: Mutex<String>,
    /// 文件类型表（findClassifyRule → fileTypes；懒加载缓存，用于 fileInfo.fileType）
    pub file_types: Mutex<Option<Value>>,
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
        // ⚠️ 严格对齐 Python 参考实现 `_post()`：
        //   1) 每次请求都带 **`accesstoken: <token>`** 头 —— wohome 通道据此识别
        //      登录态。缺了这个头，wohome 一律返回 `1001 无效登录信息`
        //      （而 api-user 不依赖它，故此前 api-user 通、wohome 全挂）。
        //   2) body 手动序列化成 UTF-8 字节（Python 用 `dumps(data).encode()`），
        //      而非 `json=`（后者会走不同的 Content-Type 处理）。
        let body_bytes = crate::protocol::dumps(&envelope).into_bytes();
        let resp = self
            .http
            .post(&url)
            .header("accesstoken", self.token.as_str())
            .header("Content-Type", "application/json;charset=UTF-8")
            .body(body_bytes)
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

    /// 取上传域名（`GetZoneInfo`），懒加载并缓存；失败时退回兜底域名
    pub fn upload_host(&self) -> String {
        if let Ok(g) = self.upload_host.lock() {
            if !g.is_empty() {
                return g.clone();
            }
        }
        let host = match self.dispatch(
            crate::CMD_GET_ZONE_INFO,
            json!({ "appId": protocol::upload::ZONE_APP_ID }),
            Channel::WoHome,
        ) {
            Ok(d) => d
                .get("url")
                .and_then(|u| u.as_str())
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| protocol::upload::DEFAULT_HOST.to_string()),
            Err(_) => protocol::upload::DEFAULT_HOST.to_string(),
        };
        if let Ok(mut g) = self.upload_host.lock() {
            *g = host.clone();
        }
        host
    }

    /// 按扩展名取 fileType（findClassifyRule 的 fileTypes）；查不到回退 "0"
    pub fn file_type(&self, filename: &str) -> String {
        // 懒加载类型表
        let need = self.file_types.lock().map(|g| g.is_none()).unwrap_or(false);
        if need {
            let table = self
                .get_classify_rule()
                .ok()
                .and_then(|d| d.get("result").and_then(|r| r.get("fileTypes")).cloned());
            if let Ok(mut g) = self.file_types.lock() {
                *g = table;
            }
        }
        let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        let table = match self.file_types.lock() {
            Ok(g) => g.clone(),
            Err(_) => None,
        };
        if let Some(t) = table {
            for key in [ext.clone(), format!(".{ext}")] {
                if let Some(e) = t.get(&key) {
                    if let Some(s) = e.as_str() {
                        return s.to_string();
                    }
                    if let Some(v) = e.get("type").and_then(|x| x.as_str()) {
                        return v.to_string();
                    }
                }
            }
        }
        "0".to_string()
    }

    /// `GET /wohome/free/v1/findClassifyRule`（**不走 dispatcher**，结构是 meta/result）
    fn get_classify_rule(&self) -> Result<Value, String> {
        let url = format!("{}{}", protocol::BASE_URL, "/wohome/free/v1/findClassifyRule");
        let resp = self
            .http
            .get(&url)
            .send()
            .map_err(|e| format!("findClassifyRule 请求失败: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("findClassifyRule HTTP {}", resp.status()));
        }
        resp.json::<Value>()
            .map_err(|e| format!("findClassifyRule 响应不是 JSON: {e}"))
    }

    /// 上传一个分片（multipart，字段名 `file`）
    ///
    /// ⚠️ 必须用**独立连接**：上传网关对 dispatcher 的自定义头敏感（会 400）。
    /// 该域名 TLS 偶发 UNEXPECTED_EOF，故重试若干次。
    pub fn post_upload_part(
        &self,
        host: &str,
        form: &[(&str, String)],
        filename: &str,
        chunk: &[u8],
        retries: usize,
    ) -> Result<reqwest::blocking::Response, String> {
        let url = format!("{}{}", host, protocol::upload::PATH);
        let mut last = String::new();
        for attempt in 0..retries.max(1) {
            // 每片都新建 client：避免复用连接的头污染；代价是每次 TLS 握手
            let client = match reqwest::blocking::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .build()
            {
                Ok(c) => c,
                Err(e) => return Err(format!("上传客户端创建失败: {e}")),
            };
            let mut mp = reqwest::blocking::multipart::Form::new();
            for (k, v) in form {
                mp = mp.text((*k).to_string(), v.clone());
            }
            let part = reqwest::blocking::multipart::Part::bytes(chunk.to_vec())
                .file_name(filename.to_string())
                .mime_str("application/octet-stream")
                .map_err(|e| format!("构造分片失败: {e}"))?;
            mp = mp.part("file".to_string(), part);

            match client.post(&url).multipart(mp).send() {
                Ok(r) => return Ok(r),
                Err(e) => {
                    last = e.to_string();
                    // 退避重试（域名间歇 UNEXPECTED_EOF）
                    std::thread::sleep(std::time::Duration::from_millis(
                        500 * (attempt as u64 + 1),
                    ));
                }
            }
        }
        Err(format!("上传连接失败（已重试）: {last}"))
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

    let cid_for_headers = client_id.clone();
    Ok(Wopan {
        phone,
        token,
        client_id,
        secret_key,
        root,
        space_type,
        last_error: Mutex::new(String::new()),
        upload_host: Mutex::new(String::new()),
        file_types: Mutex::new(None),
        // ⚠️ dispatcher 请求**必须**带 H5 渠道固定头，否则 wohome 通道返回
        //    `1001 登录态失效`（实测：只发 UA 时 api-user 通、wohome 1001）。
        //    这些头是服务端识别渠道的依据，不是可选项。
        //
        //    另注：**上传/下载不能复用本客户端** —— 网关（hyupload/hydownload）
        //    对 X-CM-SERVICE 这类头敏感，会 400；那两处用独立连接（见
        //    `post_upload_part` 与 `read_begin`）。
        http: {
            use reqwest::header::{HeaderMap, HeaderValue};
            let mut headers = HeaderMap::new();
            let cid = cid_for_headers;
            for (k, v) in [
                ("X-CM-SERVICE", "PHONE"),
                ("source-type", "woapi"),
                ("X-YP-Open-Version", "v1.0"),
                ("Client-Id", cid.as_str()),
                ("X-YP-Client-Id", cid.as_str()),
                ("clientId", cid.as_str()),
                // Python session 默认头里还有这两个（站点可能据此校验来源）
                ("Origin", "https://pan.wo.cn"),
                ("Referer", "https://pan.wo.cn/"),
            ] {
                if let (Ok(name), Ok(val)) = (
                    reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(v),
                ) {
                    headers.insert(name, val);
                }
            }
            reqwest::blocking::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                .default_headers(headers)
                .build()
                .map_err(|e| format!("HTTP 客户端创建失败: {e}"))?
        },
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
