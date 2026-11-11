//! 多模态附件模型（M10-05 / ADR-035 v1.1）。
//!
//! 本模块只承载**纯模型**（限额常量、MIME 白名单、魔数与文件头尺寸解析、
//! 线协议/`content_parts` 载荷形态），不做任何 I/O：
//! - 文件读取、哈希、原子存储、数据库读写分别在 `aether-app` / `aether-store`；
//! - 解码/缩略图在 Electron UtilityProcess（ADR-035 D-035-3），核心只读文件头。
//!
//! 安全口径（D-035-1/D-035-6）：
//! - 只允许 PNG/JPEG/WebP/GIF，按魔数嗅探，不信任扩展名；
//! - 像素/边长检查必须在构造解码器前完成（`@napi-rs/image` 无 `limitInputPixels`）；
//! - SVG/PDF/音视频/任意二进制一律拒绝。

use serde::{Deserialize, Serialize};

/// 单文件硬上限（ADR-035 D-035-1：10 MiB）。
pub const ATTACHMENT_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// 单条消息附件数上限（ADR-035 D-035-1：8）。
pub const ATTACHMENT_MAX_PER_MESSAGE: usize = 8;
/// 单会话附件数上限（ADR-035 D-035-1：200）。
pub const ATTACHMENT_MAX_PER_SESSION: usize = 200;
/// 单会话附件总量上限（ADR-035 D-035-1：1 GiB）。
pub const ATTACHMENT_MAX_SESSION_BYTES: u64 = 1024 * 1024 * 1024;
/// 单图像素上限（ADR-035 附录 A / v1.1 Q9：25,000,000）。
pub const ATTACHMENT_MAX_PIXELS: u64 = 25_000_000;
/// 单图最长边上限（ADR-035 v1.1 Q9：8192）。
pub const ATTACHMENT_MAX_EDGE: u32 = 8192;
/// 文件名长度上限（字符数；ADR-035 D-035-1：255）。
pub const ATTACHMENT_FILENAME_MAX_CHARS: usize = 255;

/// 运行时多模态能力项（ADR-035 D-035-4：`hello.runtime.capabilities` 字符串项）。
pub const MULTIMODAL_CAPABILITY: &str = "multimodal";
/// 模型视觉能力项（ADR-035 D-035-4：`provider_models.capabilities` 元素）。
pub const VISION_CAPABILITY: &str = "vision";
/// 模型能力未声明时的非阻断警告码（ADR-035 D-035-4）。
pub const MULTIMODAL_MODEL_UNVERIFIED: &str = "multimodal_model_unverified";

/// 允许的图像 MIME（ADR-035 附录 A）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentMime {
    Png,
    Jpeg,
    Webp,
    Gif,
}

impl AttachmentMime {
    /// 线上 MIME 字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }

    /// 内容寻址文件扩展名（不含点）。
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }

    /// 由线上 MIME 反解；未知值返回 `None`（调用方拒绝）。
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            "image/gif" => Some(Self::Gif),
            _ => None,
        }
    }
}

/// 图像文件头尺寸（只读元数据，不整图解码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

impl ImageDimensions {
    /// 像素总数（`u64`，避免乘法溢出）。
    pub const fn pixels(self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// ADR-035 前置上限：像素 ≤ 25MP 且最长边 ≤ 8192。
    pub fn within_limits(self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width.max(self.height) <= ATTACHMENT_MAX_EDGE
            && self.pixels() <= ATTACHMENT_MAX_PIXELS
    }
}

/// `session.send.attachments` 线协议载荷（ADR-035 D-035-3）。
///
/// **数据体不进线协议**（D6）：只传 canonical 路径与元数据，适配器按路径读取。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageAttachment {
    /// 当前仅 `image`。
    pub kind: String,
    /// canonicalize 后的绝对路径（适配器按此读取；信任级边界同 artifacts）。
    pub path: String,
    pub mime: String,
    pub size_bytes: i64,
    pub sha256: String,
    /// 引用帧标记（ADR-035：恒为 `true`，帧 <1MiB）。
    pub artifact_ref: bool,
}

