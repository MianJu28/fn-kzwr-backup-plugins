//! 酷族备份 · 外置插件**稳定 C ABI** SDK（v1）
//!
//! 用这个 crate 写插件可以做到：**宿主升级后不需要重新编译插件**（只要 ABI v1 不变）。
//! 原因：跨边界只传 **C 类型 + UTF-8 JSON**，不传 Rust trait 对象（Rust 没有稳定 ABI）。
//!
//! 本 crate **零第三方依赖**，把契约钉死在下面这一份结构体里：
//!
//! ```text
//! 插件（cdylib）                          宿主
//! ─────────────────────────────────────────────────────────────
//! fn_kzwr_plugin_abi_v1() ──► *const KzwrPluginAbi ──► 校验 abi/size
//!        describe_json()  ◄── 调用 ──  解析 {id,name,ui,caps}
//!        available_json(cfg)             {"available":bool}
//!        action_json(action, request)    任意 JSON（原样回给前端）
//!        health_json(cfg)                [{key,title,status,detail,hint}]
//!        event_json(event, cfg)          任意 JSON（可选实现）
//!        free_str(ptr)        ◄── 宿主释放插件返回的字符串
//! ```
//!
//! ## 最小示例
//!
//! ```ignore
//! use fn_kzwr_plugin_sdk as sdk;
//!
//! extern "C" fn describe() -> *mut std::os::raw::c_char {
//!     sdk::to_c_string(r#"{"id":"hello","name":"示例","ui":{"title":"示例","blocks":[]}}"#)
//! }
//! extern "C" fn available(_cfg: *const std::os::raw::c_char) -> *mut std::os::raw::c_char {
//!     sdk::to_c_string(r#"{"available":true}"#)
//! }
//! extern "C" fn action(_a: *const std::os::raw::c_char, _r: *const std::os::raw::c_char) -> *mut std::os::raw::c_char {
//!     sdk::to_c_string(r#"{"success":true,"message":"hi"}"#)
//! }
//! extern "C" fn health(_cfg: *const std::os::raw::c_char) -> *mut std::os::raw::c_char {
//!     sdk::to_c_string("[]")
//! }
//!
//! sdk::export_plugin_v1!(describe, available, action, health);
//! ```
//!
//! ## 约定（违反会导致宿主拒绝或崩溃）
//!
//! - 字符串一律 **UTF-8 + NUL 结尾**，用 [`to_c_string`] 分配、由宿主的 `free_str` 释放
//! - 返回**空指针**表示"无内容"（宿主按空处理，不会崩）
//! - **不要 panic 跨 FFI**：Rust 插件请自行 `catch_unwind`，或保证不 panic
//! - 结构体字段**只能追加**（`size` 会告诉宿主实际长度）；宿主按自己的已知长度读取
//! - 回调需线程安全（宿主可能从不同线程调用）
//!
//! JSON 契约细节见仓库 `docs/PLUGIN_ABI.md`。

use std::ffi::CString;
use std::os::raw::c_char;

/// ABI 版本（与宿主保持一致；破坏性改动才会 +1）
///
/// **v2**：三张表移除全部 `config_get`/`config_set` 死字段（字段位于表中部的
/// 破坏性布局改动）。宿主对 `abi != 2` 的 v1 插件会**干净拒绝**并给出明确
/// 错误（而不是错读函数指针），因此老 .so 升级宿主后需重新编译。
pub const ABI_VERSION: u32 = 2;

/// 返回 JSON 字符串的无参回调
pub type JsonFn0 = extern "C" fn() -> *mut c_char;
/// 返回 JSON 字符串的单参回调（入参：`cfg_json`）
pub type JsonFn1 = extern "C" fn(*const c_char) -> *mut c_char;
/// 返回 JSON 字符串的双参回调（如 `action_json(action, request_json)`）
pub type JsonFn2 = extern "C" fn(*const c_char, *const c_char) -> *mut c_char;

/// 插件提供的静态函数表（**布局必须与宿主完全一致**）
///
/// 字段只能**尾部追加**：宿主按 `size` 判断老插件是否"短到这里之前"。
#[repr(C)]
pub struct KzwrPluginAbi {
    /// 必须为 [`ABI_VERSION`]
    pub abi: u32,
    /// 本结构体字节大小
    pub size: u32,
    /// 元信息 + UI 描述（JSON）
    pub describe_json: JsonFn0,
    /// 是否可用（JSON：`{"available":true}`）
    pub available_json: JsonFn1,
    /// 动作：`POST/GET /api/p/<插件id>/<action>`（入参 `request_json` = `{"body":…,"cfg":…}`）
    pub action_json: JsonFn2,
    /// 「一键体检」自检项（JSON 数组）
    pub health_json: JsonFn1,
    /// 生命周期事件（`startup` / `patrol` / `after_backup` / `reload` / `timer`），可选
    pub event_json: Option<JsonFn2>,
    /// 释放插件返回的字符串（宿主调用）
    pub free_str: extern "C" fn(*mut c_char),
    /// 卸载时清理，可选
    pub destroy: Option<extern "C" fn()>,
    /// 宿主能力表下发（可选；见 [`host`]）。用 [`export_plugin_v1!`] 会自动填好。
    pub host_bind: Option<HostBindFn>,
}

