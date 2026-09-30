//! 酷族网盘（kzwr.com）API 客户端 —— **插件自带**
//!
//! 迁移前这块代码在宿主里（`backend/src/infra/kzwr_api/`），现在完整搬到插件内：
//! 核心不再知道 kzwr 的存在，只按稳定 C ABI 调本插件。
//!
//! 只保留增强功能**真正用到**的四个端点（原客户端还有登录/上传/下载/分享等
//! 几十个方法，那些属于 WebDAV 主链路，与本插件无关）：
//!
//! - `GET  /api/v2/member`                → 是否已登录 + 空间用量（total/use）
//! - `GET  /api/v2/user/trash?page=N`     → 回收站分页列表
//! - `POST /api/v2/files/delete/physical` → 物理删除文件（body `{"Pids":[…]}`）
//! - `POST /api/v2/folder/delete/physical`→ 物理删除文件夹（body `{"FolderIds":[…]}`）
//!
//! 认证：请求头 `access-token`（浏览器登录后从 Cookie 复制）。

use std::time::Duration;

use serde_json::{json, Value};

/// kzwr 站点地址（原 `infra/kzwr_api/constants.rs::BASE_URL`）
const BASE_URL: &str = "https://www.kzwr.com";

/// 全局请求超时。原宿主取 3600s 是为大文件分片 PUT 服务的，
/// 本插件只有轻量的 JSON 查询/删除，用 60s 更能及时暴露网络问题。
const TIMEOUT_SECS: u64 = 60;

/// 网络错误 / 5xx 的重试次数（4xx 是确定性错误，不重试）
const RETRY_ATTEMPTS: u32 = 3;

/// 单个端点的调用结果
pub type ApiResult<T> = Result<T, String>;

/// 客户端（无状态：token 由调用方按账号传入，故可全局共享一个连接池）
pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                h.insert(
                    reqwest::header::CONTENT_TYPE,
                    "application/json; charset=utf-8".parse().unwrap(),
                );
                h.insert(
                    reqwest::header::ACCEPT,
                    "application/json".parse().unwrap(),
                );
                h.insert(
                    reqwest::header::USER_AGENT,
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36"
                        .parse()
                        .unwrap(),
                );
                h
            })
            .build()
            .expect("kzwr 插件：构建 HTTP 客户端失败");
        Self { http }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", BASE_URL, path)
    }

    /// `GET /api/v2/member`：登录态 + 空间用量
    pub async fn get_member(&self, token: &str) -> ApiResult<Value> {
        self.get_json("/api/v2/member", token, &[]).await
    }

    /// `GET /api/v2/user/trash?page=N`：回收站分页
    pub async fn get_trash(&self, token: &str, page: u32) -> ApiResult<Value> {
        self.get_json("/api/v2/user/trash", token, &[("page", page.to_string())])
            .await
    }

    /// 物理删除回收站里的**文件**（用条目的 `encodedId`，不是普通列表的 `sid`）
    pub async fn delete_trash_files(&self, token: &str, pids: &[String]) -> ApiResult<Value> {
        self.post_json("/api/v2/files/delete/physical", token, json!({ "Pids": pids }))
            .await
    }

    /// 物理删除回收站里的**文件夹**
    pub async fn delete_trash_folders(
        &self,
        token: &str,
        folder_ids: &[String],
    ) -> ApiResult<Value> {
        self.post_json(
            "/api/v2/folder/delete/physical",
            token,
            json!({ "FolderIds": folder_ids }),
        )
        .await
    }

    async fn get_json(
        &self,
        path: &str,
        token: &str,
        params: &[(&str, String)],
    ) -> ApiResult<Value> {
        let what = format!("GET {path}");
        self.with_retry(&what, || {
            let mut req = self.http.get(self.url(path));
            for (k, v) in params {
                req = req.query(&[(k, v.as_str())]);
            }
            req.header("access-token", token)
        })
        .await
    }

    async fn post_json(&self, path: &str, token: &str, body: Value) -> ApiResult<Value> {
        let what = format!("POST {path}");
        self.with_retry(&what, || {
            self.http
                .post(self.url(path))
                .header("access-token", token)
                .json(&body)
        })
        .await
    }

    /// 发送并在网络错误 / 5xx 时重试（线性退避）
    async fn with_retry<F>(&self, what: &str, build: F) -> ApiResult<Value>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            // 闭包只做请求构造（同步、不 panic）；发送与解析的错误都走下面的分支
            let result = match build().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() >= 500 && attempt < RETRY_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(500_u64 * attempt as u64)).await;
                        continue;
                    }
                    parse_response(resp).await
                }
                Err(e) => Err(format!("网络请求失败: {e}")),
            };
            match result {
                Ok(v) => return Ok(v),
                Err(msg) => {
                    // 只重试确定性之外的错误
                    let retryable = msg.starts_with("网络请求失败") || msg.starts_with("HTTP 5");
                    if retryable && attempt < RETRY_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(500_u64 * attempt as u64)).await;
                        continue;
                    }
                    return Err(format!("{what} 失败: {msg}"));
                }
            }
        }
    }
}

