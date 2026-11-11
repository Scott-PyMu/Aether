//! 附件内容存储（M10-05/ADR-035 D-035-2）。
//!
//! 口径：
//! - 内容寻址：`<data_dir>/attachments/<sha256[0..2]>/<sha256>.<ext>`；
//! - 同一内容全局去重；**不覆盖既有文件**（存在即视为已存储）；
//! - 原子写：同目录临时文件 → `sync_all` → rename；失败清理临时文件；
//! - 路径校验：canonicalize + 前缀必须位于 `<data_dir>/attachments/`（拒绝 `..`/
//!   符号链接/Junction 逃逸）；DB 只存相对路径；
//! - 孤儿清理：启动扫描 `attachments/` 中 >24h 无 DB 引用的文件（含缩略图缓存）。

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aether_adapters::connection::ConnectionState;
use aether_adapters::supervisor::Supervisor;
use aether_control::{
    AttachmentResolveError, AttachmentResolveErrorKind, AttachmentResolveFuture, AttachmentResolver,
};
use aether_core::{
    static_capabilities, AttachmentMime, ImageDimensions, Message, OutboundAttachment, SessionId,
    ATTACHMENT_MAX_BYTES, ATTACHMENT_MAX_EDGE, ATTACHMENT_MAX_PIXELS, MULTIMODAL_CAPABILITY,
};
use aether_store::ReadPool;
use sha2::{Digest, Sha256};
/// 附件根目录名（相对 `data_dir`）。
pub const ATTACHMENT_DIR: &str = "attachments";
/// 缩略图缓存子目录（不写 DB；ADR-035 D-035-3）。
pub const ATTACHMENT_THUMB_DIR: &str = "thumbs";
/// 临时文件子目录（原子写中间态；启动/清理时回收）。
pub const ATTACHMENT_TMP_DIR: &str = ".tmp";
/// 孤儿清理年龄（ADR-035 D-035-6：>24h 且无 DB 引用）。
pub const ATTACHMENT_ORPHAN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// 文件头读取长度（足够覆盖 PNG/JPEG/WebP/GIF 尺寸解析）。
pub const ATTACHMENT_HEADER_BYTES: usize = 64 * 1024;

/// 读取文件头并嗅探图像元数据（不解码整图）。
pub fn sniff_file(path: &Path) -> Result<(AttachmentMime, ImageDimensions), String> {
    let mut file = File::open(path).map_err(|error| format!("附件源不可读：{error}"))?;
    let mut header = vec![0u8; ATTACHMENT_HEADER_BYTES];
    let read = file
        .read(&mut header)
        .map_err(|error| format!("附件源读取失败：{error}"))?;
    header.truncate(read);
    aether_core::sniff_image(&header).ok_or_else(|| {
        "附件格式不受支持（仅 image/png、image/jpeg、image/webp、image/gif）".to_owned()
    })
}