/// 宿主能力表的下发回调（插件实现）
///
/// 宿主调用一次：`table` 指向宿主**静态**能力表（进程内永不释放），`ctx` 是不透明句柄，
/// 把之后每次回调限定到本插件。返回 0 = 接受（宿主据此认为绑定成功）。
pub type HostBindFn =
    extern "C" fn(*const KzwrHostAbi, *mut std::os::raw::c_void) -> std::os::raw::c_int;

// ── 宿主能力表（宿主 → 插件；用 `host::*` 安全 API 访问，不要直接解引用）──

/// 宿主能力表 ABI 版本（独立于 [`ABI_VERSION`] 计数）
///
/// **v2**：移除已弃用的 `config_get`/`config_set` 桩。`host_bind` 先校验版本，
/// v1 老宿主给 v2 表时本 SDK 会拒绝接受并安全降级（声明式通道不受影响）。
pub const HOST_ABI_VERSION: u32 = 2;

/// 日志级别（与宿主一致）
pub const LOG_TRACE: i32 = 0;
pub const LOG_DEBUG: i32 = 1;
pub const LOG_INFO: i32 = 2;
pub const LOG_WARN: i32 = 3;
pub const LOG_ERROR: i32 = 4;

/// 告警级别（与宿主 `AlertLevel` 一致）
pub const ALERT_ERROR: i32 = 0;
pub const ALERT_WARN: i32 = 1;

/// **宿主能力表**（布局与宿主严格一致；插件侧只读）
///
/// 取值一律走 [`host`] 模块的安全 API —— 它会校验 `size`，
/// 避免插件比宿主新时读到不存在的尾部字段（那是未定义行为）。
///
/// `free_str` 位于**必需前缀**（紧跟 `abi`/`size`）：它是唯一非可选的入口，
/// 这样老宿主的表短一截时插件仍能接受整表、只逐字段跳过缺失的能力。
#[repr(C)]
pub struct KzwrHostAbi {
    /// 必须为 [`HOST_ABI_VERSION`]
    pub abi: u32,
    /// 本结构体字节大小（用于探测尾部字段，**勿硬编码**）
    pub size: u32,
    /// 释放宿主分配的字符串
    pub free_str: extern "C" fn(*mut c_char),
    /// 写宿主运行日志：`(ctx, level, msg)`
    pub log: Option<extern "C" fn(*mut std::os::raw::c_void, i32, *const c_char)>,
    /// 写审计日志：`(ctx, action, detail, ok)`
    pub audit: Option<
        extern "C" fn(*mut std::os::raw::c_void, *const c_char, *const c_char, i32),
    >,
    /// 上报告警：`(ctx, level, msg)`
    pub alert: Option<extern "C" fn(*mut std::os::raw::c_void, i32, *const c_char)>,
    /// 消解本插件告警（按消息前缀）：`(ctx, prefix)`
    pub resolve_alerts: Option<extern "C" fn(*mut std::os::raw::c_void, *const c_char)>,
    /// 宿主版本串（静态内存，**不得释放**）
    pub host_version: Option<extern "C" fn() -> *const c_char>,
    /// 当前毫秒时间戳
    pub now_ms: Option<extern "C" fn() -> i64>,
    /// 本插件私有数据目录（返回串须用 `free_str` 释放）
    pub own_data_dir: Option<extern "C" fn(*mut std::os::raw::c_void) -> *mut c_char>,
    /// 上报进度：`(ctx, label, done, total, detail)`
    pub progress: Option<
        extern "C" fn(*mut std::os::raw::c_void, *const c_char, u64, u64, *const c_char),
    >,
    /// 注册定时任务：`(ctx, kind, cron)`，0 = 已受理
    pub schedule: Option<
        extern "C" fn(*mut std::os::raw::c_void, *const c_char, *const c_char) -> i32,
    >,
    /// 用**宿主密钥**加密明文（返回 base64 密文；NULL = 失败）
    ///
    /// 插件把敏感内容写进自己的数据目录时用它 —— 密钥在宿主手里，插件拿不到。
    pub seal: Option<extern "C" fn(*mut std::os::raw::c_void, *const c_char) -> *mut c_char>,
    /// 用宿主密钥解密 [`Self::seal`] 的产物（NULL = 失败或非本宿主密钥加密）
    pub unseal: Option<extern "C" fn(*mut std::os::raw::c_void, *const c_char) -> *mut c_char>,
}

/// 宿主能力表的安全封装
///
/// 用法（在 `host_bind` 由 [`export_plugin_v1!`] 自动接好之后）：
///
/// ```ignore
/// if sdk::host::available() {
///     sdk::host::log(sdk::LOG_INFO, "插件已就绪");
///     sdk::host::audit("kzwr.trash.auto", "自动清空回收站：3 个文件", true);
/// }
/// ```
///
/// ## 为什么是全局单例
/// 一个进程里一个插件实例只接一次能力表；宿主热重载时会**重新下发**，
/// 因此这里用 `RwLock` 而非 `OnceLock`（能覆盖旧 ctx）。
pub mod host {
    use super::*;
    use std::sync::RwLock;

