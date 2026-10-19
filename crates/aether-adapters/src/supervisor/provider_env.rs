//! 供应商消费映射的 spawn 环境注入点（M7-06/ADR-024 附录 A-1）。
//!
//! 本模块只定义**注入点**：适配器启动（spawn）前，由外部实现（`aether-tauri`：读
//! `providers`/`provider_models` 快照 + `aether-security` 密钥库）解析本次 spawn 需要
//! 追加的环境变量；密钥值仅以一次性 env 形态进入子进程（D10），不落盘、不经线协议。
//!
//! 失败隔离：实现方负责诊断日志与降级（读取失败 / 无匹配 provider → 返回空集合；
//! 不得阻断适配器启动）；本 crate 不感知存储与密钥库（依赖方向：仅 `aether-core`）。

use std::future::Future;
use std::pin::Pin;

/// spawn 前环境变量解析来源（按运行时 id 裁剪；ADR-024「env 白名单按适配器 × provider」）。
pub trait SpawnEnvSource: Send + Sync + 'static {
    /// 返回本次 spawn 需追加的环境变量（键值对；空集合 = 不注入）。
    ///
    /// 实现约定：**不返回错误**——读取失败或无匹配 provider 时返回空集合并记诊断日志
    /// （失败隔离；官方默认路径不被阻断）。值可能含密钥材料，调用方与实现方都不得记录。
    fn spawn_env<'a>(&'a self, runtime_id: &'a str) -> SpawnEnvFuture<'a>;
}

/// [`SpawnEnvSource::spawn_env`] 的异步返回（object-safe）。
pub type SpawnEnvFuture<'a> = Pin<Box<dyn Future<Output = Vec<(String, String)>> + Send + 'a>>;
