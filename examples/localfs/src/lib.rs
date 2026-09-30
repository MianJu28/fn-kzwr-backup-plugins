//! 酷族备份 · 外置插件示例 —— **自定义备份目标（本地目录）**
//!
//! 这是「外置插件也能提供备份目标」的**参考实现**，用 [`sdk::export_target_v1!`]
//! 导出目标能力表 `fn_kzwr_plugin_target_v1`。宿主只给它 **age 密文**（推块），
//! 明文与密钥永不离开宿主，插件也不需要回调宿主。
//!
//! ## 怎么用
//!
//! 1. 构建：`bash Scripts/build_plugins.sh` → `libfn_kzwr_plugin_example_localfs.so`
//! 2. 安装：放进 `$TRIM_PKGETC/plugins/`，设置页开启「外置插件加载」后**重启应用**
//! 3. 建目标：`POST /api/targets {kind:"example-localfs", url:"/某个本地目录", username:"x", password:"x"}`
//!    —— 本插件把 **`url` 当作本地目录路径**用（不需要真实账号，随便填即可）
//!
//! ## 本文件同时演示了三件事
//!
//! - **路径越权防护**：`resolve()` 拒绝 `..` 与绝对路径逃逸，所有写入都在 root 内
//! - **并发回传**：`plan_*` 让插件自己决定分批（本例每批 10 个，按大小升序：小文件先传）
//! - **panic 不跨 FFI**：所有回调都包了 `catch_unwind`
//!
//! 契约细节见仓库 `docs/PLUGIN_ABI.md` §9。

use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use fn_kzwr_plugin_sdk as sdk;
use serde_json::{json, Value};

/// 每批发放的文件数（宿主编内按 `max_parallel` 并发上传这些文件）
const PLAN_BATCH: usize = 10;

// ════════════════════════════════════════════════════════════════════════════
// 实例与句柄
// ════════════════════════════════════════════════════════════════════════════

/// `target_open` 创建的实例：一个本地目录根
struct LocalFs {
    root: PathBuf,
    last_error: Mutex<String>,
}

/// 一次写入的句柄：先写 `.part` 临时文件，`write_end` 时改名为正式文件
struct WriteH {
    tmp: PathBuf,
    final_path: PathBuf,
    file: Option<File>,
    fed: u64,
}

/// 一次读取的句柄：整文件读入内存（示例简化；大文件可改成分块读）
struct ReadH {
    data: Vec<u8>,
    pos: usize,
}

/// 一次规划（并发回传）的句柄
struct PlanH {
    items: Vec<String>,
    pos: usize,
}

impl LocalFs {
    fn err(&self, s: String) {
        if let Ok(mut g) = self.last_error.lock() {
            *g = s;
        }
    }
}

/// **路径安全**：把目标端相对路径解析到 root 内，拒绝任何越权（`..` / 绝对路径逃逸）
fn resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    let cleaned = rel.trim_start_matches('/');
    if cleaned.is_empty() {
        return None;
    }
    let mut out = root.to_path_buf();
    for seg in cleaned.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return None; // 越权一律拒绝
        }
        out.push(seg);
    }
    // 双保险：结果必须仍在 root 之下
    if !out.starts_with(root) {
        return None;
    }
    Some(out)
}