    /// 已接下的能力表 + 本插件的 ctx
    struct Binding {
        table: *const KzwrHostAbi,
        ctx: *mut std::os::raw::c_void,
    }

    // SAFETY: 两者都是宿主签发、进程内**永不释放**的指针（宿主从不 dlclose 插件），
    // 且宿主保证其实现线程安全（入口自带 catch_unwind 与限流）。插件可能从任意
    // 工作线程调用这些 API，故必须 Send + Sync。
    unsafe impl Send for Binding {}
    unsafe impl Sync for Binding {}

    static BINDING: RwLock<Option<Binding>> = RwLock::new(None);

    /// 取当前绑定（每次调用短暂持读锁；不跨 FFI 持有 —— 先把指针拷出来）
    ///
    /// 返回 `(table, ctx)`。**不持锁跨 FFI**：否则插件在多线程里并发调用可能互锁。
    fn bound() -> Option<(*const KzwrHostAbi, *mut std::os::raw::c_void)> {
        BINDING
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|b| (b.table, b.ctx)))
    }

    /// 能力表是否**整体**可用（`abi` 匹配且 `size` 覆盖到 `free_str`）
    pub fn available() -> bool {
        bound().is_some()
    }

    /// [`export_plugin_v1!`] 自动注册的 `host_bind` 实现（插件通常不必自己调）
    ///
    /// 校验版本与必需前缀后保存绑定；返回 0 表示接受。
    /// 版本不匹配＝拒绝（返回非 0），插件随后 `available()` 为 false 并退回声明式回传。
    pub extern "C" fn bind_trampoline(
        table: *const KzwrHostAbi,
        ctx: *mut std::os::raw::c_void,
    ) -> std::os::raw::c_int {
        if table.is_null() {
            return 1;
        }
        // 只读 `abi`/`size` 两个头部字段：它们在必需前缀内
        let (abi, size) = unsafe { ((*table).abi, (*table).size) };
        if abi != HOST_ABI_VERSION {
            return 2;
        }
        let need = std::mem::offset_of!(KzwrHostAbi, free_str)
            + std::mem::size_of::<extern "C" fn(*mut c_char)>();
        if (size as usize) < need {
            return 3;
        }
        match BINDING.write() {
            Ok(mut g) => {
                *g = Some(Binding { table, ctx });
                0
            }
            Err(_) => 4,
        }
    }

    /// 能力表是否覆盖到某字段（宿主可能比插件旧：尾部字段不存在）
    fn has(offset: usize, field_size: usize) -> bool {
        match bound() {
            Some((table, _)) => {
                let size = unsafe { (*table).size } as usize;
                size >= offset + field_size
            }
            None => false,
        }
    }

    /// 写宿主运行日志（`level` 用 [`LOG_INFO`] 等常量）
    pub fn log(level: i32, msg: &str) {
        let off = std::mem::offset_of!(KzwrHostAbi, log);
        if !has(off, std::mem::size_of::<usize>()) {
            return;
        }
        if let Some((table, ctx)) = bound() {
            let f = unsafe { (*table).log };
            if let Some(f) = f {
                let c = CString::new(msg.replace('\0', "")).unwrap_or_default();
                f(ctx, level, c.as_ptr());
            }
        }
    }

    /// 写审计日志（与宿主敏感操作同一份 `audit.log`）
    pub fn audit(action: &str, detail: &str, ok: bool) {
        let off = std::mem::offset_of!(KzwrHostAbi, audit);
        if !has(off, std::mem::size_of::<usize>()) {
            return;
        }
        if let Some((table, ctx)) = bound() {
            let f = unsafe { (*table).audit };
            if let Some(f) = f {
                let a = CString::new(action.replace('\0', "")).unwrap_or_default();
                let d = CString::new(detail.replace('\0', "")).unwrap_or_default();
                f(ctx, a.as_ptr(), d.as_ptr(), if ok { 1 } else { 0 });
            }
        }
    }

    /// 上报一条告警（`level` 用 [`ALERT_ERROR`] / [`ALERT_WARN`]）
    ///
    /// 与声明式 `alerts` 走**同一去重规则**（来源 = 本插件，消息相同即视为重复），
    /// 两条通道混用不会产生重复告警。
    pub fn alert(level: i32, msg: &str) {
        let off = std::mem::offset_of!(KzwrHostAbi, alert);
        if !has(off, std::mem::size_of::<usize>()) {
            return;
        }
        if let Some((table, ctx)) = bound() {
            let f = unsafe { (*table).alert };
            if let Some(f) = f {
                let c = CString::new(msg.replace('\0', "")).unwrap_or_default();
                f(ctx, level, c.as_ptr());
            }
        }
    }

    /// 消解本插件此前上报的告警（按**消息前缀**批量删除）
    pub fn resolve_alerts(prefix: &str) {
        let off = std::mem::offset_of!(KzwrHostAbi, resolve_alerts);
        if !has(off, std::mem::size_of::<usize>()) {
            return;
        }
        if let Some((table, ctx)) = bound() {
            let f = unsafe { (*table).resolve_alerts };
            if let Some(f) = f {
                let c = CString::new(prefix.replace('\0', "")).unwrap_or_default();
                f(ctx, c.as_ptr());
            }
        }
    }


    /// 宿主版本串（无能力表或字段缺失 → 空串）
    ///
    /// 用 `host_version()` 而非硬编码，便于插件按宿主版本做兼容分支。
    pub fn host_version() -> String {
        let off = std::mem::offset_of!(KzwrHostAbi, host_version);
        if !has(off, std::mem::size_of::<usize>()) {
            return String::new();
        }
        let Some((table, _)) = bound() else {
            return String::new();
        };
        let Some(f) = (unsafe { (*table).host_version }) else {
            return String::new();
        };
        let p = f();
        if p.is_null() {
            return String::new();
        }
        // 静态内存：**不得**释放
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned()
    }

    /// 当前毫秒时间戳（不可用 → -1）
    ///
    /// 用宿主时基，避免与宿主的告警/日志时间戳对不上。
    pub fn now_ms() -> i64 {
        let off = std::mem::offset_of!(KzwrHostAbi, now_ms);
        if !has(off, std::mem::size_of::<usize>()) {
            return -1;
        }
        let Some((table, _)) = bound() else {
            return -1;
        };
        match unsafe { (*table).now_ms } {
            Some(f) => f(),
            None => -1,
        }
    }

    /// 本插件私有的数据目录（宿主已创建；不可用 → `None`）
    ///
    /// 存放插件自己的缓存/临时文件；**凭据不要明文写这里**——
    /// 先用 [`seal`] 加密（密钥在宿主手里），密文落盘。
    pub fn own_data_dir() -> Option<String> {
        let off = std::mem::offset_of!(KzwrHostAbi, own_data_dir);
        if !has(off, std::mem::size_of::<usize>()) {
            return None;
        }
        let (table, ctx) = bound()?;
        let f = unsafe { (*table).own_data_dir }?;
        let p = f(ctx);
        if p.is_null() {
            return None;
        }
        let s = unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned();
        unsafe { ((*table).free_str)(p) };
        Some(s)
    }

    /// 上报进度（`total == 0` = 总量未知；宿主转发到 WebSocket，`kind="plugin"`）
    ///
    /// 长任务（如批量清空回收站）应定期调用，让用户看到进展。
    pub fn progress(label: &str, done: u64, total: u64, detail: &str) {
        let off = std::mem::offset_of!(KzwrHostAbi, progress);
        if !has(off, std::mem::size_of::<usize>()) {
            return;
        }
        let Some((table, ctx)) = bound() else {
            return;
        };
        let Some(f) = (unsafe { (*table).progress }) else {
            return;
        };
        let (Ok(l), Ok(d)) = (
            CString::new(label.replace('\0', "")),
            CString::new(detail.replace('\0', "")),
        ) else {
            return;
        };
        f(ctx, l.as_ptr(), done, total, d.as_ptr());
    }

    /// 注册周期任务：到点宿主回调 `event_json("timer", cfg)`，`cfg.timer_kind` = `kind`
    ///
    /// `cron` 按**宿主本地时区**解释（5 段式）。返回是否已受理
    /// （cron 非法 / 能力表缺失 / 队列满 → false，插件应退化为「下次 patrol 时做」）。
    ///
    /// 多次注册同一个 `kind` = 覆盖（宿主按 `(插件, kind)` 去重）。
    pub fn schedule(kind: &str, cron: &str) -> bool {
        let off = std::mem::offset_of!(KzwrHostAbi, schedule);
        if !has(off, std::mem::size_of::<usize>()) {
            return false;
        }
        let Some((table, ctx)) = bound() else {
            return false;
        };
        let Some(f) = (unsafe { (*table).schedule }) else {
            return false;
        };
        let (Ok(k), Ok(c)) = (
            CString::new(kind.replace('\0', "")),
            CString::new(cron.replace('\0', "")),
        ) else {
            return false;
        };
        f(ctx, k.as_ptr(), c.as_ptr()) == 0
    }

    /// 用**宿主密钥**加密一段明文（失败 → `None`）
    ///
    /// 插件自管配置时用它保护敏感内容（如 access-token）：密钥在宿主手里，
    /// 插件只能请求加解密，因此「配置自管」不会降级成明文落盘。
    ///
    /// 典型用法：
    /// ```ignore
    /// let sealed = sdk::host::seal(&json!({"token": t}).to_string());
    /// std::fs::write(dir.join("config.json"), sealed.unwrap_or_default())?;
    /// ```
    pub fn seal(plain: &str) -> Option<String> {
        let off = std::mem::offset_of!(KzwrHostAbi, seal);
        if !has(off, std::mem::size_of::<usize>()) {
            return None;
        }
        let (table, ctx) = bound()?;
        let f = unsafe { (*table).seal }?;
        let c = CString::new(plain.replace('\0', "")).ok()?;
        let p = f(ctx, c.as_ptr());
        if p.is_null() {
            return None;
        }
        let s = unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned();
        unsafe { ((*table).free_str)(p) };
        Some(s)
    }

    /// 解密 [`seal`] 的产物（失败 → `None`）
    ///
    /// **失败绝不回退成明文**：调用方必须把 `None` 当作「无此配置」处理。
    pub fn unseal(sealed: &str) -> Option<String> {
        let off = std::mem::offset_of!(KzwrHostAbi, unseal);
        if !has(off, std::mem::size_of::<usize>()) {
            return None;
        }
        let (table, ctx) = bound()?;
        let f = unsafe { (*table).unseal }?;
        let c = CString::new(sealed.replace('\0', "")).ok()?;
        let p = f(ctx, c.as_ptr());
        if p.is_null() {
            return None;
        }
        let s = unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned();
        unsafe { ((*table).free_str)(p) };
        Some(s)
    }
}