/// 读取完整文件并计算 sha256（单文件硬上限 10 MiB；超限不继续读）。
pub fn read_and_hash(path: &Path) -> Result<(Vec<u8>, String), AttachmentReadError> {
    let metadata = fs::metadata(path).map_err(AttachmentReadError::Io)?;
    if metadata.len() > ATTACHMENT_MAX_BYTES {
        return Err(AttachmentReadError::TooLarge(metadata.len()));
    }
    let file = File::open(path).map_err(AttachmentReadError::Io)?;
    let mut bytes = Vec::new();
    file.take(ATTACHMENT_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(AttachmentReadError::Io)?;
    if bytes.len() as u64 > ATTACHMENT_MAX_BYTES {
        return Err(AttachmentReadError::TooLarge(bytes.len() as u64));
    }
    let digest = Sha256::digest(&bytes);
    Ok((bytes, hex::encode(digest)))
}

/// 附件读取错误（映射到稳定业务码）。
#[derive(Debug)]
pub enum AttachmentReadError {
    Io(std::io::Error),
    TooLarge(u64),
}

/// 相对 `data_dir` 的内容路径（DB `storage_path` 唯一形态）。
pub fn relative_content_path(sha256: &str, mime: AttachmentMime) -> String {
    let prefix = &sha256[..2.min(sha256.len())];
    format!("{ATTACHMENT_DIR}/{prefix}/{sha256}.{}", mime.extension())
}

/// 相对 `data_dir` 的缩略图缓存路径。
pub fn relative_thumb_path(sha256: &str) -> String {
    format!("{ATTACHMENT_DIR}/{ATTACHMENT_THUMB_DIR}/{sha256}.webp")
}

/// 附件根目录（绝对）。
pub fn attachments_root(data_dir: &Path) -> PathBuf {
    data_dir.join(ATTACHMENT_DIR)
}

/// 把 DB 中的相对 `storage_path` 解析为**已校验**的绝对路径。
///
/// 拒绝绝对路径、`..`、以及 canonicalize 后逃出附件根目录的路径。
pub fn resolve_storage_path(data_dir: &Path, relative: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return Err("storage_path 不得为绝对路径".to_owned());
    }
    if candidate
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("storage_path 不得包含 ..".to_owned());
    }
    if !relative.starts_with(&format!("{ATTACHMENT_DIR}/"))
        && !relative.starts_with(&format!("{ATTACHMENT_DIR}\\"))
    {
        return Err("storage_path 必须位于 attachments/ 下".to_owned());
    }
    let root = attachments_root(data_dir);
    fs::create_dir_all(&root).map_err(|error| format!("附件目录创建失败：{error}"))?;
    let canonical_root =
        fs::canonicalize(&root).map_err(|error| format!("附件目录不可解析：{error}"))?;
    let absolute = data_dir.join(candidate);
    // 文件可能尚未存在（孤儿判定）；以父目录 canonicalize + 文件名拼接校验前缀。
    let parent = absolute
        .parent()
        .ok_or_else(|| "storage_path 缺少父目录".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| format!("附件子目录创建失败：{error}"))?;
    let canonical_parent =
        fs::canonicalize(parent).map_err(|error| format!("附件子目录不可解析：{error}"))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err("storage_path 逃出附件根目录（路径白名单拒绝）".to_owned());
    }
    let file_name = absolute
        .file_name()
        .ok_or_else(|| "storage_path 缺少文件名".to_owned())?;
    Ok(canonical_parent.join(file_name))
}

/// 原子写入内容文件（已存在则复用，不覆盖）。
///
/// 返回 `(absolute_path, inserted)`；`inserted=false` 表示同内容已存在（全局去重）。
pub fn store_content(
    data_dir: &Path,
    sha256: &str,
    mime: AttachmentMime,
    bytes: &[u8],
) -> Result<(PathBuf, bool), String> {
    let relative = relative_content_path(sha256, mime);
    let target = resolve_storage_path(data_dir, &relative)?;
    if target.is_file() {
        return Ok((target, false));
    }
    let tmp_dir = attachments_root(data_dir).join(ATTACHMENT_TMP_DIR);
    fs::create_dir_all(&tmp_dir).map_err(|error| format!("附件临时目录创建失败：{error}"))?;
    let temp = tmp_dir.join(format!("{sha256}.{}.tmp", std::process::id()));
    let write_result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| format!("附件临时文件创建失败：{error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("附件写入失败：{error}"))?;
        file.sync_all()
            .map_err(|error| format!("附件 fsync 失败：{error}"))?;
        drop(file);
        // Windows rename 在目标存在时失败；再次检查避免覆盖并发写入的同内容文件。
        if target.exists() {
            let _ = fs::remove_file(&temp);
            return Ok(());
        }
        fs::rename(&temp, &target).map_err(|error| format!("附件原子改名失败：{error}"))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result.map(|()| (target, true))
}

/// 删除单文件（不存在视为成功；失败上抛供诊断）。
pub fn remove_content_if_unreferenced(data_dir: &Path, relative: &str) -> Result<(), String> {
    let path = resolve_storage_path(data_dir, relative)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("附件文件删除失败：{error}")),
    }
}

