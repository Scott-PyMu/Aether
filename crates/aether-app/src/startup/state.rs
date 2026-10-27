//! 迁移状态文件（M1-06 幂等续跑）：`migration_state.json`。
//!
//! 位置：应用配置目录（与数据目录指针 `data-location.json` 同目录；`AETHER_DATA_LOCATION_FILE`
//! 的父目录，测试/E2E 可隔离）。不写入源（同步盘）目录，避免同步污染与自复制。
//!
//! 阶段机（与 [`MigrationPhase`] 一一对应）：
//! ```text
//! copying ──复制+校验+原子替换成功──▶ verified ──指针写入成功──▶ pointer_written ──门就绪──▶ done
//!    │                                  │
//!    └────── 中断（目标可能半套）────────┘ 中断（目标完整副本，可直接续跑写指针）
//! ```
//! 续跑语义：
//! - 状态文件存在且 `source`/`target` 与本次一致、阶段为 `copying`/`verified`：允许原地续跑；
//! - `verified` 且目标清单摘要与 `checksum` 一致 → 跳过复制，直接重试写指针；
//! - `copying` 或摘要不一致 → 清理目标中「本应用迁移残留」（暂存目录 + 与源同名的条目）
//!   后重新复制；目标含未知条目 → 拒绝并提示另选目录；
//! - 无状态文件的非空目标 → 维持原行为（拒绝，提示另选目录）。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 状态文件名（与指针文件同目录）。
pub const STATE_FILE_NAME: &str = "migration_state.json";
/// Crockford Base32 字母表（ULID 编码）。
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
static ULID_COUNTER: AtomicU64 = AtomicU64::new(1);
static ULID_SEED: OnceLock<RandomState> = OnceLock::new();

/// 迁移阶段（稳定契约，序列化为 snake_case）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    /// 复制/校验/原子替换进行中（目标可能不完整）。
    Copying,
    /// 目标已含完整副本且校验通过（可跳过复制，直接写指针）。
    Verified,
    /// 指针已写入（锁定新目录）。
    PointerWritten,
    /// 迁移完成。
    Done,
}

impl MigrationPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Copying => "copying",
            Self::Verified => "verified",
            Self::PointerWritten => "pointer_written",
            Self::Done => "done",
        }
    }

    /// 是否为「可续跑」阶段（目标可能已含完整或部分副本）。
    pub const fn is_resumable(self) -> bool {
        matches!(self, Self::Copying | Self::Verified)
    }
}

/// 迁移状态文件内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationState {
    /// 迁移 ID（ULID 文本）。
    pub migration_id: String,
    /// 源数据目录（绝对路径字符串）。
    pub source: String,
    /// 目标数据目录（绝对路径字符串）。
    pub target: String,
    /// 当前阶段。
    pub phase: MigrationPhase,
    /// 目标副本清单摘要（sha256；`copying` 阶段为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// 开始时间（Unix epoch 毫秒，与附录 E 口径一致）。
    pub started_at: u64,
}

impl MigrationState {
    pub fn new(source: &str, target: &str) -> Self {
        Self {
            migration_id: new_ulid(),
            source: source.to_string(),
            target: target.to_string(),
            phase: MigrationPhase::Copying,
            checksum: None,
            started_at: now_millis(),
        }
    }
}

/// 状态文件路径：给定指针文件，取同目录下的 [`STATE_FILE_NAME`]。
pub fn state_path_for_pointer(pointer_file: &Path) -> Option<PathBuf> {
    pointer_file
        .parent()
        .map(|parent| parent.join(STATE_FILE_NAME))
}

/// 读取状态文件；不存在返回 `Ok(None)`；损坏返回 `Err`。
pub fn read_state(path: &Path) -> Result<Option<MigrationState>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取迁移状态失败：{error}")),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|error| format!("迁移状态文件不是合法 JSON：{error}"))
}

/// 原子写入状态文件（临时文件 + rename）。
pub fn write_state(path: &Path, state: &MigrationState) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "迁移状态文件路径缺少父目录".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("创建状态目录 {} 失败：{error}", parent.display()))?;
    let serialized = serde_json::to_string_pretty(state)
        .map_err(|error| format!("序列化迁移状态失败：{error}"))?;
    let temp = parent.join(format!(".migration-state.tmp-{}", std::process::id()));
    std::fs::write(&temp, serialized)
        .map_err(|error| format!("写入迁移状态临时文件失败：{error}"))?;
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("替换迁移状态文件失败：{error}")
    })
}

/// 删除状态文件（不存在视为成功）。
pub fn clear_state(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("删除迁移状态文件失败：{error}")),
    }
}

fn now_millis() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

/// 生成 26 字符 ULID 文本（48 位毫秒时间戳 + 80 位进程内随机/单调后缀）。
///
/// 与 M1-05 的 `aether-control::ulid` 同思路：不引入 `ulid` crate，避免为标识符
/// 增加新依赖；随机源使用 `RandomState`（OS 播种的 SipHash 密钥）与原子计数，
/// 仅用于标识（非安全用途）。
fn new_ulid() -> String {
    let state = ULID_SEED.get_or_init(RandomState::default);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let counter = ULID_COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut first = state.build_hasher();
    first.write_u64(nanos);
    first.write_u64(counter);
    let high = first.finish();

    let mut second = state.build_hasher();
    second.write_u64(!nanos);
    second.write_u64(counter.rotate_left(17));
    let low = second.finish();

    let millis = now_millis();
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6..10].copy_from_slice(&(high as u32).to_be_bytes());
    bytes[10..16].copy_from_slice(&low.to_be_bytes()[2..]);

    let mut text = String::with_capacity(26);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = ((buffer >> bits) & 0x1F) as usize;
            text.push(CROCKFORD[index] as char);
        }
    }
    if bits > 0 {
        let index = ((buffer << (5 - bits)) & 0x1F) as usize;
        text.push(CROCKFORD[index] as char);
    }
    text
}
