//! 数据目录定位与「迁移后锁定新目录」的持久化指针（M1-06）。
//!
//! 解析优先级（设计 D3 为基础路径；迁移锁定为 M1-06 增量）：
//! 1. `AETHER_DATA_DIR` 环境变量（测试 / 受控部署钩子；仍走 A4 检测，不能借此
//!    绕过同步盘拒绝）；
//! 2. 指针文件 `data-location.json`（迁移成功后写入，锁定新目录）；
//! 3. 缺省 `dirs::data_dir()/Aether`（D3：Win `%APPDATA%\Aether`，
//!    macOS `~/Library/Application Support/Aether`）。
//!
//! 指针文件位于 `dirs::data_local_dir()/Aether/data-location.json`（Win
//! `%LOCALAPPDATA%`，不随 OneDrive 漫游），可用 `AETHER_DATA_LOCATION_FILE`
//! 覆盖（E2E 隔离用）。写入采用「临时文件 + rename」原子替换。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 数据目录环境变量（解析优先级最高；仍受 A4 检测约束）。
pub const DATA_DIR_ENV: &str = "AETHER_DATA_DIR";
/// 指针文件路径覆盖（测试 / E2E 隔离）。
pub const POINTER_ENV: &str = "AETHER_DATA_LOCATION_FILE";
/// 应用目录名（与 D3 一致）。
pub const APP_DIR_NAME: &str = "Aether";
/// 指针文件名。
pub const POINTER_FILE_NAME: &str = "data-location.json";
/// 指针文件版本（结构演进时递增）。
pub const POINTER_VERSION: u32 = 1;

/// 指针文件内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointerFile {
    pub v: u32,
    pub data_dir: String,
}

/// 指针文件路径：环境变量覆盖优先，否则 `data_local_dir()/Aether/data-location.json`。
pub fn pointer_file_path() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(POINTER_ENV) {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return Some(path);
        }
    }
    dirs::data_local_dir().map(|dir| dir.join(APP_DIR_NAME).join(POINTER_FILE_NAME))
}

/// `AETHER_DATA_DIR` 覆盖值（必须为绝对路径，否则视为配置错误）。
pub fn env_data_dir() -> Option<Result<PathBuf, String>> {
    let raw = std::env::var_os(DATA_DIR_ENV)?;
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Some(Ok(path))
    } else {
        Some(Err(format!(
            "{DATA_DIR_ENV} 必须是绝对路径（当前值：{}）",
            path.display()
        )))
    }
}

/// 缺省数据目录（D3）：`data_dir()/Aether`。
pub fn default_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join(APP_DIR_NAME))
}

/// 迁移前置检查：指针文件所在目录可创建、可写（临时探针文件）。
pub fn check_writable(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "数据目录指针路径缺少父目录".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("创建指针目录 {} 失败：{error}", parent.display()))?;
    let probe = parent.join(format!(".aether-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"probe")
        .map_err(|error| format!("指针目录 {} 不可写：{error}", parent.display()))?;
    std::fs::remove_file(&probe).map_err(|error| format!("清理写探针文件失败：{error}"))?;
    Ok(())
}

/// 读取指针文件；不存在返回 `Ok(None)`；损坏或版本不认识返回 `Err`。
pub fn read_pointer(path: &Path) -> Result<Option<PathBuf>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取数据目录指针失败：{error}")),
    };
    let pointer: PointerFile = serde_json::from_str(&raw)
        .map_err(|error| format!("数据目录指针不是合法 JSON：{error}"))?;
    if pointer.v != POINTER_VERSION {
        return Err(format!(
            "数据目录指针版本 {} 不受支持（当前 {POINTER_VERSION}）",
            pointer.v
        ));
    }
    let dir = PathBuf::from(&pointer.data_dir);
    if !dir.is_absolute() {
        return Err("数据目录指针必须指向绝对路径".to_string());
    }
    Ok(Some(dir))
}

/// 写入指针文件（临时文件 + rename 原子替换；父目录按需创建）。
pub fn write_pointer(path: &Path, data_dir: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "数据目录指针路径缺少父目录".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("创建指针目录 {} 失败：{error}", parent.display()))?;
    let pointer = PointerFile {
        v: POINTER_VERSION,
        data_dir: data_dir.to_string_lossy().to_string(),
    };
    let serialized = serde_json::to_string_pretty(&pointer)
        .map_err(|error| format!("序列化数据目录指针失败：{error}"))?;

    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| "数据目录指针路径缺少文件名".to_string())?;
    let temp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&temp)
            .map_err(|error| format!("创建临时指针文件失败：{error}"))?;
        file.write_all(serialized.as_bytes())
            .map_err(|error| format!("写入临时指针文件失败：{error}"))?;
        file.sync_all()
            .map_err(|error| format!("同步临时指针文件失败：{error}"))?;
    }
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("替换数据目录指针失败：{error}")
    })
}
