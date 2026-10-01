//! 酷族备份 · 外置插件 —— **联通云盘（沃云盘）备份目标**
//!
//! 采用 **H5 端协议**（`LoginByMobileV2`），不实现 Web / PC 等其他端。
//! H5 渠道 token 有效期更长（实测约 60 天，Web 端约 7 天）。
//!
//! ## 上传实现要点
//!
//! 上传**不走 dispatcher**：`GetZoneInfo` 取上传域名 → `POST {host}/openapi/client/upload2C`
//! （multipart，字段名 `file`，8MB 分片）。
//!
//! ⚠️ **分片必须是严格 ceil 布局**（非末片整 8MB、末片为余数）。服务端按固定
//! 8MB 步长拼装：若沿用前端那套 `floor`+末尾吸收，末片会 >8MB，服务端**仍返回成功**
//! 但文件被**静默截断**成 `片数 × 8MB`（实测 20MB → 16,777,216）。
//! 本插件在 `write_end` 校验"已发字节数 == 声明总字节数"，不符即报错，
//! 绝不让"备份显示成功但云端少一截"静默发生。
//!
//! ## 凭据
//!
//! - `username` → 手机号
//! - `password` → H5 access_token（同时是 wohome 通道的加密密钥，等同数据密钥）
//!
//! 令牌可用插件页的「发送短信验证码」+「用验证码登录」动作获取。
//!
//! ## 安全
//!
//! - 插件只接触 **age 密文**，明文与密钥永不离开宿主
//! - token 只留在内存，不写日志、不落盘
//! - 所有 FFI 入口用 guard 闭包包裹，panic 绝不跨越 FFI

pub mod config;
pub mod instance;
pub mod protocol;

use instance::{join_remote, parse_target, ReadH, Wopan, WriteH};
use protocol::Channel;
use serde_json::{json, Value};
use std::ffi::c_void;
use std::os::raw::c_char;

use fn_kzwr_plugin_sdk as sdk;

// ════════════════════════════════════════════════════════════════════════════
// panic 兜底：**绝不让 panic 跨越 FFI 边界**
// ════════════════════════════════════════════════════════════════════════════

fn guard_str(f: impl FnOnce() -> String) -> *mut c_char {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(s) => sdk::to_c_string(s),
        Err(_) => sdk::to_c_string(sdk::json::error("插件内部错误（panic 已拦截）")),
    }
}

fn guard_ptr(f: impl FnOnce() -> Option<*mut c_void>) -> *mut c_void {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .ok()
        .flatten()
        .unwrap_or(std::ptr::null_mut())
}

fn guard_i32(f: impl FnOnce() -> i32) -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(-1)
}

#[allow(dead_code)] // write_end 目前返回固定错误码；保留以便上传实现后直接复用
fn guard_i64(f: impl FnOnce() -> i64) -> i64 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(-1)
}

/// 命令字
mod cmd {
    pub const QUERY_USER: &str = "AppQueryUser";
    pub const QUERY_ALL_FILES: &str = "QueryAllFiles";
    pub const CREATE_DIRECTORY: &str = "CreateDirectory";
    pub const DELETE_FILE: &str = "DeleteFile";
    pub const GET_DOWNLOAD_URL: &str = "GetDownloadUrl";
    pub const LOGIN_SMS_V2: &str = "LoginByMobileV2";
    /// 下发上传域名
    pub const GET_ZONE_INFO: &str = "GetZoneInfo";
    // ── 回收站 ──
    pub const QUERY_RECYCLE: &str = "QueryRecycleData";
    pub const REDUCTION_RECYCLE: &str = "ReductionRecycleData";
    pub const DELETE_RECYCLE: &str = "DeleteRecycleData";
    pub const EMPTY_RECYCLE: &str = "EmptyRecycleData";
}

/// 供 `instance.rs` 复用（`upload_host()` 用到）
pub(crate) use cmd::GET_ZONE_INFO as CMD_GET_ZONE_INFO;

// ════════════════════════════════════════════════════════════════════════════
// 目标能力表：生命周期
// ════════════════════════════════════════════════════════════════════════════

extern "C" fn target_open(raw: *const c_char) -> *mut c_void {
    guard_ptr(|| {
        let s = unsafe { sdk::from_c_str(raw) };
        match parse_target(&s) {
            Ok(inst) => Some(Box::into_raw(Box::new(inst)) as *mut c_void),
            Err(_) => None,
        }
    })
}