/// 启动孤儿扫描：删除 >24h 且不在 DB 引用集合内的内容/缩略图/临时文件。
///
/// 返回 `(cleaned, kept)` 计数；不删除仍被引用的文件。IO 错误只跳过单个文件
/// （诊断在调用方记录；清理失败不阻断启动）。
pub fn cleanup_orphans(
    data_dir: &Path,
    referenced: &HashSet<String>,
    now: SystemTime,
) -> (u64, u64) {
    let root = attachments_root(data_dir);
    let mut cleaned = 0u64;
    let mut kept = 0u64;
    if !root.is_dir() {
        return (0, 0);
    }
    let thumb_dir = root.join(ATTACHMENT_THUMB_DIR);
    let tmp_dir = root.join(ATTACHMENT_TMP_DIR);
    let mut candidates = Vec::new();
    // 内容文件位于 `<root>/<sha[0..2]>/<sha>.<ext>`：遍历两级，跳过 thumbs/.tmp。
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path == thumb_dir || path == tmp_dir {
                    continue;
                }
                if let Ok(children) = fs::read_dir(&path) {
                    for child in children.flatten() {
                        if child.path().is_file() {
                            candidates.push((child.path(), false));
                        }
                    }
                }
            } else {
                candidates.push((path, false));
            }
        }
    }
    for (directory, is_tmp) in [(thumb_dir, false), (tmp_dir, true)] {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().is_file() {
                candidates.push((entry.path(), is_tmp));
            }
        }
    }
    for (path, is_tmp) in candidates {
        let relative = match path.strip_prefix(data_dir) {
            Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if !is_tmp && referenced.contains(&relative) {
            kept += 1;
            continue;
        }
        let age_ok = fs::metadata(&path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= ATTACHMENT_ORPHAN_AGE);
        if !age_ok {
            kept += 1;
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            cleaned += 1;
        }
    }
    (cleaned, kept)
}

/// 附件重放解析器（M10-05/ADR-035）：retry/resume 从 `content_parts` 重新水合。
///
/// 每次重放重新校验：
/// - 运行时 `multimodal`（静态表或 hello/initialize）；
/// - 模型 `vision`（明确 false 拒绝；未知允许）；
/// - 附件归属/文件存在/canonicalize 路径/内容 sha256/size/像素上限；
/// - 单消息 ≤8。
///
/// 任一失败返回结构化错误（由控制层映射业务码），不静默降级为纯文本。
pub struct AttachmentReplayResolver {
    reads: ReadPool,
    data_dir: PathBuf,
    supervisor: Option<Arc<Supervisor>>,
}

impl AttachmentReplayResolver {
    pub fn new(
        reads: ReadPool,
        data_dir: impl Into<PathBuf>,
        supervisor: Option<Arc<Supervisor>>,
    ) -> Self {
        Self {
            reads,
            data_dir: data_dir.into(),
            supervisor,
        }
    }