/// 把字符串转成插件返回给宿主的 C 字符串（内部 NUL 会被剔除）
pub fn to_c_string(s: impl AsRef<str>) -> *mut c_char {
    let cleaned: Vec<u8> = s
        .as_ref()
        .as_bytes()
        .iter()
        .copied()
        .filter(|b| *b != 0)
        .collect();
    match CString::new(cleaned) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// **推荐写法**：把入口函数体写成**普通闭包**交给本函数，panic 会被吃掉
///
/// ⚠️ **为什么必须这样写**（语言机制限制，实测）：
/// Rust 中 `extern "C" fn` 具有 `nounwind` 属性 —— 若 panic 发生在
/// **extern "C" fn 的函数体里**，panic **无法展开**，运行时直接
/// `panic_cannot_unwind` → **abort 整个进程**，外层任何 `catch_unwind` 都拦不住。
///
/// 因此**入口函数体必须是普通闭包**，让 panic 发生在可被捕获的上下文里：
///
/// ```ignore
/// extern "C" fn describe() -> *mut c_char {
///     sdk::guard_str(|| json!({ ... }).to_string())   // ✅ panic 可捕获
/// }
/// extern "C" fn bad() -> *mut c_char {
///     panic!("...");                                  // ❌ 直接 abort 宿主
/// }
/// ```
///
/// 这也是宿主 release 用 `panic = "abort"` 的**放大器**：宿主侧的
/// `catch_unwind` 在 release 下同样失效（panic 直接 abort），
/// 所以兜底只能在插件侧、且必须以这种闭包形态写。
pub fn guard_str<F>(f: F) -> *mut c_char
where
    F: FnOnce() -> String + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(f) {
        Ok(s) => to_c_string(s),
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "未知 panic".to_string());
            to_c_string(format!(
                "{{\"success\":false,\"error\":\"插件内部错误（已在插件侧捕获，宿主进程安全）：{msg}\"}}"
            ))
        }
    }
}