impl MessageAttachment {
    /// 构造图像附件引用。
    pub fn image(path: String, mime: AttachmentMime, size_bytes: i64, sha256: String) -> Self {
        Self {
            kind: "image".to_owned(),
            path,
            mime: mime.as_str().to_owned(),
            size_bytes,
            sha256,
            artifact_ref: true,
        }
    }
}

/// `messages.content_parts` 中的附件引用（ADR-035 D-035-2：不存二进制）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentContentPart {
    /// 当前仅 `image`。
    pub kind: String,
    pub attachment_id: String,
    pub mime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
}

impl AttachmentContentPart {
    /// 构造图像引用部分。
    pub fn image(
        attachment_id: String,
        mime: AttachmentMime,
        dimensions: Option<ImageDimensions>,
    ) -> Self {
        Self {
            kind: "image".to_owned(),
            attachment_id,
            mime: mime.as_str().to_owned(),
            width: dimensions.map(|size| i64::from(size.width)),
            height: dimensions.map(|size| i64::from(size.height)),
        }
    }
}

/// 发送时附件的完整内核形态（命令层校验 → 控制层组装消息 → 执行器透传线协议）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundAttachment {
    pub attachment_id: String,
    pub mime: AttachmentMime,
    /// 文件头尺寸（发送前已按 ADR-035 上限校验）。
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// canonicalize 后的绝对路径（适配器按路径读取）。
    pub path: String,
    pub size_bytes: i64,
    pub sha256: String,
}

impl OutboundAttachment {
    /// 线协议载荷（D6：只传引用，数据体不进线协议）。
    pub fn wire(&self) -> MessageAttachment {
        MessageAttachment::image(
            self.path.clone(),
            self.mime,
            self.size_bytes,
            self.sha256.clone(),
        )
    }

    /// `messages.content_parts` 附件引用（不存二进制）。
    pub fn content_part(&self) -> AttachmentContentPart {
        AttachmentContentPart {
            kind: "image".to_owned(),
            attachment_id: self.attachment_id.clone(),
            mime: self.mime.as_str().to_owned(),
            width: self.width.map(i64::from),
            height: self.height.map(i64::from),
        }
    }
}

/// 按魔数嗅探图像格式与文件头尺寸（ADR-035 附录 A）。
///
/// 输入只需文件头（建议 ≥ 64 字节）；返回 `None` = 非允许格式或头部不完整。
/// **不**做整图解码；像素/边长上限由 [`ImageDimensions::within_limits`] 判定。
pub fn sniff_image(header: &[u8]) -> Option<(AttachmentMime, ImageDimensions)> {
    if header.len() >= 24
        && header[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        && &header[12..16] == b"IHDR"
    {
        let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
        let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
        return Some((AttachmentMime::Png, ImageDimensions { width, height }));
    }
    if header.len() >= 10 && (&header[..6] == b"GIF87a" || &header[..6] == b"GIF89a") {
        let width = u16::from_le_bytes([header[6], header[7]]) as u32;
        let height = u16::from_le_bytes([header[8], header[9]]) as u32;
        return Some((AttachmentMime::Gif, ImageDimensions { width, height }));
    }
    if header.len() >= 4 && header[..3] == [0xFF, 0xD8, 0xFF] {
        return sniff_jpeg(header).map(|size| (AttachmentMime::Jpeg, size));
    }
    if header.len() >= 21 && &header[..4] == b"RIFF" && &header[8..12] == b"WEBP" {
        return sniff_webp(header).map(|size| (AttachmentMime::Webp, size));
    }
    None
}

/// JPEG：扫描 SOF0–SOF15（排除 DHT/JPG/DAC）读取高/宽。
fn sniff_jpeg(header: &[u8]) -> Option<ImageDimensions> {
    let mut index = 2;
    while index + 4 <= header.len() {
        if header[index] != 0xFF {
            index += 1;
            continue;
        }
        // 跳过填充字节。
        while index < header.len() && header[index] == 0xFF {
            index += 1;
        }
        if index >= header.len() {
            return None;
        }
        let marker = header[index];
        index += 1;
        // 无长度的独立标记。
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            continue;
        }
        if index + 2 > header.len() {
            return None;
        }
        let length = u16::from_be_bytes([header[index], header[index + 1]]) as usize;
        if length < 2 || index + length > header.len() {
            return None;
        }
        // SOF0..SOF3 / SOF5..SOF7 / SOF9..SOF11 / SOF13..SOF15。
        let is_sof = matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        );
        if is_sof {
            if length < 7 {
                return None;
            }
            let height = u16::from_be_bytes([header[index + 3], header[index + 4]]) as u32;
            let width = u16::from_be_bytes([header[index + 5], header[index + 6]]) as u32;
            return Some(ImageDimensions { width, height });
        }
        index += length;
    }
    None
}