/// 解析响应：>=400 转成错误（401 明确提示 token 无效），其它原样返回 JSON
async fn parse_response(resp: reqwest::Response) -> ApiResult<Value> {
    let status = resp.status();
    let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return Err(format!("读取响应失败: {e}")),
    };
    let data: Value =
        serde_json::from_str(&body_text).unwrap_or_else(|_| Value::String(body_text.clone()));

    if status.as_u16() >= 400 {
        let err_msg = match &data {
            Value::Object(map) => ["error", "message", "errorMessage"]
                .iter()
                .find_map(|k| map.get(*k).and_then(|v| v.as_str()))
                .unwrap_or_default()
                .to_string(),
            other => other
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
        };
        let msg = if status.as_u16() == 401 {
            format!("未授权(access-token 无效): {err_msg}")
        } else if err_msg.is_empty() {
            format!("HTTP {}", status)
        } else {
            err_msg
        };
        return Err(msg);
    }
    Ok(data)
}

/// 是否处于「未登录」状态（酷族对无效 token 返回 200 + `data.isLogin=false`）
///
/// 取不到该字段时**保守认为已登录**（避免把站点改版误报成 token 失效）。
pub fn member_is_login(v: &Value) -> bool {
    v.get("data")
        .and_then(|d| d.get("isLogin"))
        .and_then(|x| x.as_bool())
        .unwrap_or(true)
}

/// 从 `/api/v2/member` 响应里取空间用量（`total`/`capacity` 取大者，`use`）
pub fn member_space(v: &Value) -> (u64, u64) {
    let data = v.get("data").cloned().unwrap_or_default();
    let num = |key: &str| data.get(key).and_then(|x| x.as_u64()).unwrap_or(0);
    (num("total").max(num("capacity")), num("use"))
}

/// 回收站条目数组（站点字段名随版本而异，逐个尝试兼容）
pub fn trash_items(v: &Value) -> Vec<Value> {
    for (obj, key) in [
        (v.get("data"), "items"),
        (v.get("data"), "list"),
        (v.get("data"), "records"),
        (v.get("data"), "files"),
        (Some(v), "items"),
    ] {
        if let Some(arr) = obj.and_then(|d| d.get(key)).and_then(|i| i.as_array()) {
            if !arr.is_empty() {
                return arr.clone();
            }
        }
    }
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        return arr.clone();
    }
    Vec::new()
}

