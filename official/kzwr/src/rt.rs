//! 插件自己的 tokio runtime
//!
//! 宿主在 **`spawn_blocking` 的阻塞线程**上调用本插件的回调（见 `docs/PLUGIN_ABI.md`），
//! 因此这里可以安全地 `block_on`：既不会 panic（"Cannot start a runtime from within a
//! runtime"），也不会占住 tokio worker 线程。
//!
//! runtime 建一次全局复用：`reqwest::Client` 内部持有连接池与 DNS 解析器，
//! 每次调用新建 runtime 会让 keep-alive 完全失效。

use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};

static RT: OnceLock<Runtime> = OnceLock::new();

/// 取全局 runtime（多线程 + 定时器：HTTP 超时与重试退避都要用）
fn rt() -> &'static Runtime {
    RT.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .thread_name("kzwr-plugin")
            .build()
            .expect("kzwr 插件：构建 tokio runtime 失败")
    })
}

/// 在插件 runtime 上同步执行一个 async 闭包
///
/// 所有 FFI 入口都必须经这里，且**外部输入不得让闭包 panic**
/// （跨 FFI panic 是未定义行为级别的事故；调用方另有 `catch_unwind` 兜底）。
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    rt().block_on(f)
}