    async fn resolve_inner(
        &self,
        session_id: &SessionId,
        message: &Message,
    ) -> Result<Vec<OutboundAttachment>, AttachmentResolveError> {
        let parts = message
            .content_parts
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                AttachmentResolveError::new(
                    AttachmentResolveErrorKind::Invalid,
                    "messages.content_parts 不是数组（不变量破坏）",
                )
            })?;
        if parts.len() > aether_core::ATTACHMENT_MAX_PER_MESSAGE {
            return Err(AttachmentResolveError::new(
                AttachmentResolveErrorKind::LimitExceeded,
                format!(
                    "单条消息附件上限 {} 个（实际 {}）",
                    aether_core::ATTACHMENT_MAX_PER_MESSAGE,
                    parts.len()
                ),
            ));
        }
        let session = self
            .reads
            .session(session_id)
            .await
            .map_err(|error| {
                AttachmentResolveError::new(
                    AttachmentResolveErrorKind::StorageFailed,
                    format!("会话读取失败：{error}"),
                )
            })?
            .ok_or_else(|| {
                AttachmentResolveError::new(
                    AttachmentResolveErrorKind::StorageFailed,
                    "会话不存在（无法校验附件重放能力）",
                )
            })?;
        // 运行时 multimodal：静态表优先，未声明时看 hello/initialize。
        let mut runtime_supported =
            static_capabilities(session.runtime_id.as_str()).contains(&MULTIMODAL_CAPABILITY);
        if !runtime_supported {
            if let Some(supervisor) = self.supervisor.as_ref() {
                if let Some(runtime) = supervisor.get(session.runtime_id.as_str()) {
                    if let Some(connection) = runtime.connection().await {
                        runtime_supported = match connection.state() {
                            ConnectionState::Ready(hello)
                            | ConnectionState::Degraded { hello, .. } => hello
                                .runtime
                                .capabilities
                                .iter()
                                .any(|item| item == MULTIMODAL_CAPABILITY),
                            ConnectionState::Connecting | ConnectionState::Disconnected(_) => false,
                        };
                    }
                }
            }
        }
        if !runtime_supported {
            return Err(AttachmentResolveError::new(
                AttachmentResolveErrorKind::Unsupported,
                format!(
                    "运行时 {} 未声明 multimodal：拒绝重放附件（不静默丢弃）",
                    session.runtime_id.as_str()
                ),
            ));
        }
        // 模型 vision：明确 false 拒绝；未知允许（与首次发送口径一致）。
        if let (Some(provider_id), Some(model)) = (
            crate::session_backend::session_provider_id(&session),
            session.model.as_deref(),
        ) {
            let models = self.reads.provider_models().await.map_err(|error| {
                AttachmentResolveError::new(
                    AttachmentResolveErrorKind::StorageFailed,
                    format!("供应商模型读取失败：{error}"),
                )
            })?;
            if let Some(record) = models
                .iter()
                .find(|item| item.provider_id == provider_id && item.model_id == model)
            {
                if crate::session_backend::vision_state(&record.capabilities)
                    == crate::session_backend::VisionState::Blocked
                {
                    return Err(AttachmentResolveError::new(
                        AttachmentResolveErrorKind::Unsupported,
                        format!("模型 {model} 明确声明 vision=false：拒绝重放附件"),
                    ));
                }
            }
        }

        let mut attachments = Vec::with_capacity(parts.len());
        for part in parts {
            let attachment_id = part
                .get("attachment_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    AttachmentResolveError::new(
                        AttachmentResolveErrorKind::Invalid,
                        "content_parts[] 缺少 attachment_id",
                    )
                })?;
            let record = self
                .reads
                .attachment(attachment_id)
                .await
                .map_err(|error| {
                    AttachmentResolveError::new(
                        AttachmentResolveErrorKind::StorageFailed,
                        format!("附件读取失败：{error}"),
                    )
                })?
                .ok_or_else(|| {
                    AttachmentResolveError::new(
                        AttachmentResolveErrorKind::StorageFailed,
                        format!("附件不存在或已删除：{attachment_id}"),
                    )
                })?;
            if record.session_id != session_id.as_str() {
                return Err(AttachmentResolveError::new(
                    AttachmentResolveErrorKind::StorageFailed,
                    format!("附件不属于当前会话：{attachment_id}"),
                ));
            }
            let mime = AttachmentMime::from_wire(&record.mime).ok_or_else(|| {
                AttachmentResolveError::new(
                    AttachmentResolveErrorKind::Invalid,
                    format!("附件 MIME 非白名单（DB 不变量破坏）：{}", record.mime),
                )
            })?;
            let dimensions = match (record.width, record.height) {
                (Some(width), Some(height)) => Some(ImageDimensions {
                    width: u32::try_from(width).map_err(|_| {
                        AttachmentResolveError::new(
                            AttachmentResolveErrorKind::TooLarge,
                            format!("附件宽度越界：{width}"),
                        )
                    })?,
                    height: u32::try_from(height).map_err(|_| {
                        AttachmentResolveError::new(
                            AttachmentResolveErrorKind::TooLarge,
                            format!("附件高度越界：{height}"),
                        )
                    })?,
                }),
                _ => None,
            };
            let absolute =
                resolve_storage_path(&self.data_dir, &record.storage_path).map_err(|error| {
                    AttachmentResolveError::new(
                        AttachmentResolveErrorKind::StorageFailed,
                        format!("附件路径校验失败：{error}"),
                    )
                })?;
            if !absolute.is_file() {
                return Err(AttachmentResolveError::new(
                    AttachmentResolveErrorKind::StorageFailed,
                    format!("附件文件缺失：{attachment_id}"),
                ));
            }
            let (bytes, sha256) = read_and_hash(&absolute).map_err(|error| match error {
                AttachmentReadError::TooLarge(size) => AttachmentResolveError::new(
                    AttachmentResolveErrorKind::TooLarge,
                    format!(
                        "附件超过单文件上限 {} 字节（实际 {size}）",
                        ATTACHMENT_MAX_BYTES
                    ),
                ),
                AttachmentReadError::Io(error) => AttachmentResolveError::new(
                    AttachmentResolveErrorKind::StorageFailed,
                    format!("附件读取失败：{error}"),
                ),
            })?;
            if sha256 != record.sha256 || i64::try_from(bytes.len()).ok() != Some(record.size_bytes)
            {
                return Err(AttachmentResolveError::new(
                    AttachmentResolveErrorKind::StorageFailed,
                    format!("附件内容校验失败（sha256/size 不一致）：{attachment_id}"),
                ));
            }
            let dimensions = match dimensions {
                Some(dimensions) => dimensions,
                None => sniff_file(&absolute)
                    .map(|(_, dimensions)| dimensions)
                    .map_err(|error| {
                        AttachmentResolveError::new(
                            AttachmentResolveErrorKind::Invalid,
                            format!("附件尺寸读取失败：{error}"),
                        )
                    })?,
            };
            if !dimensions.within_limits() {
                return Err(AttachmentResolveError::new(
                    AttachmentResolveErrorKind::TooLarge,
                    format!(
                        "图像超限：{}×{}（上限 {} 像素且最长边 ≤ {}）",
                        dimensions.width,
                        dimensions.height,
                        ATTACHMENT_MAX_PIXELS,
                        ATTACHMENT_MAX_EDGE
                    ),
                ));
            }
            attachments.push(OutboundAttachment {
                attachment_id: record.id.clone(),
                mime,
                width: Some(dimensions.width),
                height: Some(dimensions.height),
                path: absolute.to_string_lossy().to_string(),
                size_bytes: record.size_bytes,
                sha256: record.sha256.clone(),
            });
        }
        Ok(attachments)
    }
}

