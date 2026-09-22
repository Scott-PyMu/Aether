//! M2-09 `artifact_ref` 引用帧语义（D6）：附件一律以引用帧上报（路径 + 元数据），
//! 引用帧本身必须 <1MiB；**数据体不进入线协议**——存 artifacts 文件，不落库。
//!
//! 全帧形状（`params`，行顶层 `"type":"artifact_ref"` 为判别键，见 [`crate::framing`]）：
//!
//! ```json
//! {
//!   "jsonrpc": "2.0",
//!   "method": "artifact_ref",
//!   "type": "artifact_ref",
//!   "params": {
//!     "session_id": "01J...",      // 可选
//!     "run_id": "01J...",          // 可选
//!     "refs": [
//!       { "path": "shot.png", "size": 3145728, "kind": "image/png" }
//!     ]
//!   }
//! }
//! ```
//!
//! 职责划分：
//! - [`crate::connection`]：线协议层解析（形状校验失败按「无效帧」计数，D6 失败场景表），
//!   产出 [`AdapterNotification::ArtifactRef`]；本模块不感知线协议；
//! - [`ArtifactValidator`]：**路径安全校验**（D6「存 artifacts 文件」的边界）——`path`
//!   必须为相对路径、不得含 `..` 段、canonicalize 后必须落在 artifacts 根目录内、
//!   文件必须存在且 `size` 与实际字节数一致；校验失败的引用帧**不落库、不广播**。
//!
//! 边界口径（与 D9 的 TOCTOU 限制同源）：canonicalize 与后续文件操作之间存在检查-使用
//! 竞态窗口，MVP 信任级下接受该限制（防误操作，不防蓄意攻击）；P3 沙箱以句柄级校验消除。

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// `artifact_ref` 引用帧方法名（D6；与顶层判别键 [`crate::framing::ARTIFACT_REF_TYPE`] 同值）。
pub const ARTIFACT_REF_METHOD: &str = "artifact_ref";

/// `artifact_ref` 通知参数形状（`params`；D6 全帧形状，M2-09 落全帧形状）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRefParams {
    /// 所属会话（可选；附件可先于事件流到达）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 所属 run（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// 附件引用列表（路径 + 元数据；**数据体不在此处**）。
    pub refs: Vec<ArtifactRefEntry>,
}

/// 单条附件引用（路径 + 元数据；数据体存 artifacts 文件，不进入线协议、不落库）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRefEntry {
    /// 相对 artifacts 根目录的路径（禁止绝对路径 / `..` / 盘符 / UNC）。
    pub path: String,
    /// 附件字节数（与实际文件大小核对）。
    pub size: u64,
    /// 附件类型等元数据（可选，如 `image/png`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// 校验通过的附件引用（含解析后的绝对路径，供消费者读取）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedRef {
    /// 相对路径（原样）。
    pub path: String,
    /// 附件字节数。
    pub size: u64,
    /// 元数据（可选）。
    pub kind: Option<String>,
    /// canonicalize 后的绝对路径（已在 artifacts 根目录内）。
    pub absolute: PathBuf,
}

/// 引用帧校验错误（校验失败 = 引用帧作废；不落库、不广播、不产生事件）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ArtifactError {
    /// `refs` 为空（无附件可引用）。
    #[error("artifact_ref refs 为空")]
    EmptyRefs,
    /// 路径非法（绝对路径 / `..` 段 / 盘符 / UNC 等）。
    #[error("artifact_ref 路径非法（{reason}）：{path}")]
    InvalidPath {
        /// 原始路径。
        path: String,
        /// 拒绝原因。
        reason: String,
    },
    /// canonicalize 失败（文件不存在或不可访问）。
    #[error("artifact_ref 文件不可访问：{path}")]
    Unreachable {
        /// 原始路径。
        path: String,
    },
    /// canonicalize 后不在 artifacts 根目录内（路径逃逸）。
    #[error("artifact_ref 路径逃逸：{path} 解析到 {resolved}（根目录 {root} 之外）")]
    OutsideRoot {
        /// 原始路径。
        path: String,
        /// 解析后的绝对路径。
        resolved: String,
        /// 根目录。
        root: String,
    },
    /// 声明的 `size` 与实际文件字节数不符。
    #[error("artifact_ref 尺寸不符：{path} 声明 {declared} 字节，实际 {actual} 字节")]
    SizeMismatch {
        /// 相对路径。
        path: String,
        /// 声明字节数。
        declared: u64,
        /// 实际字节数。
        actual: u64,
    },
}