fn mtime_secs(p: &Path) -> u64 {
    fs::metadata(p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 递归列出目录下的文件（相对 root 的路径）
fn walk(root: &Path, dir: &Path, out: &mut Vec<Value>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if path.is_dir() {
            out.push(json!({"rel_path": rel, "size": 0, "mtime_secs": mtime_secs(&path), "is_dir": true}));
            walk(root, &path, out);
        } else {
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push(json!({"rel_path": rel, "size": size, "mtime_secs": mtime_secs(&path), "is_dir": false}));
        }
    }
}

/// 取出目录根：优先 `config.root`，其次把 `url` 当路径用
///
/// 之后若本目标声明了 `subdir` 字段（见 `describe_json.target.form`），再拼一层子目录。
/// `subdir` 来自宿主按**目标**存储的自定义字段，会合并进 `config`（同名时目标级优先），
/// 因此这里从 `config.subdir` 读即可 —— 与读插件级配置同一路径，插件无需区分来源。
fn root_from(json_str: &str) -> Option<PathBuf> {
    let v: Value = serde_json::from_str(json_str).ok()?;
    let cfg_root = v
        .get("config")
        .and_then(|c| c.get("root"))
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());
    let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("");
    let chosen = cfg_root.unwrap_or_else(|| url.trim_end_matches('/').to_string());
    if chosen.is_empty() {
        return None;
    }
    let mut root = PathBuf::from(chosen);
    // 目标级子目录（可选）：只接受单层相对名，防越权逃逸
    if let Some(sub) = v.get("config").and_then(|c| c.get("subdir")).and_then(|s| s.as_str()) {
        let sub = sub.trim().trim_matches('/');
        if !sub.is_empty()
            && !sub.contains("..")
            && !sub.contains('/')
            && !sub.contains('\\')
        {
            root.push(sub);
        }
    }
    Some(root)
}

// ════════════════════════════════════════════════════════════════════════════
// panic 兜底（**绝不让 panic 跨 FFI 边界**）
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

fn guard_i64(f: impl FnOnce() -> i64) -> i64 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(-1)
}

// ════════════════════════════════════════════════════════════════════════════
// 目标能力表实现
// ════════════════════════════════════════════════════════════════════════════

extern "C" fn target_open(raw: *const c_char) -> *mut c_void {
    guard_ptr(|| {
        let s = unsafe { sdk::from_c_str(raw) };
        let root = root_from(&s)?;
        if let Err(e) = fs::create_dir_all(&root) {
            // 目录不可创建也算配置无效
            let _ = e;
            return None;
        }
        let inst = Box::new(LocalFs { root, last_error: Mutex::new(String::new()) });
        Some(Box::into_raw(inst) as *mut c_void)
    })
}

extern "C" fn target_close(th: *mut c_void) {
    if !th.is_null() {
        unsafe {
            drop(Box::from_raw(th as *mut LocalFs));
        }
    }
}

extern "C" fn write_begin(th: *mut c_void, rel: *const c_char, _total: u64) -> *mut c_void {
    guard_ptr(|| {
        let inst = unsafe { &*(th as *const LocalFs) };
        let rel = unsafe { sdk::from_c_str(rel) };
        let final_path = resolve(&inst.root, &rel)?;
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent).ok()?;
        }
        // 先写临时文件，write_end 成功后再改名 —— 避免半成品被当成正式备份
        let tmp = final_path.with_extension("part");
        let file = File::create(&tmp).ok()?;
        let h = Box::new(WriteH { tmp, final_path, file: Some(file), fed: 0 });
        Some(Box::into_raw(h) as *mut c_void)
    })
}

extern "C" fn write_chunk(th: *mut c_void, h: *mut c_void, buf: *const u8, len: u32) -> i32 {
    guard_i32(|| {
        if th.is_null() || h.is_null() || buf.is_null() || len == 0 {
            return 0;
        }
        let h = unsafe { &mut *(h as *mut WriteH) };
        let chunk = unsafe { std::slice::from_raw_parts(buf, len as usize) };
        match h.file.as_mut() {
            Some(f) => match f.write_all(chunk) {
                Ok(()) => {
                    h.fed += len as u64;
                    len as i32
                }
                Err(_) => -1,
            },
            None => -1,
        }
    })
}

extern "C" fn write_end(th: *mut c_void, h: *mut c_void) -> i64 {
    guard_i64(|| {
        if th.is_null() || h.is_null() {
            return -1;
        }
        let inst = unsafe { &*(th as *const LocalFs) };
        let mut h = unsafe { Box::from_raw(h as *mut WriteH) };
        // 关闭句柄 → 改名落定
        drop(h.file.take());
        match fs::rename(&h.tmp, &h.final_path) {
            Ok(()) => h.fed as i64,
            Err(e) => {
                inst.err(format!("落盘失败: {e}"));
                -1
            }
        }
    })
}

