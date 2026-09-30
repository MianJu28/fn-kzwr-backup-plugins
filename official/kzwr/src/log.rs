//! 插件侧日志：**优先走宿主能力表**（进宿主的 `app.log`），无能力表时退回 stderr
//!
//! ## 为什么不能直接用 `tracing`
//! 本 crate 编成独立 cdylib，`tracing` 的 dispatcher 是**每个副本各自一份**的线程局部
//! 状态。宿主的订阅器装在宿主那份里，插件这份没有订阅器 → 事件被静默丢弃，
//! 等于日志黑洞（还白白拖进 `tracing` 依赖）。
//!
//! ## 两条通道
//! 1. **宿主能力表**（推荐）：`sdk::host::log` 把日志经宿主能力表写进**宿主自己的**
//!    `app.log`，带时间戳/级别/`plugin=kzwr` 前缀，与核心日志同一份、同样轮转 ——
//!    用户在「日志」页直接就能看到，不必去翻 stderr。
//! 2. **stderr 兜底**：宿主未下发能力表（老宿主 / 绑定被拒）时退回原来的行为，
//!    带 `[kzwr]` 前缀。NAS 上宿主把 stderr 收进自己的日志，`grep '\[kzwr\]'` 可捞。
//!
//! 这样插件在**新旧宿主**上都能留下可诊断的痕迹，不会因为升级顺序而丢日志。
//!
//! `debug!` 默认关闭，设环境变量 `KZWR_PLUGIN_DEBUG=1` 打开（排障时临时用）。

use fn_kzwr_plugin_sdk as sdk;

/// 是否输出 debug 级（启动时读一次）
fn debug_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("KZWR_PLUGIN_DEBUG")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
    })
}

/// 写一条：优先宿主能力表，退回 stderr
fn line(level: &str, host_level: i32, msg: &str) {
    if sdk::host::available() {
        sdk::host::log(host_level, msg);
    } else {
        eprintln!("[kzwr] {level} {msg}");
    }
}

/// 信息级（默认输出）
#[allow(dead_code)]
pub fn info(msg: impl AsRef<str>) {
    line("INFO", sdk::LOG_INFO, msg.as_ref());
}

/// 警告级：不影响功能但用户该知道的问题
pub fn warn(msg: impl AsRef<str>) {
    line("WARN", sdk::LOG_WARN, msg.as_ref());
}

/// 错误级：功能已失败（如清空回收站中途出错）
#[allow(dead_code)]
pub fn error(msg: impl AsRef<str>) {
    line("ERROR", sdk::LOG_ERROR, msg.as_ref());
}

/// 调试级：仅在 `KZWR_PLUGIN_DEBUG=1` 时输出
pub fn debug(msg: impl AsRef<str>) {
    if debug_on() {
        line("DEBUG", sdk::LOG_DEBUG, msg.as_ref());
    }
}