/// **panic 兜底**（入口已返回 `*mut c_char` 的版本）
///
/// 现有插件的入口自己返回 C 字符串（内部可能已有 `catch_unwind`），
/// 这里再兜一层，保证 panic **绝不越过 C 边界**。
///
/// 正常路径原样透传 `f` 的返回值；`f` panic 则返回
/// `{"success":false,"error":…}` 的 C 字符串。
pub fn guard_ptr<F>(f: F) -> *mut c_char
where
    F: FnOnce() -> *mut c_char + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(f) {
        Ok(p) => p,
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "未知 panic".to_string());
            to_c_string(format!(
                "{{\"success\":false,\"error\":\"插件内部错误（已在插件侧捕获，宿主进程安全）：{msg}\"}}"
            ))
        }
    }
}

/// **panic 兜底**：执行 `f`，把 panic 转成 `{"success":false,"error":…}`
///
/// 供 [`guarded_entry!`] / 插件自带包装使用。**绝不让 panic 越过 C 边界** ——
/// 宿主 release 是 `panic = "abort"`，panic 一旦越界就是整个进程 abort。
///
/// ## 与 `panic = "abort"` 的关系（重要）
///
/// 本 SDK（插件侧）**不设** `panic` 策略，即插件默认是 `unwind`，
/// 因此这里的 `catch_unwind` **有效**。若某个插件自己在 `Cargo.toml` 里
/// 设了 `panic = "abort"`，本函数将**失效** —— 那时 panic 会直接 abort。
/// 故 `security_gate.sh` 会检查插件不得设 `panic = "abort"`。
pub fn catch_c_str<F>(f: F) -> *mut c_char
where
    F: FnOnce() -> String + std::panic::UnwindSafe,
{
    let out = std::panic::catch_unwind(f).unwrap_or_else(|e| {
        let msg = e
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| e.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "未知 panic".to_string());
        format!(
            "{{\"success\":false,\"error\":\"插件内部错误（已在插件侧捕获，宿主进程安全）：{msg}\"}}"
        )
    });
    to_c_string(out)
}