extern "C" fn write_abort(_th: *mut c_void, h: *mut c_void) {
    if h.is_null() {
        return;
    }
    let mut h = unsafe { Box::from_raw(h as *mut WriteH) };
    drop(h.file.take());
    let _ = fs::remove_file(&h.tmp);
}

extern "C" fn read_begin(th: *mut c_void, rel: *const c_char) -> *mut c_void {
    guard_ptr(|| {
        let inst = unsafe { &*(th as *const LocalFs) };
        let rel = unsafe { sdk::from_c_str(rel) };
        let path = resolve(&inst.root, &rel)?;
        let mut f = OpenOptions::new().read(true).open(&path).ok()?;
        let mut data = Vec::new();
        f.read_to_end(&mut data).ok()?;
        let h = Box::new(ReadH { data, pos: 0 });
        Some(Box::into_raw(h) as *mut c_void)
    })
}

extern "C" fn read_chunk(_th: *mut c_void, h: *mut c_void, buf: *mut u8, cap: u32) -> i32 {
    guard_i32(|| {
        if h.is_null() || buf.is_null() || cap == 0 {
            return 0;
        }
        let r = unsafe { &mut *(h as *mut ReadH) };
        let remain = r.data.len().saturating_sub(r.pos);
        if remain == 0 {
            return 0; // EOF
        }
        let n = remain.min(cap as usize);
        let dst = unsafe { std::slice::from_raw_parts_mut(buf, n) };
        dst.copy_from_slice(&r.data[r.pos..r.pos + n]);
        r.pos += n;
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

extern "C" fn list_json(th: *mut c_void, prefix: *const c_char) -> *mut c_char {
    guard_str(|| {
        if th.is_null() {
            return "[]".to_string();
        }
        let inst = unsafe { &*(th as *const LocalFs) };
        let prefix = unsafe { sdk::from_c_str(prefix) };
        let base = if prefix.trim().is_empty() {
            inst.root.clone()
        } else {
            match resolve(&inst.root, &prefix) {
                Some(p) => p,
                None => return "[]".to_string(),
            }
        };
        let mut out = Vec::new();
        walk(&inst.root, &base, &mut out);
        serde_json::to_string(&out).unwrap_or_else(|_| "[]".to_string())
    })
}

extern "C" fn delete(th: *mut c_void, path: *const c_char) -> i32 {
    guard_i32(|| {
        let inst = unsafe { &*(th as *const LocalFs) };
        let rel = unsafe { sdk::from_c_str(path) };
        let Some(p) = resolve(&inst.root, &rel) else {
            return -1;
        };
        if p.is_dir() {
            if fs::remove_dir_all(&p).is_ok() {
                0
            } else {
                -1
            }
        } else if fs::remove_file(&p).is_ok() {
            0
        } else {
            -1
        }
    })
}

extern "C" fn ensure_dir(th: *mut c_void, path: *const c_char) -> i32 {
    guard_i32(|| {
        let inst = unsafe { &*(th as *const LocalFs) };
        let rel = unsafe { sdk::from_c_str(path) };
        let Some(p) = resolve(&inst.root, &rel) else {
            return -1;
        };
        if fs::create_dir_all(&p).is_ok() {
            0
        } else {
            -1
        }
    })
}

extern "C" fn ping(th: *mut c_void) -> i32 {
    guard_i32(|| {
        let inst = unsafe { &*(th as *const LocalFs) };
        // 可写探针：写一个临时文件再删掉
        let probe = inst.root.join(".kzwr-ping");
        let ok = File::create(&probe).is_ok();
        let _ = fs::remove_file(&probe);
        if ok {
            0
        } else {
            -1
        }
    })
}

extern "C" fn test_json(raw: *const c_char) -> *mut c_char {
    guard_str(|| {
        let s = unsafe { sdk::from_c_str(raw) };
        match root_from(&s) {
            Some(root) => match fs::create_dir_all(&root) {
                Ok(()) => json!({ "root": root.to_string_lossy() }).to_string(),
                Err(e) => sdk::json::error(&format!("目录不可用: {e}")),
            },
            None => sdk::json::error("未指定目录（填目标地址 url 即可）"),
        }
    })
}

extern "C" fn last_error_json(th: *mut c_void) -> *mut c_char {
    guard_str(|| {
        if th.is_null() {
            return json!({ "error": "" }).to_string();
        }
        let inst = unsafe { &*(th as *const LocalFs) };
        let e = inst.last_error.lock().map(|g| g.clone()).unwrap_or_default();
        json!({ "error": e }).to_string()
    })
}

// ── 并发回传：本插件按「小文件优先」排序后每批发 10 个 ──

extern "C" fn plan_begin(_th: *mut c_void, job: *const c_char) -> *mut c_void {
    guard_ptr(|| {
        let s = unsafe { sdk::from_c_str(job) };
        let v: Value = serde_json::from_str(&s).ok()?;
        let mut items: Vec<(String, u64)> = v
            .get("upload")
            .and_then(|u| u.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let p = x.get("rel_path")?.as_str()?.to_string();
                        let sz = x.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                        Some((p, sz))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // 小文件先传：让面板更早看到完成数增长
        items.sort_by_key(|(_, sz)| *sz);
        let h = Box::new(PlanH { items: items.into_iter().map(|(p, _)| p).collect(), pos: 0 });
        Some(Box::into_raw(h) as *mut c_void)
    })
}

extern "C" fn plan_next(_th: *mut c_void, ph: *mut c_void) -> *mut c_char {
    guard_str(|| {
        if ph.is_null() {
            return "[]".to_string();
        }
        let p = unsafe { &mut *(ph as *mut PlanH) };
        let end = (p.pos + PLAN_BATCH).min(p.items.len());
        let batch: Vec<&str> = p.items[p.pos..end].iter().map(|s| s.as_str()).collect();
        p.pos = end;
        serde_json::to_string(&batch).unwrap_or_else(|_| "[]".to_string())
    })
}

extern "C" fn plan_end(_th: *mut c_void, ph: *mut c_void) {
    if !ph.is_null() {
        unsafe {
            drop(Box::from_raw(ph as *mut PlanH));
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 主表（增强 + 元信息）：目标插件同样需要提供 describe，宿主据此发现目标能力
// ════════════════════════════════════════════════════════════════════════════

extern "C" fn describe() -> *mut c_char {
    guard_str(|| {
        json!({
            "id": "example-localfs",
            "name": "示例目标：本地目录",
            "version": env!("CARGO_PKG_VERSION"),
            "kind": "target",
            "description": "外置插件提供的备份目标：把加密后的备份写到本机某个目录（用于演示自定义目标与并发回传）",
            "runtime": { "target": "fn_kzwr_plugin_target_v1" },
            "target": {
                "write": "push",
                "supports_plan": true,
                "max_parallel": 4,
                "preferred_chunk_kib": 1024,
                // 本地目录目标**不用凭据**：只认一个路径。
                // 不声明的话宿主会强制要求账号密码，用户在「目标」页建不出本插件目标。
                "needs_credentials": false,
                // 告诉前端 `url` 字段的实际语义（否则标签是「地址」，用户不知道该填路径）
                "url_label": "目录路径",
                "url_placeholder": "/vol1/backup/kzwr-localfs",
                "url_hint": "本机目录的绝对路径；插件只写入 age 密文，明文不会离开宿主机",

                // **沙箱白名单**：声明 `url` 的值是「本机路径」。
                // 宿主据此把该路径加入该目标实例的沙箱可读写白名单
                // —— 否则沙箱会挡住插件的正常写入（误伤）。
                // 对比：webdav 的 url 是远程 URL，故**不声明**。
                "path_fields": ["url"],

                // ── 「新建/编辑目标」弹窗的字段（完全由本插件声明）──────────────
                //
                // 宿主只渲染与存取，不解释语义。三个 well-known 键映射到既有存储：
                //   url → TargetConfig.url，username/password → 加密凭据
                // 其余键存入**本目标自己的** TargetConfig.fields（按目标，互不共用），
                // 并注入 target_json.config（同时也在 target_json.fields）。
                "form": [
                    {
                        "key": "url",
                        "label": "目录路径",
                        "kind": "text",
                        "required": true,
                        "placeholder": "/vol1/backup/kzwr-localfs",
                        "hint": "本机目录的绝对路径；插件只写入 age 密文，明文不会离开宿主机"
                    },
                    {
                        // 本插件自己的字段：验证「非 well-known 键按目标存储」这条路径
                        "key": "subdir",
                        "label": "子目录（可选）",
                        "kind": "text",
                        "placeholder": "my-backups",
                        "hint": "在该路径下再建一层子目录；留空则直接写在根目录",
                        "default": ""
                    },
                    {
                        // 演示敏感字段：加密存储、回显只给「是否已设置」
                        "key": "passphrase_hint",
                        "label": "备注口令（演示加密字段）",
                        "kind": "password",
                        "hint": "演示用：敏感字段加密落盘，界面上只显示「已设置」"
                    },
                    {
                        "key": "keep_local",
                        "label": "保留本机明文副本",
                        "kind": "toggle",
                        "hint": "演示布尔字段；本插件当前不实现该行为"
                    }
                ]
            },
            "ui": {
                "title": "示例目标：本地目录",
                "blocks": [
                    {
                        "type": "tips",
                        "text": "这个备份目标来自**外置插件**（.so，稳定 C ABI）。在「目标」页新建目标时选它，地址填一个本机目录即可；插件只接收加密后的密文。"
                    },
                    {
                        "type": "metric",
                        "label": "并发回传",
                        "value": "已支持（每批 10 个）",
                        "hint": "插件自己决定分批顺序（小文件优先），宿主按并发度上传"
                    },
                    {
                        // 目录等设置**不再由宿主代存**（ADR-021）：它们是**目标自己的属性**，
                        // 已在「新建/编辑目标」弹窗里按 `target.form` 声明（见上方 form）。
                        // 因此这里不提供表单块（`scope` 字段本身也已从 ABI 移除）。
                        "type": "tips",
                        "text": "目录路径与子目录在「目标」页每个目标自己的编辑弹窗里填写（可建多个目标指向不同目录）。"
                    }
                ]
            }
        })
        .to_string()
    })
}

extern "C" fn available(_cfg: *const c_char) -> *mut c_char {
    guard_str(|| json!({ "available": true, "reason": null }).to_string())
}

extern "C" fn action(action: *const c_char, _request: *const c_char) -> *mut c_char {
    let action = unsafe { sdk::from_c_str(action) };
    guard_str(move || match action.as_str() {
        "hello" => sdk::json::ok_message("本地目录目标插件在线（稳定 C ABI）"),
        other => sdk::json::error(&format!("未知动作：{other}")),
    })
}

extern "C" fn health(_cfg: *const c_char) -> *mut c_char {
    guard_str(|| {
        json!([{
            "key": "example_localfs",
            "title": "示例目标：本地目录",
            "status": "ok",
            "detail": "目标能力表已加载（fn_kzwr_plugin_target_v1）；支持并发回传",
            "hint": null
        }])
        .to_string()
    })
}

// 导出两张表：主表（元信息/UI）+ 目标表（传输能力）
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
    Some(plan_begin),
    Some(plan_next),
    Some(plan_end),
);

/// 说明：宿主注入的自管配置从 `target_json.config` 读取（见 [`root_from`]）。
/// 本例直接用 `url` 当目录，因此不强制依赖自管配置。
#[allow(dead_code)]
fn _doc_config() {}