/// WebP：RIFF 容器内 VP8 / VP8L / VP8X 三种码流头部。
fn sniff_webp(header: &[u8]) -> Option<ImageDimensions> {
    let chunk = &header[12..16];
    match chunk {
        b"VP8 " => {
            // 帧头：3 字节帧标记 + 起始码 9D 01 2A，随后宽/高各 16 位（14 位有效）。
            if header.len() < 30 || header[23..26] != [0x9D, 0x01, 0x2A] {
                return None;
            }
            let width = (u16::from_le_bytes([header[26], header[27]]) & 0x3FFF) as u32;
            let height = (u16::from_le_bytes([header[28], header[29]]) & 0x3FFF) as u32;
            Some(ImageDimensions { width, height })
        }
        b"VP8L" => {
            if header.len() < 25 || header[20] != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes([header[21], header[22], header[23], header[24]]);
            let width = (bits & 0x3FFF) + 1;
            let height = ((bits >> 14) & 0x3FFF) + 1;
            Some(ImageDimensions { width, height })
        }
        b"VP8X" => {
            if header.len() < 30 {
                return None;
            }
            let width = u32::from_le_bytes([header[24], header[25], header[26], 0]) + 1;
            let height = u32::from_le_bytes([header[27], header[28], header[29], 0]) + 1;
            Some(ImageDimensions { width, height })
        }
        _ => None,
    }
}