extern "C" fn target_close(th: *mut c_void) {
    if !th.is_null() {
        unsafe {
            drop(Box::from_raw(th as *mut Wopan));
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 写入（上传）
//
// ⚠️ **尚未实现**：联通云盘上传接口资料缺失。
//    这里刻意**明确失败**而非静默吞数据 —— 静默成功会让备份"看起来好了"
//    但云端其实没有数据，是最危险的失败模式。
// ════════════════════════════════════════════════════════════════════════════

extern "C" fn write_begin(th: *mut c_void, rel: *const c_char, total: u64) -> *mut c_void {
    guard_ptr(|| {
        if th.is_null() {
            return None;
        }
        let inst = unsafe { &*(th as *const Wopan) };
        let rel_s = unsafe { sdk::from_c_str(rel) };
        let remote = join_remote(&inst.root, &rel_s)?;

        // 父目录必须存在（逐层创建），否则上传的 directoryId 无效
        let (parent, name) = match remote.rfind('/') {
            Some(i) => (&remote[..i], remote[i + 1..].to_string()),
            None => ("", remote.clone()),
        };
        if !parent.is_empty() && ensure_remote_dir(inst, parent).is_err() {
            inst.err(format!("准备目录失败: {parent}"));
            return None;
        }
        let dir_id = if parent.is_empty() {
            "0".to_string()
        } else {
            match find_dir_id(inst, parent) {
                Some(d) => d,
                None => {
                    inst.err(format!("目录不存在且创建失败: {parent}"));
                    return None;
                }
            }
        };

        // 分片布局严格 ceil；total=0 时按单片处理
        let size = if total == 0 { 1 } else { total };
        let parts = protocol::part_sizes(size, protocol::upload::CHUNK_SIZE);
        let file_type = inst.file_type(&name);
        let info = json!({
            "spaceType": inst.space_type,
            "directoryId": dir_id,
            "batchNo": "",   // 占位，下面用真实 batchNo 覆盖
            "fileName": name,
            "fileSize": total,
            "fileType": file_type,
        });
        let batch_no = protocol::random_str(32);
        let mut info = info;
        info["batchNo"] = json!(batch_no.clone());
        let file_info = match protocol::encrypt(&info.to_string(), &inst.token) {
            Ok(s) => s,
            Err(e) => {
                inst.err(format!("构造 fileInfo 失败: {e}"));
                return None;
            }
        };

        let h = WriteH {
            remote,
            file_name: name,
            dir_id,
            total,
            fed: 0,
            uploaded: 0,
            buf: Vec::new(),
            part_index: 0,
            total_parts: parts.len() as u64,
            unique_id: format!("{}_{}", instance::now_ms(), protocol::random_str(6)),
            batch_no,
            file_info,
            host: inst.upload_host(),
        };
        Some(Box::into_raw(Box::new(h)) as *mut c_void)
    })
}

/// 发送一个分片（非末片应为整 8MB；末片为余数）
fn send_part(inst: &Wopan, h: &mut WriteH, chunk: &[u8]) -> Result<(), String> {
    let form: Vec<(&str, String)> = vec![
        ("uniqueId", h.unique_id.clone()),
        ("accessToken", inst.token.clone()),
        ("fileName", h.file_name.clone()),
        ("psToken", "undefined".to_string()),
        ("fileSize", h.total.to_string()),
        ("totalPart", h.total_parts.to_string()),
        ("partSize", chunk.len().to_string()),
        ("partIndex", (h.part_index + 1).to_string()),
        ("channel", protocol::upload::CHANNEL.to_string()),
        ("directoryId", h.dir_id.clone()),
        ("fileInfo", h.file_info.clone()),
    ];
    let resp = inst.post_upload_part(&h.host, &form, &h.file_name, chunk, 5)?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(format!("上传分片失败 HTTP {status}: {}", &body[..body.len().min(200)]));
    }
    h.part_index += 1;
    h.uploaded += chunk.len() as u64;
    Ok(())
}

extern "C" fn write_chunk(
    th: *mut c_void,
    h: *mut c_void,
    buf: *const u8,
    len: u32,
) -> i32 {
    guard_i32(|| {
        if th.is_null() || h.is_null() || buf.is_null() || len == 0 {
            return 0;
        }
        let inst = unsafe { &*(th as *const Wopan) };
        let wh = unsafe { &mut *(h as *mut WriteH) };
        let chunk = unsafe { std::slice::from_raw_parts(buf, len as usize) };
        wh.fed += len as u64;
        wh.buf.extend_from_slice(chunk);

        // 攒够整片就发（**末片留到 write_end**，因此处无法判断是否为末片）
        let csize = protocol::upload::CHUNK_SIZE;
        let mut sent = 0i32;
        while wh.buf.len() >= csize {
            let part: Vec<u8> = wh.buf.drain(..csize).collect();
            match send_part(inst, wh, &part) {
                Ok(()) => sent += part.len() as i32,
                Err(e) => {
                    inst.err(format!("上传失败: {e}"));
                    return -1;
                }
            }
        }
        if sent > 0 {
            sent
        } else {
            len as i32 // 数据已入缓冲，等价于"已接收"
        }
    })
}

extern "C" fn write_end(th: *mut c_void, h: *mut c_void) -> i64 {
    guard_i64(|| {
        if th.is_null() || h.is_null() {
            return -1;
        }
        let inst = unsafe { &*(th as *const Wopan) };
        let mut wh = unsafe { Box::from_raw(h as *mut WriteH) };

        // 末片：把残余缓冲发出（严格 ceil 布局下，它就是余数）
        if !wh.buf.is_empty() {
            let part = std::mem::take(&mut wh.buf);
            if let Err(e) = send_part(inst, &mut wh, &part) {
                inst.err(format!("上传末片失败: {e}"));
                return -1;
            }
        }
        // total=0 的占位情形：从未收到数据也要发一个空片，否则云端无文件
        if wh.part_index == 0 {
            if let Err(e) = send_part(inst, &mut wh, &[]) {
                inst.err(format!("上传空文件失败: {e}"));
                return -1;
            }
        }

        // ⚠️ 严格校验：服务端曾出现"返回成功但静默截断成 片数×8MB"。
        //    这里确认发出的字节数与宿主声明的一致，不一致即报错，
        //    绝不让"备份显示成功但云端少一截"静默发生。
        if wh.uploaded != wh.total && wh.total > 0 {
            inst.err(format!(
                "上传字节数不符：已发 {} / 声明 {}（疑似服务端截断）",
                wh.uploaded, wh.total
            ));
            return -1;
        }
        wh.uploaded as i64
    })
}

extern "C" fn write_abort(_th: *mut c_void, h: *mut c_void) {
    if !h.is_null() {
        unsafe {
            drop(Box::from_raw(h as *mut WriteH));
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 读取（下载 → 恢复）
// ════════════════════════════════════════════════════════════════════════════

extern "C" fn read_begin(th: *mut c_void, rel: *const c_char) -> *mut c_void {
    guard_ptr(|| {
        if th.is_null() {
            return None;
        }
        let inst = unsafe { &*(th as *const Wopan) };
        let rel_s = unsafe { sdk::from_c_str(rel) };
        let remote = join_remote(&inst.root, &rel_s)?;
        // 1) 取 fid（列表接口的 id 与 fid 语义不同；GetDownloadUrl 必须传 fid）
        let fid = find_fid(inst, &remote)?;
        // 2) 换直链  3) 用干净连接下载（带自定义头访问 CDN 会 SSL 错误）
        let url = download_url(inst, &fid).map_err(|e| inst.err(format!("下载直链获取失败: {e}"))).ok()?;
        let bytes = reqwest::blocking::Client::new()
            .get(&url)
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .ok()?
            .bytes()
            .ok()?
            .to_vec();
        Some(Box::into_raw(Box::new(ReadH { data: bytes, pos: 0 })) as *mut c_void)
    })
}

extern "C" fn read_chunk(
    _th: *mut c_void,
    h: *mut c_void,
    buf: *mut u8,
    len: u32,
) -> i32 {
    guard_i32(|| {
        if h.is_null() || buf.is_null() || len == 0 {
            return 0;
        }
        let rh = unsafe { &mut *(h as *mut ReadH) };
        if rh.pos >= rh.data.len() {
            return 0; // EOF
        }
        let n = (len as usize).min(rh.data.len() - rh.pos);
        unsafe {
            std::ptr::copy_nonoverlapping(rh.data.as_ptr().add(rh.pos), buf, n);
        }
        rh.pos += n;
        n as i32
    })
}

extern "C" fn read_end(_th: *mut c_void, h: *mut c_void) -> i32 {
    if !h.is_null() {
        unsafe {
            drop(Box::from_raw(h as *mut ReadH));
        }
    }
    0
}

// ════════════════════════════════════════════════════════════════════════════
// 列举 / 删除 / 目录 / 连通性
// ════════════════════════════════════════════════════════════════════════════

extern "C" fn list_json(th: *mut c_void, prefix: *const c_char) -> *mut c_char {
    guard_str(|| {
        if th.is_null() {
            return "[]".to_string();
        }
        let inst = unsafe { &*(th as *const Wopan) };
        let pfx = unsafe { sdk::from_c_str(prefix) };
        match list_files(inst, &pfx) {
            Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| "[]".to_string()),
            Err(e) => {
                inst.err(format!("列举失败: {e}"));
                "[]".to_string()
            }
        }
    })
}

extern "C" fn delete(th: *mut c_void, rel: *const c_char) -> i32 {
    guard_i32(|| {
        if th.is_null() {
            return -1;
        }
        let inst = unsafe { &*(th as *const Wopan) };
        let rel_s = unsafe { sdk::from_c_str(rel) };
        // 闭包返回 i32，不能用 `?`；越权路径按失败处理
        let remote = match join_remote(&inst.root, &rel_s) {
            Some(r) => r,
            None => return -1,
        };
        // ⚠️ 删除用 **id**（不是 fid）：前端 `fileAndDirIds` 对文件/目录都 push `e.id`；
        //    只有 `GetDownloadUrl` 用 fid。传 fid 会失败。
        let id = match find_id(inst, &remote) {
            Some(i) => i,
            None => return -1,
        };
        match inst.dispatch(
            cmd::DELETE_FILE,
            json!({ "fileList": [id], "dirList": [], "spaceType": inst.space_type }),
            Channel::WoHome,
        ) {
            Ok(_) => 0,
            Err(e) => {
                inst.err(format!("删除失败: {e}"));
                -1
            }
        }
    })
}

extern "C" fn ensure_dir(th: *mut c_void, rel: *const c_char) -> i32 {
    guard_i32(|| {
        if th.is_null() {
            return -1;
        }
        let inst = unsafe { &*(th as *const Wopan) };
        let rel_s = unsafe { sdk::from_c_str(rel) };
        let remote = match join_remote(&inst.root, &rel_s) {
            Some(r) => r,
            None => return -1,
        };
        // 逐层创建（云盘接口一次只能建一层）
        match ensure_remote_dir(inst, &remote) {
            Ok(_) => 0,
            Err(e) => {
                inst.err(format!("建目录失败: {e}"));
                -1
            }
        }
    })
}

extern "C" fn ping(th: *mut c_void) -> i32 {
    guard_i32(|| {
        if th.is_null() {
            return -1;
        }
        let inst = unsafe { &*(th as *const Wopan) };
        match inst.dispatch(cmd::QUERY_USER, json!({ "accessToken": inst.token }), Channel::ApiUser) {
            Ok(_) => 0,
            Err(e) => {
                inst.err(format!("连通性测试失败: {e}"));
                -1
            }
        }
    })
}

extern "C" fn test_json(raw: *const c_char) -> *mut c_char {
    guard_str(|| {
        let s = unsafe { sdk::from_c_str(raw) };
        match parse_target(&s) {
            Err(e) => json!({ "ok": false, "error": e }).to_string(),
            Ok(inst) => match inst.dispatch(
                cmd::QUERY_USER,
                json!({ "accessToken": inst.token }),
                Channel::ApiUser,
            ) {
                // 只回显是否连通，**不回显 token 或用户信息**（防敏感外泄）
                Ok(_) => json!({ "ok": true, "error": "" }).to_string(),
                Err(e) => json!({ "ok": false, "error": e }).to_string(),
            },
        }
    })
}

extern "C" fn last_error_json(th: *mut c_void) -> *mut c_char {
    guard_str(|| {
        if th.is_null() {
            return json!({ "error": "" }).to_string();
        }
        let inst = unsafe { &*(th as *const Wopan) };
        let e = inst.last_error.lock().map(|g| g.clone()).unwrap_or_default();
        json!({ "error": e }).to_string()
    })
}

// ════════════════════════════════════════════════════════════════════════════
// 云盘操作辅助
// ════════════════════════════════════════════════════════════════════════════

/// 列举某目录下的条目（宿主期望 `[{rel_path,size,mtime_secs,is_dir}]`）
fn list_files(inst: &Wopan, prefix: &str) -> Result<Vec<Value>, String> {
    let dir_id = if prefix.is_empty() {
        "0".to_string()
    } else {
        find_fid(inst, &join_remote(&inst.root, prefix).unwrap_or_default())
            .unwrap_or_else(|| "0".to_string())
    };
    let data = inst.dispatch(
        cmd::QUERY_ALL_FILES,
        json!({ "parentDirectoryId": dir_id, "spaceType": inst.space_type, "pageNum": 0, "pageSize": 1000, "sortRule": 0 }),
        Channel::WoHome,
    )?;
    let items = data
        .get("fileList")
        .or_else(|| data.get("files"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for it in items {
        let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        // 真实响应：`type` 0=目录 1=文件（`fileType` 是扩展名分类，目录为空串）
        let is_dir = it.get("type").and_then(|t| t.as_i64()).map(|t| t == 0).unwrap_or(false);
        let size = it.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        let mtime = it
            .get("createTime")
            .or_else(|| it.get("updateTime"))
            .and_then(|t| t.as_str())
            .and_then(parse_time)
            .unwrap_or(0);
        out.push(json!({
            "rel_path": join_remote(prefix, name).unwrap_or_else(|| name.to_string()),
            "size": size,
            "mtime_secs": mtime,
            "is_dir": is_dir,
        }));
    }
    Ok(out)
}

/// 找节点的 **`id`**（删除/移动等写操作用 id；只有下载用 fid）
fn find_id(inst: &Wopan, remote: &str) -> Option<String> {
    list_entries(inst, remote)?
        .into_iter()
        .find_map(|it| {
            it.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

/// 找目录的 id（找不到返回 None；根目录为 "0"）
fn find_dir_id(inst: &Wopan, remote: &str) -> Option<String> {
    if remote.trim_matches('/').is_empty() {
        return Some("0".to_string());
    }
    let items = match parent_entries(inst, remote) {
        Some(v) => v,
        None => return None,
    };
    let (_, name) = match remote.rfind('/') {
        Some(i) => (&remote[..i], &remote[i + 1..]),
        None => ("", remote),
    };
    for it in items {
        let n = it.get("name").and_then(|x| x.as_str()).unwrap_or("");
        if n != name {
            continue;
        }
        // 真实响应：`type` 0=目录 1=文件（`fileType` 是扩展名分类，目录为空串）
        let is_dir = it.get("type").and_then(|t| t.as_i64()).map(|t| t == 0).unwrap_or(false);
        if is_dir {
            return it.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
        }
    }
    None
}

/// 确保云盘内目录存在（逐层创建），返回其 id
fn ensure_remote_dir(inst: &Wopan, remote: &str) -> Result<String, String> {
    if remote.trim_matches('/').is_empty() {
        return Ok("0".to_string());
    }
    let mut cur = String::new();
    let mut last_id = "0".to_string();
    for seg in remote.split('/') {
        if seg.is_empty() {
            continue;
        }
        if !cur.is_empty() {
            cur.push('/');
        }
        cur.push_str(seg);
        if let Some(id) = find_dir_id(inst, &cur) {
            last_id = id;
            continue;
        }
        // 建这一层
        inst.dispatch(
            cmd::CREATE_DIRECTORY,
            json!({
                "directoryName": seg,
                "parentDirectoryId": last_id,
                "familyId": "0",
                "spaceType": inst.space_type,
            }),
            Channel::WoHome,
        )?;
        last_id = find_dir_id(inst, &cur).unwrap_or_else(|| "0".to_string());
    }
    Ok(last_id)
}

/// 列出某目录下所有条目（原始节点）
fn parent_entries(inst: &Wopan, remote: &str) -> Option<Vec<Value>> {
    let (parent, _) = match remote.rfind('/') {
        Some(i) => (&remote[..i], &remote[i + 1..]),
        None => ("", remote),
    };
    let dir_id = if parent.trim_matches('/').is_empty() {
        "0".to_string()
    } else {
        find_dir_id(inst, parent)?
    };
    let data = inst
        .dispatch(
            cmd::QUERY_ALL_FILES,
            json!({ "parentDirectoryId": dir_id, "spaceType": inst.space_type, "pageNum": 0, "pageSize": 1000, "sortRule": 0 }),
            Channel::WoHome,
        )
        .ok()?;
    Some(
        data.get("fileList")
            .or_else(|| data.get("files"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    )
}

/// 按完整路径取到该节点本身（在其父目录条目里按名字匹配）
fn list_entries(inst: &Wopan, remote: &str) -> Option<Vec<Value>> {
    let (_, name) = match remote.rfind('/') {
        Some(i) => (&remote[..i], &remote[i + 1..]),
        None => ("", remote),
    };
    let items = parent_entries(inst, remote)?;
    let matched: Vec<Value> = items
        .into_iter()
        .filter(|it| it.get("name").and_then(|x| x.as_str()).unwrap_or("") == name)
        .collect();
    Some(matched)
}

/// 按云盘内路径找节点的 **`fid`**（不是 `id`！误传 id 会得到 9999）
fn find_fid(inst: &Wopan, remote: &str) -> Option<String> {
    let (parent, name) = match remote.rfind('/') {
        Some(i) => (&remote[..i], &remote[i + 1..]),
        None => ("", remote),
    };
    if name.is_empty() {
        return Some("0".to_string()); // 根
    }
    let parent_id = if parent.is_empty() {
        "0".to_string()
    } else {
        find_fid(inst, parent)?
    };
    let data = inst
        .dispatch(
            cmd::QUERY_ALL_FILES,
            json!({ "parentDirectoryId": parent_id, "spaceType": inst.space_type, "pageNum": 0, "pageSize": 1000, "sortRule": 0 }),
            Channel::WoHome,
        )
        .ok()?;
    let items = data.get("fileList").or_else(|| data.get("files")).and_then(|v| v.as_array())?;
    for it in items {
        let n = it.get("name").and_then(|x| x.as_str()).unwrap_or("");
        if n == name {
            return it
                .get("fid")
                .or_else(|| it.get("fileId"))
                .and_then(|f| f.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}

/// 取下载直链（必须传 fid）
fn download_url(inst: &Wopan, fid: &str) -> Result<String, String> {
    let data = inst.dispatch(
        cmd::GET_DOWNLOAD_URL,
        json!({ "fidList": [fid], "spaceType": inst.space_type }),
        Channel::WoHome,
    )?;
    let arr = data.as_array().cloned().unwrap_or_else(|| {
        data.get("downloadList")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    });
    arr.first()
        .and_then(|x| x.get("downloadUrl").or_else(|| x.get("url")))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "响应中没有下载直链".to_string())
}

/// 时间串 → 秒（兼容毫秒数字串与 `YYYY-MM-DD …`）
fn parse_time(s: &str) -> Option<u64> {
    if let Ok(ms) = s.parse::<i64>() {
        return Some(if ms > 1_000_000_000_000 { ms / 1000 } else { ms } as u64);
    }
    let d: Vec<&str> = s.split(' ').next()?.split('-').collect();
    if d.len() != 3 {
        return None;
    }
    let (y, m, day): (i64, i64, i64) = (d[0].parse().ok()?, d[1].parse().ok()?, d[2].parse().ok()?);
    // 简化换算（不处理时区；仅用于 mtime 比较/排序）
    let days = (y - 1970) * 365 + (y - 1969) / 4 - (y - 1901) / 100 + (y - 1601) / 400 + (m - 1) * 30 + day;
    Some((days * 86400) as u64)
}

// ════════════════════════════════════════════════════════════════════════════
// 主表：元信息 / 动作 / 体检
// ════════════════════════════════════════════════════════════════════════════

extern "C" fn describe() -> *mut c_char {
    guard_str(|| include_str!("../describe.json").to_string())
}

extern "C" fn available(_cfg: *const c_char) -> *mut c_char {
    guard_str(|| json!({ "available": true, "reason": null }).to_string())
}

extern "C" fn action(action: *const c_char, request: *const c_char) -> *mut c_char {
    let action = unsafe { sdk::from_c_str(action) };
    let request = unsafe { sdk::from_c_str(request) };
    guard_str(move || match action.as_str() {
        "/sms/send" => sms_send(&request),
        "/sms/login" => sms_login(&request),
        "/user/info" => user_info_action(),
        "/recycle/list" => recycle_list_action(&request),
        "/recycle/empty" => recycle_empty_action(&request),
        other => sdk::json::error(&format!("未知动作：{other}")),
    })
}

/// 从自管配置构造实例；无令牌时给出可操作的提示
fn inst_from_self_config() -> Result<crate::instance::Wopan, String> {
    let cfg = config::SelfConfig::load();
    cfg.instance()
        .ok_or_else(|| "尚未保存令牌：请先用「用验证码登录」获取并保存令牌".to_string())
}

/// 用户信息（**脱敏**后返回：不回显 token，手机号打码）
fn user_info_action() -> String {
    let inst = match inst_from_self_config() {
        Ok(i) => i,
        Err(e) => return sdk::json::error(&e),
    };
    match inst.user_info() {
        Ok(v) => {
            let mask = |s: &str| -> String {
                let n = s.chars().count();
                if n <= 7 {
                    return "*".repeat(n);
                }
                let head: String = s.chars().take(3).collect();
                let tail: String = s.chars().skip(n - 4).collect();
                format!("{head}****{tail}")
            };
            let phone = v
                .get("userName")
                .or_else(|| v.get("phone"))
                .and_then(|x| x.as_str())
                .map(mask)
                .unwrap_or_default();
            json!({
                "success": true,
                "user": {
                    "name": v.get("userName").and_then(|x| x.as_str()).unwrap_or(""),
                    "phone_masked": phone,
                    // 只暴露容量相关（若存在），不整包回显
                    "total": v.get("totalSize").or_else(|| v.get("total")).and_then(|x| x.as_u64()),
                    "used": v.get("usedSize").or_else(|| v.get("used")).and_then(|x| x.as_u64()),
                }
            })
            .to_string()
        }
        Err(e) => sdk::json::error(&format!("查询用户信息失败: {e}")),
    }
}

/// 回收站列表
fn recycle_list_action(request: &str) -> String {
    let inst = match inst_from_self_config() {
        Ok(i) => i,
        Err(e) => return sdk::json::error(&e),
    };
    // 可选 pageNo/pageSize 从 body 取
    let body = serde_json::from_str::<Value>(request)
        .ok()
        .and_then(|v| v.get("body").cloned())
        .unwrap_or(Value::Null);
    let page_no = body.get("pageNo").and_then(|x| x.as_u64()).unwrap_or(1) as u32;
    let page_size = body.get("pageSize").and_then(|x| x.as_u64()).unwrap_or(20) as u32;

    match inst.query_recycle(page_no, page_size) {
        Ok(v) => {
            let arr = v.as_array().cloned().unwrap_or_default();
            let items: Vec<Value> = arr
                .iter()
                .map(|it| {
                    json!({
                        // 实测字段：name / fileSize / deleteTime / deleteNo / keepDays
                        "name": it.get("name").and_then(|x| x.as_str()).unwrap_or(""),
                        "size": it.get("fileSize").or_else(|| it.get("size")).and_then(|x| x.as_u64()).unwrap_or(0),
                        "delete_time": it.get("deleteTime").and_then(|x| x.as_str()).unwrap_or(""),
                        "keep_days": it.get("keepDays").and_then(|x| x.as_u64()),
                        "delete_no": it.get("deleteNo").and_then(|x| x.as_str()).unwrap_or(""),
                    })
                })
                .collect();
            json!({ "success": true, "count": items.len(), "items": items }).to_string()
        }
        Err(e) => sdk::json::error(&format!("查询回收站失败: {e}")),
    }
}

/// 清空回收站（**危险操作**：不可恢复）
///
/// 宿主侧前端已用 `confirm` 文案做二次确认（见 describe.json 的 danger 按钮）。
fn recycle_empty_action(request: &str) -> String {
    let inst = match inst_from_self_config() {
        Ok(i) => i,
        Err(e) => return sdk::json::error(&e),
    };
    // 显式确认位：避免误触发（前端 confirm 之外再加一道）
    let confirmed = serde_json::from_str::<Value>(request)
        .ok()
        .and_then(|v| v.get("body").cloned())
        .and_then(|b| b.get("confirm").and_then(|c| c.as_bool()))
        .unwrap_or(false);
    if !confirmed {
        return sdk::json::error("未确认：清空回收站不可恢复，需显式确认");
    }
    match inst.empty_recycle() {
        Ok(_) => sdk::json::ok_message("回收站已清空"),
        Err(e) => sdk::json::error(&format!("清空回收站失败: {e}")),
    }
}

/// 发送短信验证码（独立端点，不走 dispatcher）
fn sms_send(request: &str) -> String {
    let v: Value = match serde_json::from_str(request) {
        Ok(v) => v,
        Err(_) => return sdk::json::error("请求不是合法 JSON"),
    };
    let phone = v
        .get("body")
        .and_then(|b| b.get("phone"))
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .trim();
    if phone.is_empty() {
        return sdk::json::error("缺少手机号");
    }
    let payload = json!({ "operateType": "1", "phone": phone, "uuid": protocol::js_uuid(), "verifyCode": "" });
    let param = match protocol::encrypt(&payload.to_string(), protocol::H5_DEFAULT_SECRET_KEY) {
        Ok(p) => p,
        Err(e) => return sdk::json::error(&format!("加密失败: {e}")),
    };
    let body = json!({ "func": "pc_send", "clientId": protocol::H5_CLIENT_ID, "param": param });
    let url = format!("{}{}", protocol::BASE_URL, "/api-user/sendMessageCodeBase");
    match reqwest::blocking::Client::new().post(&url).json(&body).send() {
        Ok(r) => match r.json::<Value>() {
            Ok(resp) => {
                let rsp = resp.get("RSP").cloned().unwrap_or(Value::Null);
                let code = rsp.get("RSP_CODE").and_then(|c| c.as_str()).unwrap_or("");
                if code == protocol::code::OK {
                    sdk::json::ok_message("验证码已发送，请查收短信")
                } else {
                    let desc = rsp.get("RSP_DESC").and_then(|d| d.as_str()).unwrap_or("");
                    sdk::json::error(&format!("发送失败（{code}）：{desc}"))
                }
            }
            Err(e) => sdk::json::error(&format!("响应解析失败: {e}")),
        },
        Err(e) => sdk::json::error(&format!("请求失败: {e}")),
    }
}

/// 用短信验证码登录（H5 渠道）→ 返回 access_token
///
/// ⚠️ token 会显示在界面上供用户填入目标配置（必要代价）；**插件自身不保存它**。
fn sms_login(request: &str) -> String {
    let v: Value = match serde_json::from_str(request) {
        Ok(v) => v,
        Err(_) => return sdk::json::error("请求不是合法 JSON"),
    };
    let body = v.get("body").cloned().unwrap_or(Value::Null);
    let phone = body.get("phone").and_then(|p| p.as_str()).unwrap_or("").trim();
    let code = body
        .get("sms_code")
        .or_else(|| body.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim();
    if phone.is_empty() || code.is_empty() {
        return sdk::json::error("缺少手机号或短信验证码");
    }
    let key = protocol::h5_secret(protocol::H5_CLIENT_ID);
    let payload = json!({ "phone": phone, "smsCode": code, "clientSecret": key });
    let res_time = instance::now_ms();
    let req_seq = 100000 + (res_time % 89999) as i64;
    let envelope = match protocol::build_envelope(
        cmd::LOGIN_SMS_V2,
        &payload,
        Some(protocol::H5_CLIENT_ID),
        key,
        Channel::ApiUser.as_str(),
        res_time,
        req_seq,
    ) {
        Ok(e) => e,
        Err(e) => return sdk::json::error(&format!("构造请求失败: {e}")),
    };
    let url = format!("{}{}", protocol::BASE_URL, Channel::ApiUser.path());
    match reqwest::blocking::Client::new().post(&url).json(&envelope).send() {
        Ok(r) => match r.json::<Value>() {
            Ok(resp) => {
                let rsp = resp.get("RSP").cloned().unwrap_or(Value::Null);
                let rc = rsp.get("RSP_CODE").and_then(|c| c.as_str()).unwrap_or("");
                if rc != protocol::code::OK {
                    let desc = rsp.get("RSP_DESC").and_then(|d| d.as_str()).unwrap_or("");
                    return sdk::json::error(&format!("登录失败（{rc}）：{desc}"));
                }
                let token = rsp
                    .get("DATA")
                    .and_then(|d| d.as_str())
                    .and_then(|c| protocol::decrypt(c, key).ok())
                    .and_then(|plain| serde_json::from_str::<Value>(&plain).ok())
                    .and_then(|d| {
                        d.get("access_token")
                            .or_else(|| d.get("accessToken"))
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                    });
                match token {
                    Some(t) if !t.is_empty() => {
                        // 顺手把令牌存进**插件自管配置**（宿主 seal 加密落盘），
                        // 这样插件页的「用户信息」「回收站」无需再让用户粘一次。
                        // 存失败不影响本次登录结果，只提示。
                        let mut c = config::SelfConfig::load();
                        c.set_token(&t);
                        c.phone = phone.to_string();
                        let saved = match c.save() {
                            Ok(()) => true,
                            Err(e) => {
                                eprintln!("[wopan] 令牌未能持久化: {e}");
                                false
                            }
                        };
                        // ⚠️ 仍返回 token：用户可能想填进「目标」配置。
                        //    但**不回显**是否保存失败的原因细节，避免噪音。
                        json!({
                            "success": true,
                            "token": t,
                            "saved": saved,
                            "message": if saved {
                                "登录成功，令牌已加密保存；可直接在插件页查看用户信息/清理回收站"
                            } else {
                                "登录成功；令牌未能保存（可手动填入目标配置）"
                            }
                        })
                        .to_string()
                    }
                    _ => sdk::json::error("登录成功但未取得 access_token"),
                }
            }
            Err(e) => sdk::json::error(&format!("响应解析失败: {e}")),
        },
        Err(e) => sdk::json::error(&format!("请求失败: {e}")),
    }
}

extern "C" fn health(_cfg: *const c_char) -> *mut c_char {
    guard_str(|| {
        let mut checks = vec![json!({
            "key": "wopan_target",
            "title": "联通云盘（沃云盘）备份目标",
            "status": "ok",
            "detail": "目标能力表已加载（fn_kzwr_plugin_target_v1）；H5 端协议，支持备份写入（8MB 分片直传）、列举、恢复、删除。",
            "hint": null
        })];

        // 有自管令牌时，顺带做一次登录态体检（拿用户信息）。
        // 体检**不得阻塞太久**：这里已有令牌，一次请求即可。
        let cfg = config::SelfConfig::load();
        if let Some(inst) = cfg.instance() {
            match inst.user_info() {
                Ok(v) => {
                    let name = v.get("userName").and_then(|x| x.as_str()).unwrap_or("");
                    let used = v.get("usedSize").or_else(|| v.get("used")).and_then(|x| x.as_u64());
                    let total = v.get("totalSize").or_else(|| v.get("total")).and_then(|x| x.as_u64());
                    let detail = match (used, total) {
                        (Some(u), Some(t)) if t > 0 => format!(
                            "登录态有效（{name}）；容量 {:.1}/{:.1} GB",
                            u as f64 / 1e9,
                            t as f64 / 1e9
                        ),
                        _ => format!("登录态有效（{name}）"),
                    };
                    checks.push(json!({
                        "key": "wopan_login",
                        "title": "登录态",
                        "status": "ok",
                        "detail": detail,
                        "hint": null
                    }));
                }
                Err(e) => {
                    checks.push(json!({
                        "key": "wopan_login",
                        "title": "登录态",
                        "status": "warn",
                        "detail": format!("令牌不可用：{e}"),
                        "hint": "在插件页用「短信登录」重新获取令牌（H5 渠道约 60 天有效）"
                    }));
                }
            }
        } else {
            checks.push(json!({
                "key": "wopan_login",
                "title": "登录态",
                "status": "warn",
                "detail": "尚未保存令牌",
                "hint": "在插件页点「发送短信验证码」→「用验证码登录」"
            }));
        }
        serde_json::to_string(&checks).unwrap_or_else(|_| "[]".to_string())
    })
}

// 导出两张表：主表（元信息/UI/动作）+ 目标表（传输能力）
sdk::export_plugin_v1!(describe, available, action, health);

sdk::export_target_v1!(
    target_open,
    Some(target_close),
    write_begin,
    write_chunk,
    write_end,
    Some(write_abort),
    read_begin,
    read_chunk,
    Some(read_end),
    list_json,
    delete,
    Some(ensure_dir),
    Some(ping),
    Some(test_json),
    Some(last_error_json),
    None, // plan_begin
    None, // plan_next
    None, // plan_end
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_is_valid_json() {
        let v: Value = serde_json::from_str(include_str!("../describe.json")).expect("describe.json 必须合法");
        assert_eq!(v["id"], "wopan");
        assert_eq!(v["kind"], "target");
        assert_eq!(v["runtime"]["target"], "fn_kzwr_plugin_target_v1");
    }

    /// 上传未实现时必须返回 NULL（宿主据此报错），绝不能给假句柄
    #[test]
    /// 空实例句柄调用 write_begin 必须返回 NULL（而不是崩溃或伪造句柄）
    fn write_begin_null_instance_is_null() {
        assert!(write_begin(std::ptr::null_mut(), std::ptr::null(), 0).is_null());
    }

    #[test]
    fn null_handle_is_safe() {
        assert_eq!(read_chunk(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), 0), 0);
    }

    // ══════════════════════════════════════════════════════════════════════
    // **真实联网**测试（默认不跑，需真实 token）
    //   WOPAN_TOKEN=<token> cargo test --lib -- --ignored --nocapture
    // 只打印脱敏信息，绝不输出 token 明文。
    // ══════════════════════════════════════════════════════════════════════

    fn live_instance() -> Option<Wopan> {
        let token = std::env::var("WOPAN_TOKEN").ok()?;
        if token.is_empty() {
            return None;
        }
        let j = json!({
            "username": std::env::var("WOPAN_PHONE").unwrap_or_default(),
            "password": token,
            "url": "",
            "config": { "space_type": "0" },
        })
        .to_string();
        parse_target(&j).ok()
    }

    /// 诊断：逐一试探 wohome 命令字是否可达
    ///
    /// ⚠️ 这里**只用 `inst.dispatch()`**（它带 `accesstoken` 头）。
    ///    不要在这里手写裸请求做"clientId 摆放"对照 —— 裸请求缺 `accesstoken`
    ///    头会一律返回 1001，得出的结论是假的（曾据此误判过一轮）。
    #[test]
    #[ignore]
    /// 真实上传往返：建目录 → 上传 → 校验云端大小 → 下载比对 → 删除
    ///
    /// ⚠️ 会真实写入账号（写在 `_kzwr_e2e/`），跑完自行清理。
    #[test]
    #[ignore]
    fn live_upload_roundtrip() {
        let inst = match live_instance() {
            Some(i) => i,
            None => {
                println!("跳过：未设 WOPAN_TOKEN");
                return;
            }
        };
        // 用 9MB 以上触发分片（8MB 步长）才算真的测到分片逻辑
        let payload = vec![b'K'; 12_000_000];
        println!("准备上传 {} 字节（应分 2 片：8MB + 余量）", payload.len());

        let dir = "_kzwr_e2e";
        let th = Box::into_raw(Box::new(inst)) as *mut std::ffi::c_void;
        let rel = format!("{dir}/rt.bin");
        let rel_c = std::ffi::CString::new(rel.clone()).unwrap();

        // write_begin → write_chunk → write_end（走宿主同款路径）
        let h = write_begin(th, rel_c.as_ptr(), payload.len() as u64);
        assert!(!h.is_null(), "write_begin 返回 NULL：{}", unsafe {
            sdk::from_c_str(last_error_json(th))
        });
        // 分多次喂，模拟宿主分块推送
        let mut off = 0usize;
        while off < payload.len() {
            let n = 1_048_576.min(payload.len() - off); // 每次 1MB
            let r = write_chunk(th, h, payload[off..].as_ptr(), n as u32);
            assert!(r > 0, "write_chunk 失败");
            off += n;
        }
        let total = write_end(th, h);
        assert!(total == payload.len() as i64, "write_end={total} 期望 {}", payload.len());
        println!("[1] 上传完成：{total} 字节");

        let inst_ref = unsafe { &*(th as *const Wopan) };

        // 校验云端大小：用 Range 只取 1 字节读 Content-Range
        let fid = find_fid(inst_ref, &rel).expect("取 fid");
        let url = download_url(inst_ref, &fid).expect("取直链");
        let resp = reqwest::blocking::Client::new()
            .get(&url)
            .header("User-Agent", "Mozilla/5.0")
            .header("Range", "bytes=0-0")
            .send()
            .expect("Range 请求");
        let cr = resp
            .headers()
            .get("Content-Range")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        println!("[2] Content-Range = {cr}");
        assert!(
            cr.contains(&format!("/{}", payload.len())),
            "❌ 云端大小不符（可能被静默截断）: {cr}"
        );

        // 下载全文比对长度（内容一致性）
        let bytes = reqwest::blocking::Client::new()
            .get(&url)
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .expect("下载")
            .bytes()
            .expect("读取")
            .to_vec();
        assert_eq!(bytes.len(), payload.len(), "下载长度不符");
        assert!(bytes == payload, "下载内容与上传不一致");
        println!("[3] 下载比对一致（{} 字节）", bytes.len());

        // 清理
        let id = find_id(inst_ref, &rel).expect("取 id");
        inst_ref
            .dispatch(
                cmd::DELETE_FILE,
                json!({"fileList":[id],"dirList":[],"spaceType": inst_ref.space_type}),
                Channel::WoHome,
            )
            .expect("删除");
        println!("[4] 已清理测试文件");
    }

    /// 真实联网：用户信息 + 回收站列表（**只读**，不改动账号数据）
    #[test]
    #[ignore]
    fn live_user_and_recycle() {
        let inst = match live_instance() { Some(i)=>i, None=>{println!("跳过");return;} };
        // 1) 用户信息
        match inst.user_info() {
            Ok(v) => {
                println!("[1] 用户信息 keys={:?}", v.as_object().map(|o| o.keys().collect::<Vec<_>>()));
                println!("     userName={:?}", v.get("userName").and_then(|x|x.as_str()));
                println!("     原始（截断）={}", v.to_string().chars().take(300).collect::<String>());
            }
            Err(e) => println!("[1] 用户信息失败: {e}"),
        }
        // 2) 回收站列表
        match inst.query_recycle(1, 20) {
            Ok(v) => {
                let arr = v.as_array().cloned().unwrap_or_default();
                println!("[2] 回收站 {} 项", arr.len());
                for it in arr.iter().take(3) {
                    println!("     原始项={}", it.to_string().chars().take(220).collect::<String>());
                }
            }
            Err(e) => println!("[2] 回收站失败: {e}"),
        }
    }

    /// 真实联网：清空回收站（**危险**，不可恢复）
    #[test]
    #[ignore]
    fn live_empty_recycle() {
        let inst = match live_instance() { Some(i)=>i, None=>{println!("跳过");return;} };
        // 先看清理前有多少项
        let before = inst.query_recycle(1, 50).ok()
            .and_then(|v| v.as_array().map(|a| a.len())).unwrap_or(0);
        println!("[1] 清空前回收站 {} 项", before);
        match inst.empty_recycle() {
            Ok(v) => println!("[2] EmptyRecycleData OK: {}", v.to_string().chars().take(200).collect::<String>()),
            Err(e) => { println!("[2] 清空失败: {e}"); return; }
        }
        // 再查应为空
        match inst.query_recycle(1, 50) {
            Ok(v) => {
                let after = v.as_array().map(|a| a.len()).unwrap_or(0);
                println!("[3] 清空后回收站 {} 项", after);
                assert_eq!(after, 0, "清空后应为空");
            }
            Err(e) => println!("[3] 复查失败: {e}"),
        }
    }

    #[test]
    #[ignore]
    fn live_diag() {
        let inst = match live_instance() {
            Some(i) => i,
            None => {
                println!("跳过：未设 WOPAN_TOKEN");
                return;
            }
        };
        println!("client_id={} token_len={}", inst.client_id, inst.token.len());

        match inst.dispatch(cmd::QUERY_USER, json!({ "accessToken": inst.token }), Channel::ApiUser) {
            Ok(v) => println!("[api-user] AppQueryUser OK: userName={:?}", v.get("userName").and_then(|x| x.as_str())),
            Err(e) => println!("[api-user] 失败: {e}"),
        }

        for c in ["QueryMuid", "QueryAllFiles", "QueryDirectorys", "GetZoneInfo"] {
            let p = match c {
                "QueryAllFiles" => json!({"parentDirectoryId":"","spaceType":"0","pageNum":0,"pageSize":10,"sortRule":0}),
                "QueryDirectorys" => json!({"spaceType":"0"}),
                "GetZoneInfo" => json!({"appId": protocol::upload::ZONE_APP_ID}),
                _ => json!({}),
            };
            match inst.dispatch(c, p, Channel::WoHome) {
                Ok(v) => println!("[wohome] {c}: OK keys={:?}", v.as_object().map(|o| o.keys().collect::<Vec<_>>())),
                Err(e) => println!("[wohome] {c}: {e}"),
            }
        }
    }
}