/// 释放插件返回的字符串（由宿主通过 `free_str` 回调调用）
///
/// # Safety
/// `p` 必须是 [`to_c_string`] 返回且**尚未释放**的指针，或空指针。
///
/// ## 跨 allocator 约束（务必遵守）
///
/// `to_c_string` 用 **本插件的全局分配器** 分配，这里用 `CString::from_raw` 归还给
/// **同一个分配器**。因此插件必须与自身保持一致：
/// - ✅ 默认（glibc 动态链接、系统 malloc）→ 正常；
/// - ❌ 插件改用 `jemalloc`/`mimalloc` 的 `#[global_allocator]`，或静态链接另一份
///   malloc（musl 等）→ **释放时可能崩溃**（把别的分配器的指针还给本分配器）。
///
/// 宿主同理：它返回给插件的字符串用宿主分配器分配、由宿主的 `free_str` 释放，
/// 两侧各自成对 —— **插件绝不要**用 `free_c_string` 去释放宿主给的字符串，
/// 也不要让宿主释放插件自己 malloc 的内存。
pub extern "C" fn free_c_string(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    unsafe {
        drop(CString::from_raw(p));
    }
}

/// 读取宿主传入的 C 字符串（空指针 → 空串）
///
/// # Safety
/// `p` 必须是宿主传入的合法 NUL 结尾字符串，或空指针。
pub unsafe fn from_c_str(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
}

/// 导出插件（不带生命周期事件）
///
/// `host_bind` 自动填为 SDK 的 [`host::bind_trampoline`]：插件**无需额外代码**
/// 即可用 [`host`] 的日志/审计/告警/自配置/进度/定时 API。
/// 不接受能力表（或宿主未下发）时，[`host::available()`] 为 false，插件应退回声明式回传。
#[macro_export]
macro_rules! export_plugin_v1 {
    ($describe:path, $available:path, $action:path, $health:path $(,)?) => {
        $crate::export_plugin_v1!($describe, $available, $action, $health, None);
    };
    ($describe:path, $available:path, $action:path, $health:path, Some($event:path) $(,)?) => {
        /// 稳定 C ABI v1 入口：返回插件持有的静态函数表
        #[no_mangle]
        pub extern "C" fn fn_kzwr_plugin_abi_v1() -> *const $crate::KzwrPluginAbi {
            $crate::guarded_entry!(
                $describe, $available, $action, $health,
                event: Some(__gw_event), __gw_event => $event
            )
        }
    };
    ($describe:path, $available:path, $action:path, $health:path, None $(,)?) => {
        /// 稳定 C ABI v1 入口：返回插件持有的静态函数表
        #[no_mangle]
        pub extern "C" fn fn_kzwr_plugin_abi_v1() -> *const $crate::KzwrPluginAbi {
            $crate::guarded_entry!($describe, $available, $action, $health, event: None)
        }
    };
}

/// 生成插件函数表（供 [`export_plugin_v1!`] 内部使用）
///
/// ## panic 兜底的**真实能力边界**（实测，勿夸大）
///
/// 实测三条结论（2026-09-29，用 release 构建的真实 .so + 宿主验证）：
///
/// 1. **宿主 release 用 `panic = "abort"` ⇒ 宿主侧 `catch_unwind` 全部失效**
///    （panic 直接 abort，连返回值都打印不出来）。所以兜底只能在**插件侧**。
/// 2. **panic 若发生在 `extern "C" fn` 的函数体内 ⇒ 无法被任何 `catch_unwind` 拦截**：
///    `extern "C" fn` 带 `nounwind`，运行时报 `panic_cannot_unwind` 并 abort 整个进程。
///    —— **这是语言机制限制，包装救不了它**。
/// 3. **panic 发生在普通闭包内 ⇒ 可被捕获**（实测宿主进程存活）。
///
/// 因此真正有效的约定只有一条：**插件入口的函数体必须是普通闭包**，
/// 用 [`guard_str`] 包裹（现有三个插件均已如此）。
///
/// 本宏额外包的一层 `guard_ptr` 是**纵深防御**：对已用 `guard_str` 的插件是幂等的，
/// 对「入口直接返回 C 字符串、且内部走闭包」的插件也能兜住。
/// 但它**兜不住**"panic 写在 extern "C" 函数体里"的情形 —— 那种只能靠
/// 文档约束 + PR 闸门（检查入口是否用了 guard_str / 闭包形态）。
#[macro_export]
macro_rules! guarded_entry {
    ($d:path, $a:path, $act:path, $h:path, event: None $(,)?) => {{
        extern "C" fn __gw_describe() -> *mut std::os::raw::c_char {
            $crate::guard_ptr(|| $d())
        }
        extern "C" fn __gw_available(c: *const std::os::raw::c_char) -> *mut std::os::raw::c_char {
            $crate::guard_ptr(|| $a(c))
        }
        extern "C" fn __gw_action(
            x: *const std::os::raw::c_char,
            y: *const std::os::raw::c_char,
        ) -> *mut std::os::raw::c_char {
            $crate::guard_ptr(|| $act(x, y))
        }
        extern "C" fn __gw_health(c: *const std::os::raw::c_char) -> *mut std::os::raw::c_char {
            $crate::guard_ptr(|| $h(c))
        }
        static TABLE: $crate::KzwrPluginAbi = $crate::KzwrPluginAbi {
            abi: $crate::ABI_VERSION,
            size: std::mem::size_of::<$crate::KzwrPluginAbi>() as u32,
            describe_json: __gw_describe,
            available_json: __gw_available,
            action_json: __gw_action,
            health_json: __gw_health,
            event_json: None,
            free_str: $crate::free_c_string,
            destroy: None,
            host_bind: Some($crate::host::bind_trampoline),
        };
        &TABLE
    }};
    ($d:path, $a:path, $act:path, $h:path, event: Some($ef:path), $gw:ident => $e:path $(,)?) => {{
        extern "C" fn __gw_describe() -> *mut std::os::raw::c_char {
            $crate::guard_ptr(|| $d())
        }
        extern "C" fn __gw_available(c: *const std::os::raw::c_char) -> *mut std::os::raw::c_char {
            $crate::guard_ptr(|| $a(c))
        }
        extern "C" fn __gw_action(
            x: *const std::os::raw::c_char,
            y: *const std::os::raw::c_char,
        ) -> *mut std::os::raw::c_char {
            $crate::guard_ptr(|| $act(x, y))
        }
        extern "C" fn __gw_health(c: *const std::os::raw::c_char) -> *mut std::os::raw::c_char {
            $crate::guard_ptr(|| $h(c))
        }
        extern "C" fn $gw(
            x: *const std::os::raw::c_char,
            y: *const std::os::raw::c_char,
        ) -> *mut std::os::raw::c_char {
            $crate::guard_ptr(|| $e(x, y))
        }
        static TABLE: $crate::KzwrPluginAbi = $crate::KzwrPluginAbi {
            abi: $crate::ABI_VERSION,
            size: std::mem::size_of::<$crate::KzwrPluginAbi>() as u32,
            describe_json: __gw_describe,
            available_json: __gw_available,
            action_json: __gw_action,
            health_json: __gw_health,
            event_json: Some($gw),
            free_str: $crate::free_c_string,
            destroy: None,
            host_bind: Some($crate::host::bind_trampoline),
        };
        &TABLE
    }};
}