impl AttachmentResolver for AttachmentReplayResolver {
    fn resolve<'a>(
        &'a self,
        session_id: &'a SessionId,
        message: &'a Message,
    ) -> AttachmentResolveFuture<'a> {
        Box::pin(async move { self.resolve_inner(session_id, message).await })
    }
}

/// 诊断指标（ADR-035 D-035-6：只含数量/字节数，不含绝对路径）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttachmentDiagnostics {
    pub count: u64,
    pub bytes: u64,
    pub orphans_cleaned: u64,
    pub thumb_generated: u64,
    pub thumb_failed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aether-m10-05-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn store_content_is_content_addressed_and_deduplicated() {
        let dir = temp_dir("store");
        let bytes = b"hello attachment";
        let sha = hex::encode(Sha256::digest(bytes));
        let (path, inserted) =
            store_content(&dir, &sha, AttachmentMime::Png, bytes).expect("store");
        assert!(inserted);
        assert!(path.is_file());
        let relative = relative_content_path(&sha, AttachmentMime::Png);
        assert_eq!(relative, format!("attachments/{}/{sha}.png", &sha[..2]));
        let (path2, inserted2) =
            store_content(&dir, &sha, AttachmentMime::Png, bytes).expect("dedupe");
        assert!(!inserted2);
        assert_eq!(path, path2);
        assert_eq!(fs::read(&path).expect("read"), bytes);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_rejects_escape_and_absolute_paths() {
        let dir = temp_dir("escape");
        for bad in [
            "../evil.png",
            "attachments/../../evil.png",
            "C:/evil.png",
            "other/evil.png",
        ] {
            assert!(resolve_storage_path(&dir, bad).is_err(), "{bad}");
        }
        assert!(resolve_storage_path(&dir, "attachments/aa/aa.png").is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_keeps_referenced_and_fresh_files() {
        let dir = temp_dir("cleanup");
        let old = b"old";
        let old_sha = hex::encode(Sha256::digest(old));
        let (old_path, _) = store_content(&dir, &old_sha, AttachmentMime::Png, old).expect("old");
        let fresh = b"fresh";
        let fresh_sha = hex::encode(Sha256::digest(fresh));
        let (fresh_path, _) =
            store_content(&dir, &fresh_sha, AttachmentMime::Png, fresh).expect("fresh");
        // 人为把旧文件 mtime 回拨 48h。
        let old_time = SystemTime::now() - Duration::from_secs(48 * 60 * 60);
        set_modified(&old_path, old_time);
        let referenced: HashSet<String> =
            [relative_content_path(&fresh_sha, AttachmentMime::Png)].into();
        let (cleaned, kept) = cleanup_orphans(&dir, &referenced, SystemTime::now());
        assert_eq!(cleaned, 1);
        assert!(kept >= 1);
        assert!(!old_path.exists());
        assert!(fresh_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    fn set_modified(path: &Path, time: SystemTime) {
        use std::fs::FileTimes;
        let file = OpenOptions::new().write(true).open(path).expect("open");
        file.set_times(FileTimes::new().set_modified(time))
            .expect("set mtime");
    }

    #[test]
    fn read_and_hash_rejects_oversized_file() {
        let dir = temp_dir("large");
        let path = dir.join("large.bin");
        let mut file = File::create(&path).expect("create");
        file.write_all(&vec![0u8; 11 * 1024 * 1024]).expect("write");
        file.sync_all().expect("sync");
        assert!(matches!(
            read_and_hash(&path),
            Err(AttachmentReadError::TooLarge(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