/// 条目字节数（字段名不确定，逐个尝试；字符串数字也接受）
pub fn trash_item_size(it: &Value) -> Option<u64> {
    for k in ["length", "size", "fileSize", "sizeBytes", "bytes", "totalSize"] {
        let Some(v) = it.get(k) else { continue };
        if let Some(n) = v.as_u64() {
            return Some(n);
        }
        if let Some(s) = v.as_str() {
            if let Ok(n) = s.parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

/// 条目的删除时间（毫秒）。解析不出时返回 `None`，调用方**保守保留**该条目。
pub fn trash_item_deleted_ms(it: &Value) -> Option<i64> {
    const KEYS: [&str; 8] = [
        "deletedDate",
        "deleteTime",
        "deletedAt",
        "deleteAt",
        "deleteDate",
        "updateTime",
        "time",
        "createTime",
    ];
    for k in KEYS {
        let Some(v) = it.get(k) else { continue };
        if let Some(n) = v.as_i64() {
            return Some(if n < 10_000_000_000 { n * 1000 } else { n });
        }
        if let Some(s) = v.as_str() {
            if let Ok(n) = s.parse::<i64>() {
                return Some(if n < 10_000_000_000 { n * 1000 } else { n });
            }
            if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(d.timestamp_millis());
            }
            // 无时区的裸日期时间（如 "2026-09-20 12:34:56"）：酷族返回的是
            // 服务器本地时间，按本机时区解释，否则年龄门槛会偏差一个时区偏移量
            if let Some(ms) = parse_naive_local_ms(s) {
                return Some(ms);
            }
        }
    }
    None
}

fn parse_naive_local_ms(s: &str) -> Option<i64> {
    use chrono::TimeZone;
    if let Ok(nd) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return chrono::Local
            .from_local_datetime(&nd)
            .single()
            .map(|d| d.timestamp_millis());
    }
    if let Ok(nd) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return chrono::Local
            .from_local_datetime(&nd)
            .single()
            .map(|d| d.timestamp_millis());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let nd = d.and_hms_opt(0, 0, 0)?;
        return chrono::Local
            .from_local_datetime(&nd)
            .single()
            .map(|d| d.timestamp_millis());
    }
    None
}

/// 条目是否为文件夹（字段名兼容多种写法）
pub fn trash_is_folder(it: &Value) -> bool {
    for k in ["isFolder", "is_folder", "dir", "isDir"] {
        if let Some(b) = it.get(k).and_then(|x| x.as_bool()) {
            return b;
        }
    }
    for k in ["type", "fileType", "category"] {
        if let Some(s) = it.get(k).and_then(|x| x.as_str()) {
            let l = s.to_ascii_lowercase();
            if l.contains("folder") || l.contains("dir") {
                return true;
            }
        }
    }
    false
}

/// 条目的物理删除用 id（回收站一律用 `encodedId`）
pub fn trash_item_id(it: &Value) -> Option<String> {
    for k in ["encodedId", "encoded_id", "encodeId", "fileCode", "code"] {
        if let Some(s) = it.get(k).and_then(|x| x.as_str()) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    // 数字型 encodedId 亦常见
    for k in ["encodedId", "encoded_id", "encodeId"] {
        if let Some(n) = it.get(k).and_then(|x| x.as_i64()) {
            return Some(n.to_string());
        }
    }
    None
}

/// 单页批量删除（文件/文件夹自动分组）
pub async fn delete_trash_items(client: &Client, token: &str, items: &[Value]) -> ApiResult<()> {
    let file_pids: Vec<String> = items
        .iter()
        .filter(|it| !trash_is_folder(it))
        .filter_map(trash_item_id)
        .collect();
    let folder_ids: Vec<String> = items
        .iter()
        .filter(|it| trash_is_folder(it))
        .filter_map(trash_item_id)
        .collect();

    // 一个 id 都没提取到：必须报错，否则空转删除会让「已清理」计数虚高而实际什么都没删
    if file_pids.is_empty() && folder_ids.is_empty() {
        let first = items
            .first()
            .map(|v| truncate_json(v, 300))
            .unwrap_or_else(|| "(无条目)".to_string());
        return Err(format!(
            "回收站条目格式不识别（无法提取 id），已保守取消删除；首条目: {first}"
        ));
    }
    if !file_pids.is_empty() {
        client.delete_trash_files(token, &file_pids).await?;
    }
    if !folder_ids.is_empty() {
        client.delete_trash_folders(token, &folder_ids).await?;
    }
    Ok(())
}

/// 截断 JSON 原文（日志/错误信息用，避免刷出超大响应）
pub fn truncate_json(v: &Value, max: usize) -> String {
    let s = v.to_string();
    if s.len() <= max {
        s
    } else {
        format!("{}…(截断)", s.chars().take(max).collect::<String>())
    }
}

/// 人类可读字节数
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{:.0} {}", v, UNITS[i])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}