// ── 目标能力表（自定义备份目标）────────────────────────────────────────────
//
// 想提供新的备份目的地（对象存储 / 另一家网盘 / 本地目录 …）就实现这张表，
// 并用 [`export_target_v1!`] 导出 `fn_kzwr_plugin_target_v1`。
//
// ⚠️ **字段顺序与类型必须和宿主 `backend/src/plugin/abi.rs` 的 `KzwrTargetAbi` 完全一致**
// （两边独立定义，靠 `#[repr(C)]` 布局对齐；只能在尾部追加字段）。
//
// 数据模型：宿主读明文 → age 加密 → 把**密文**推给插件（推块），
// 插件**不接触明文与密钥**，也不需要回调宿主。恢复时插件给密文、宿主解密。

use std::os::raw::c_void;

/// 目标能力表（**布局与宿主严格一致**）
#[repr(C)]
pub struct KzwrTargetAbi {
    /// 必须为 [`ABI_VERSION`]
    pub abi: u32,
    /// 本结构体字节大小
    pub size: u32,

    // ── 实例生命周期 ──
    /// 用 `target_json`（含该目标凭据与插件自管配置）创建实例；返回 NULL = 配置无效
    pub target_open: extern "C" fn(*const c_char) -> *mut c_void,
    pub target_close: Option<extern "C" fn(*mut c_void)>,

    // ── 传输（宿主 → 插件，内容为 age 密文） ──
    /// `rel_path` 为**目标端**相对路径，`total` 为密文总字节（未知为 0）
    pub write_begin: extern "C" fn(*mut c_void, *const c_char, u64) -> *mut c_void,
    /// 返回实写字节数（≥0）或负错误码
    pub write_chunk: extern "C" fn(*mut c_void, *mut c_void, *const u8, u32) -> i32,
    /// 返回**实写密文字节总数**（宿主会与喂出的字节比对，防静默截断）
    pub write_end: extern "C" fn(*mut c_void, *mut c_void) -> i64,
    pub write_abort: Option<extern "C" fn(*mut c_void, *mut c_void)>,

    // ── 读取（插件 → 宿主，内容为 age 密文；用于恢复） ──
    pub read_begin: extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    /// 返回读到的字节数（>0）、0 = EOF、<0 = 错误码
    pub read_chunk: extern "C" fn(*mut c_void, *mut c_void, *mut u8, u32) -> i32,
    pub read_end: Option<extern "C" fn(*mut c_void, *mut c_void) -> i32>,

    // ── 目录与元数据 ──
    /// `[{"rel_path","size","mtime_secs","is_dir"}]`
    pub list_json: extern "C" fn(*mut c_void, *const c_char) -> *mut c_char,
    pub delete: extern "C" fn(*mut c_void, *const c_char) -> i32,
    pub ensure_dir: Option<extern "C" fn(*mut c_void, *const c_char) -> i32>,
    pub ping: Option<extern "C" fn(*mut c_void) -> i32>,
    /// 设置页「测试连接」（实例尚未建立时）
    pub test_json: Option<extern "C" fn(*const c_char) -> *mut c_char>,