/// 附件引用校验器：把引用帧限制在 artifacts 根目录内（D6「存 artifacts 文件」的边界）。
#[derive(Debug, Clone)]
pub struct ArtifactValidator {
    /// artifacts 根目录（canonicalize 后的基准；目录不存在时校验一律失败）。
    root: PathBuf,
}

impl ArtifactValidator {
    /// 构造校验器；`root` 会被 canonicalize（不可 canonicalize → 校验一律 `Unreachable`）。
    pub fn new(root: PathBuf) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        Self { root }
    }

    /// 校验基准根目录（canonicalize 后的值）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 校验引用帧参数：全部条目通过才返回；任一失败即整体拒绝。
    ///
    /// - `refs` 为空 → [`ArtifactError::EmptyRefs`]；
    /// - 相对路径（拒绝绝对路径 / `..` 段 / 盘符 / UNC）；
    /// - canonicalize 后前缀比较（解析软链接 / Junction，口径同 D9 路径校验）；
    /// - 文件存在且 `size` 与实际字节数一致。
    pub fn validate(&self, params: &ArtifactRefParams) -> Result<Vec<ValidatedRef>, ArtifactError> {
        if params.refs.is_empty() {
            return Err(ArtifactError::EmptyRefs);
        }
        let mut validated = Vec::with_capacity(params.refs.len());
        for entry in &params.refs {
            let relative = relative_safe(&entry.path)?;
            let absolute = self.root.join(&relative);
            let resolved = absolute
                .canonicalize()
                .map_err(|_| ArtifactError::Unreachable {
                    path: entry.path.clone(),
                })?;
            if !resolved.starts_with(&self.root) {
                return Err(ArtifactError::OutsideRoot {
                    path: entry.path.clone(),
                    resolved: resolved.to_string_lossy().into_owned(),
                    root: self.root.to_string_lossy().into_owned(),
                });
            }
            let actual = resolved.metadata().map(|meta| meta.len()).map_err(|_| {
                ArtifactError::Unreachable {
                    path: entry.path.clone(),
                }
            })?;
            if actual != entry.size {
                return Err(ArtifactError::SizeMismatch {
                    path: entry.path.clone(),
                    declared: entry.size,
                    actual,
                });
            }
            validated.push(ValidatedRef {
                path: entry.path.clone(),
                size: entry.size,
                kind: entry.kind.clone(),
                absolute: resolved,
            });
        }
        Ok(validated)
    }
}

/// 相对路径安全检查（纯路径语义，无 I/O）：
/// - 必须非空且为相对路径（绝对路径 / Windows 盘符 / UNC / 根路径拒绝）；
/// - 所有段不得为 `..`（含 `a/../b` 形态）；`ParentDir` 之外的所有 `..` 组合都在此拒绝；
/// - 空段（`a//b`、尾随 `/`）按普通段处理（canonicalize 后由前缀比较兜底）。
fn relative_safe(path: &str) -> Result<PathBuf, ArtifactError> {
    let raw = Path::new(path);
    let is_absolute = raw.is_absolute() || has_windows_prefix(path);
    if is_absolute {
        return Err(ArtifactError::InvalidPath {
            path: path.to_owned(),
            reason: "绝对路径（含盘符/UNC/根路径）不允许".to_owned(),
        });
    }
    for component in raw.components() {
        match component {
            Component::Normal(_) => {}
            Component::ParentDir => {
                return Err(ArtifactError::InvalidPath {
                    path: path.to_owned(),
                    reason: "`..` 段不允许".to_owned(),
                });
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ArtifactError::InvalidPath {
                    path: path.to_owned(),
                    reason: "非法路径段".to_owned(),
                });
            }
        }
    }
    if raw.components().next().is_none() {
        return Err(ArtifactError::InvalidPath {
            path: path.to_owned(),
            reason: "空路径".to_owned(),
        });
    }
    Ok(raw.to_path_buf())
}