/// 文件名清洗（ADR-035 D-035-6）：去路径分隔符、控制字符与双向文本控制符，
/// 截断到 255 字符；空名回退 `attachment`。**不用于磁盘路径**，仅展示/alt。
pub fn sanitize_attachment_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .filter(|ch| {
            !ch.is_control()
                && !matches!(
                    ch,
                    '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}'
                )
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return "attachment".to_owned();
    }
    cleaned
        .chars()
        .take(ATTACHMENT_FILENAME_MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    #[test]
    fn png_dimensions_and_limits() {
        let (mime, size) = sniff_image(&png(64, 48)).expect("png");
        assert_eq!(mime, AttachmentMime::Png);
        assert_eq!(
            size,
            ImageDimensions {
                width: 64,
                height: 48
            }
        );
        assert!((sniff_image(&png(5000, 5000)).expect("png").1).within_limits());
        assert!(!(sniff_image(&png(5001, 5000)).expect("png").1).within_limits());
        assert!(!(sniff_image(&png(8193, 1)).expect("png").1).within_limits());
        assert!(!(sniff_image(&png(0, 10)).expect("png").1).within_limits());
    }

    #[test]
    fn gif_first_frame_header() {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&64u16.to_le_bytes());
        bytes.extend_from_slice(&48u16.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);
        let (mime, size) = sniff_image(&bytes).expect("gif");
        assert_eq!(mime, AttachmentMime::Gif);
        assert_eq!(
            size,
            ImageDimensions {
                width: 64,
                height: 48
            }
        );
    }

    #[test]
    fn jpeg_sof_dimensions() {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        bytes.extend_from_slice(&[0; 14]);
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&48u16.to_be_bytes());
        bytes.extend_from_slice(&64u16.to_be_bytes());
        bytes.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        let (mime, size) = sniff_image(&bytes).expect("jpeg");
        assert_eq!(mime, AttachmentMime::Jpeg);
        assert_eq!(
            size,
            ImageDimensions {
                width: 64,
                height: 48
            }
        );
    }

    #[test]
    fn webp_lossy_and_lossless_dimensions() {
        let mut lossy = b"RIFF".to_vec();
        lossy.extend_from_slice(&[0, 0, 0, 0]);
        lossy.extend_from_slice(b"WEBP");
        lossy.extend_from_slice(b"VP8 ");
        lossy.extend_from_slice(&[10, 0, 0, 0]);
        lossy.extend_from_slice(&[0, 0, 0, 0x9D, 0x01, 0x2A]);
        lossy.extend_from_slice(&64u16.to_le_bytes());
        lossy.extend_from_slice(&48u16.to_le_bytes());
        let (mime, size) = sniff_image(&lossy).expect("webp lossy");
        assert_eq!(mime, AttachmentMime::Webp);
        assert_eq!(
            size,
            ImageDimensions {
                width: 64,
                height: 48
            }
        );

        let mut lossless = b"RIFF".to_vec();
        lossless.extend_from_slice(&[0, 0, 0, 0]);
        lossless.extend_from_slice(b"WEBP");
        lossless.extend_from_slice(b"VP8L");
        lossless.extend_from_slice(&[5, 0, 0, 0]);
        lossless.push(0x2F);
        let bits: u32 = 63 | (47 << 14);
        lossless.extend_from_slice(&bits.to_le_bytes());
        let (_, size) = sniff_image(&lossless).expect("webp lossless");
        assert_eq!(
            size,
            ImageDimensions {
                width: 64,
                height: 48
            }
        );
    }

    #[test]
    fn rejects_non_whitelisted_magic() {
        assert!(sniff_image(b"%PDF-1.7").is_none());
        assert!(sniff_image(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>").is_none());
        assert!(sniff_image(&[0u8; 64]).is_none());
    }

    #[test]
    fn sanitize_name_strips_paths_controls_and_bidi() {
        assert_eq!(sanitize_attachment_name(r"C:\tmp\pic.png"), "pic.png");
        assert_eq!(
            sanitize_attachment_name("/tmp/\u{202E}pic\n.png"),
            "pic.png"
        );
        assert_eq!(sanitize_attachment_name("   "), "attachment");
        let long = "a".repeat(400);
        assert_eq!(sanitize_attachment_name(&long).chars().count(), 255);
    }

    #[test]
    fn message_attachment_wire_shape_matches_adr_035() {
        let outbound = OutboundAttachment {
            attachment_id: "01J000000000000000000000A1".to_owned(),
            mime: AttachmentMime::Png,
            width: Some(64),
            height: Some(48),
            path: "C:/data/attachments/aa/aa.png".to_owned(),
            size_bytes: 2048,
            sha256: "aa".repeat(32),
        };
        let wire = serde_json::to_value(outbound.wire()).expect("wire");
        assert_eq!(
            wire,
            serde_json::json!({
                "kind": "image",
                "path": "C:/data/attachments/aa/aa.png",
                "mime": "image/png",
                "size_bytes": 2048,
                "sha256": "aa".repeat(32),
                "artifact_ref": true,
            })
        );
        let part = serde_json::to_value(outbound.content_part()).expect("part");
        assert_eq!(part["kind"], "image");
        assert_eq!(part["attachment_id"], "01J000000000000000000000A1");
        assert_eq!(part["width"], 64);
        assert!(
            part.get("sha256").is_none(),
            "content_parts 不扩字段（ADR-035）"
        );
    }

    #[test]
    fn mime_round_trip_and_extensions() {
        for mime in [
            AttachmentMime::Png,
            AttachmentMime::Jpeg,
            AttachmentMime::Webp,
            AttachmentMime::Gif,
        ] {
            assert_eq!(AttachmentMime::from_wire(mime.as_str()), Some(mime));
        }
        assert_eq!(AttachmentMime::from_wire("image/svg+xml"), None);
    }
}