    /// 最近一次错误的详情（JSON）
    pub last_error_json: Option<extern "C" fn(*mut c_void) -> *mut c_char>,

    // ── 并发回传（可选；describe 的 `target.supports_plan=true` 时宿主采用） ──
    /// `job_json` = `{"upload":[{"rel_path","size","mtime_secs"}]}`
    pub plan_begin: Option<extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    /// 下一批要传的**目标端路径**（`[]` = 清单已空）
    pub plan_next: Option<extern "C" fn(*mut c_void, *mut c_void) -> *mut c_char>,
    pub plan_end: Option<extern "C" fn(*mut c_void, *mut c_void)>,

    /// 释放本表返回的字符串（宿主调用）
    pub free_str: extern "C" fn(*mut c_char),
}

/// 导出**目标能力表**（自定义备份目标）
///
/// 参数按结构体字段顺序，**可选回调传 `None`**：
///
/// ```ignore
/// sdk::export_target_v1!(
///     my_open, Some(my_close),
///     my_write_begin, my_write_chunk, my_write_end, Some(my_write_abort),
///     my_read_begin, my_read_chunk, Some(my_read_end),
///     my_list_json, my_delete, Some(my_ensure_dir), Some(my_ping), Some(my_test_json),
///     Some(my_last_error),
///     Some(my_plan_begin), Some(my_plan_next), Some(my_plan_end),
/// );
/// ```
#[macro_export]
macro_rules! export_target_v1 {
    (
        $open:expr, $close:expr,
        $write_begin:expr, $write_chunk:expr, $write_end:expr, $write_abort:expr,
        $read_begin:expr, $read_chunk:expr, $read_end:expr,
        $list_json:expr, $delete:expr, $ensure_dir:expr, $ping:expr, $test_json:expr,
        $last_error_json:expr,
        $plan_begin:expr, $plan_next:expr, $plan_end:expr $(,)?
    ) => {
        /// 目标能力入口：返回插件持有的静态目标表
        #[no_mangle]
        pub extern "C" fn fn_kzwr_plugin_target_v1() -> *const $crate::KzwrTargetAbi {
            static TABLE: $crate::KzwrTargetAbi = $crate::KzwrTargetAbi {
                abi: $crate::ABI_VERSION,
                size: std::mem::size_of::<$crate::KzwrTargetAbi>() as u32,
                target_open: $open,
                target_close: $close,
                write_begin: $write_begin,
                write_chunk: $write_chunk,
                write_end: $write_end,
                write_abort: $write_abort,
                read_begin: $read_begin,
                read_chunk: $read_chunk,
                read_end: $read_end,
                list_json: $list_json,
                delete: $delete,
                ensure_dir: $ensure_dir,
                ping: $ping,
                test_json: $test_json,
                last_error_json: $last_error_json,
                plan_begin: $plan_begin,
                plan_next: $plan_next,
                plan_end: $plan_end,
                free_str: $crate::free_c_string,
            };
            &TABLE
        }
    };
}

/// 常用动作结果构造（避免插件各自手拼 JSON）
pub mod json {
    /// `{"success":true,"message":…}`
    pub fn ok_message(message: &str) -> String {
        format!(
            r#"{{"success":true,"message":{}}}"#,
            escape(message)
        )
    }

    /// `{"success":false,"error":…}`
    pub fn error(message: &str) -> String {
        format!(
            r#"{{"success":false,"error":{}}}"#,
            escape(message)
        )
    }

    /// JSON 字符串转义（仅用于上面两个便捷函数；复杂结构请自备 serde_json）
    pub fn escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固化本次排查的核心结论：**panic 发生在普通闭包内可被捕获**
    ///
    /// 反向的那半（panic 写在 `extern "C" fn` 函数体内 ⇒ `panic_cannot_unwind`、
    /// 直接 abort，**任何包装都拦不住**）无法写成单测 —— 它会终止测试进程。
    /// 已用 release 构建的真实 `.so` + 宿主实测确认，结论记录在
    /// [`guard_str`] 与 [`guarded_entry`] 的文档里。
    #[test]
    fn guard_str_catches_panic_in_closure() {
        let p = guard_str(|| -> String { panic!("boom") });
        assert!(!p.is_null(), "panic 时应返回错误 JSON，而不是空指针");
        let s = unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy();
        assert!(s.contains("success"), "应返回 JSON：{s}");
        assert!(s.contains("宿主进程安全"), "错误文案应说明宿主安全：{s}");
        free_c_string(p);
    }

    /// 正常路径：原样返回闭包产生的字符串
    #[test]
    fn guard_str_passes_through_normal_result() {
        let p = guard_str(|| r#"{"ok":true}"#.to_string());
        let s = unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy();
        assert_eq!(s, r#"{"ok":true}"#);
        free_c_string(p);
    }

    /// `guard_ptr` 对「入口返回 C 字符串且内部走闭包」的情形兜底
    #[test]
    fn guard_ptr_catches_panic() {
        let p = guard_ptr(|| -> *mut c_char { panic!("boom") });
        assert!(!p.is_null());
        let s = unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy();
        assert!(s.contains("success"));
        free_c_string(p);
    }
}