/// Windows 盘符 / UNC 前缀探测（`C:`、`C:\`、`\\server\share`、`\\?\`）。
fn has_windows_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true;
    }
    bytes.starts_with(b"\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(refs: Vec<ArtifactRefEntry>) -> ArtifactRefParams {
        ArtifactRefParams {
            session_id: None,
            run_id: None,
            refs,
        }
    }

    fn entry(path: &str, size: u64) -> ArtifactRefEntry {
        ArtifactRefEntry {
            path: path.to_owned(),
            size,
            kind: Some("image/png".to_owned()),
        }
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let dir = std::env::temp_dir()
            .join("aether-m2-09-unit")
            .join(format!("{tag}-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn empty_refs_is_rejected() {
        let validator = ArtifactValidator::new(temp_root("empty"));
        assert_eq!(
            validator.validate(&params(Vec::new())),
            Err(ArtifactError::EmptyRefs)
        );
    }

    #[test]
    fn valid_relative_entry_passes_and_resolves_inside_root() {
        let root = temp_root("valid");
        let file = root.join("shot.png");
        std::fs::write(&file, vec![0xAB; 4096]).unwrap();
        let validator = ArtifactValidator::new(root.clone());
        let validated = validator
            .validate(&params(vec![entry("shot.png", 4096)]))
            .unwrap();
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].path, "shot.png");
        assert_eq!(validated[0].size, 4096);
        assert_eq!(validated[0].kind.as_deref(), Some("image/png"));
        assert_eq!(validated[0].absolute, file.canonicalize().unwrap());
    }

    #[test]
    fn nested_relative_path_is_allowed_when_inside_root() {
        let root = temp_root("nested");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("deep.bin"), vec![0x11; 8]).unwrap();
        let validator = ArtifactValidator::new(root);
        let validated = validator
            .validate(&params(vec![entry("sub/deep.bin", 8)]))
            .unwrap();
        assert_eq!(validated[0].path, "sub/deep.bin");
    }

    #[test]
    fn parent_dir_segments_are_rejected() {
        let root = temp_root("escape");
        let validator = ArtifactValidator::new(root);
        for bad in ["../outside.bin", "a/../b.bin", ".."] {
            let error = validator.validate(&params(vec![entry(bad, 1)]));
            assert!(
                matches!(error, Err(ArtifactError::InvalidPath { .. })),
                "{bad} 必须被拒绝: {error:?}"
            );
        }
    }

    #[test]
    fn absolute_and_windows_prefix_paths_are_rejected() {
        let root = temp_root("absolute");
        let validator = ArtifactValidator::new(root);
        for bad in [
            "/etc/passwd",
            "C:\\windows\\x",
            "C:evil",
            "\\\\server\\share",
            "\\\\?\\C:\\x",
        ] {
            let error = validator.validate(&params(vec![entry(bad, 1)]));
            assert!(
                matches!(error, Err(ArtifactError::InvalidPath { .. })),
                "{bad} 必须被拒绝: {error:?}"
            );
        }
    }

    #[test]
    fn missing_file_is_unreachable() {
        let root = temp_root("missing");
        let validator = ArtifactValidator::new(root);
        let error = validator.validate(&params(vec![entry("ghost.bin", 1)]));
        assert!(matches!(error, Err(ArtifactError::Unreachable { .. })));
    }

    #[test]
    fn size_mismatch_is_rejected() {
        let root = temp_root("size");
        std::fs::write(root.join("a.bin"), vec![0x22; 100]).unwrap();
        let validator = ArtifactValidator::new(root);
        let error = validator.validate(&params(vec![entry("a.bin", 99)]));
        assert_eq!(
            error,
            Err(ArtifactError::SizeMismatch {
                path: "a.bin".to_owned(),
                declared: 99,
                actual: 100,
            })
        );
    }

    #[test]
    fn symlink_escaping_root_is_rejected_when_supported() {
        let root = temp_root("symlink");
        let outside = temp_root("symlink-outside");
        std::fs::write(outside.join("secret.bin"), vec![0x33; 16]).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside.join("secret.bin"), root.join("link.bin")).unwrap();
            let validator = ArtifactValidator::new(root);
            let error = validator.validate(&params(vec![entry("link.bin", 16)]));
            assert!(
                matches!(error, Err(ArtifactError::OutsideRoot { .. })),
                "软链接逃逸必须被拒绝: {error:?}"
            );
        }
        #[cfg(windows)]
        {
            match std::os::windows::fs::symlink_file(
                outside.join("secret.bin"),
                root.join("link.bin"),
            ) {
                Ok(()) => {
                    let validator = ArtifactValidator::new(root);
                    let error = validator.validate(&params(vec![entry("link.bin", 16)]));
                    assert!(
                        matches!(error, Err(ArtifactError::OutsideRoot { .. })),
                        "软链接逃逸必须被拒绝: {error:?}"
                    );
                }
                Err(_) => {
                    // 开发者模式未开启时 symlink 创建失败：跳过（Win 常规 CI 无权限）。
                    eprintln!("SKIP：Windows 软链接创建失败（需开发者模式）");
                }
            }
        }
    }

    #[test]
    fn method_constant_matches_discriminator() {
        assert_eq!(ARTIFACT_REF_METHOD, crate::framing::ARTIFACT_REF_TYPE);
        assert_eq!(ARTIFACT_REF_METHOD, "artifact_ref");
    }
}
