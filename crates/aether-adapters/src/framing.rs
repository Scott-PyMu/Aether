//! D6 帧层：LF 分隔、单帧硬上限 2MiB、`artifact_ref` <1MiB 契约（ADR-003/ADR-004）。
//!
//! **为什么自定义有界增量读取器（而非原样 `LinesCodec`）**：`LinesCodec` 会先把整行
//! 缓冲到 `max_length` 才判定/返回，无法在「超过帧上限时立即停止缓冲」，也无法在
//! 缓冲过程中探测 `artifact_ref` 并执行其 <1MiB 契约；若沿用 LinesCodec，2MiB 上限
//! 之下的越界引用帧与超限普通行都会先完整入内存（反事实见 `docs/M1-09-证据.md` §2.1）。
//! 因此本模块保留 LinesCodec 的**行语义**（LF 分隔、容忍 CRLF、UTF-8 校验），
//! 叠加有界策略：
//! - 行 ≤2MiB：正常缓冲解析（1–2MiB 的非引用行同样正常解析）；
//! - 行 >2MiB（任意类型）：**不继续缓冲**，立即返回错误并由宿主断连记错；
//! - `artifact_ref` 引用帧：本身必须 **<1MiB**（D6）；≥1MiB 仍在 2MiB 内声称
//!   `artifact_ref` 的帧视为契约违约，按断连处理（数据体不得进入线协议）。
//!
//! 探测器只读取已缓冲前缀：判别键 `"type":"artifact_ref"` 必须出现在前缀内，
//! 否则按非引用类消息处理（不阻塞正常大行）。误报/漏报边界见
//! [`probe_artifact_ref`] 文档与 `docs/M1-09-证据.md` §2.3。

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::BytesMut;
use thiserror::Error;
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::codec::{Decoder, Encoder};

/// D6 单帧硬上限：2MiB（超限断连并记错，ADR-004）。
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
/// `artifact_ref` 引用帧上限：必须 <1MiB（D6；≥1MiB 且 ≤2MiB 声称引用 → 契约违约断连）。
pub const ARTIFACT_REF_LIMIT: usize = 1024 * 1024;
/// 单次读取块上限：保证「>2MiB 行不继续缓冲」的硬边界
/// （缓冲上限 = 帧上限 + 本值；不依赖操作系统管道一次可读多少）。
///
/// 反事实（常量级决策，证据 §2.2）：不限流时 `FramedRead` 的读缓冲随 `reserve`
/// 翻倍，Windows 匿名管道一次 `ReadFile` 可返回整行（实测 >1.5MiB 被一次性读入），
/// 使「超限即停」策略失效；64KiB 同时高于常见 4–64KiB 管道块，无额外往返代价。
pub const READ_CHUNK_BYTES: usize = 64 * 1024;
/// 附件引用行判别值（D6；行顶层 `"type"` 字段）。
pub const ARTIFACT_REF_TYPE: &str = "artifact_ref";

/// 帧层错误（全部由宿主转换为「断连 + 记错」，不 panic）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FrameError {
    /// 底层 IO 错误。
    #[error("帧 IO 错误: {0}")]
    Io(String),
    /// 行不是合法 UTF-8。
    #[error("帧不是合法 UTF-8（valid_up_to={valid_up_to}）")]
    InvalidUtf8 {
        /// 首个非法字节位置。
        valid_up_to: usize,
    },
    /// D6 契约违约：1–2MiB 行声称 `artifact_ref`（引用帧必须 <1MiB）→ 断连。
    #[error(
        "artifact_ref 引用帧越界（{bytes} 字节，契约上限 {limit}）：引用帧必须 <1MiB，断连记错"
    )]
    ArtifactRefContractViolation {
        /// 报错时已缓冲/已解析的字节数。
        bytes: usize,
        /// 引用帧契约上限（1MiB）。
        limit: usize,
    },
    /// D6：单帧超过 2MiB（任意行，含声称 `artifact_ref` 的行）。
    #[error("单帧超过上限 {limit} 字节（D6：2MiB）")]
    LineTooLong {
        /// 上限字节数。
        limit: usize,
    },
    /// 流在未完成行处结束（半行/断流，D6 失败场景表）。
    #[error("流在残行处结束（未完成行 {bytes} 字节，丢弃）")]
    IncompleteLine {
        /// 被丢弃的残行字节数。
        bytes: usize,
    },
    /// 出站帧含换行（防帧注入）或超过上限。
    #[error("出站帧非法: {0}")]
    InvalidOutboundFrame(String),
}

impl From<std::io::Error> for FrameError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// 解码产出：一行文本 + 原始字节数（含 LF）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLine {
    /// 行内容（不含 LF/CR）。
    pub text: String,
    /// 原始字节数（含分隔符）。
    pub bytes: usize,
}

/// 读取块限流器：把每次 `poll_read` 填充的字节数限制在 `chunk` 内。
///
/// `FramedRead` 的读缓冲会随 `reserve` 翻倍增长，而 `poll_read` 一次可能返回
/// 与空闲容量同等规模的数据（平台相关）。若不限流，>1MiB 的非引用大行可能
/// 被一次性读入完整缓冲，绕过评审修订 #6 的「不缓冲完整行」策略。
pub struct ChunkLimitedReader<R> {
    inner: R,
    chunk: usize,
    scratch: Vec<u8>,
}

impl<R> ChunkLimitedReader<R> {
    pub fn new(inner: R, chunk: usize) -> Self {
        Self {
            inner,
            chunk,
            scratch: vec![0u8; chunk],
        }
    }

    pub const fn chunk(&self) -> usize {
        self.chunk
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ChunkLimitedReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let Self {
            inner,
            chunk,
            scratch,
        } = self.get_mut();
        let limit = (*chunk).min(buf.remaining());
        if limit == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut local = ReadBuf::new(&mut scratch[..limit]);
        match Pin::new(inner).poll_read(cx, &mut local) {
            Poll::Ready(Ok(())) => {
                buf.put_slice(local.filled());
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

/// Aether JSON-Lines 帧编解码器（D6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AetherLineCodec {
    max_frame: usize,
    artifact_ref_limit: usize,
}

impl Default for AetherLineCodec {
    fn default() -> Self {
        Self::new(MAX_FRAME_BYTES, ARTIFACT_REF_LIMIT)
    }
}

impl AetherLineCodec {
    /// 自定义上限（测试/基准可用更小阈值）。
    pub const fn new(max_frame: usize, artifact_ref_limit: usize) -> Self {
        Self {
            max_frame,
            artifact_ref_limit,
        }
    }

    pub const fn max_frame(&self) -> usize {
        self.max_frame
    }

    pub const fn artifact_ref_limit(&self) -> usize {
        self.artifact_ref_limit
    }

    /// 完整行（不含 LF）尺寸校验；`content` 不含 LF/CR。
    fn check_complete_line(&self, content: &[u8]) -> Result<(), FrameError> {
        let len = content.len();
        if len > self.max_frame {
            return Err(FrameError::LineTooLong {
                limit: self.max_frame,
            });
        }
        if len >= self.artifact_ref_limit && probe_artifact_ref(content) {
            return Err(FrameError::ArtifactRefContractViolation {
                bytes: len,
                limit: self.artifact_ref_limit,
            });
        }
        Ok(())
    }

    /// 未找到 LF 时的前缀尺寸校验（渐进读取路径：超过帧上限即报错，不再继续缓冲）。
    fn check_partial_line(&self, buffered: &[u8]) -> Result<(), FrameError> {
        let len = buffered.len();
        if len > self.max_frame {
            return Err(FrameError::LineTooLong {
                limit: self.max_frame,
            });
        }
        if len >= self.artifact_ref_limit && probe_artifact_ref(buffered) {
            return Err(FrameError::ArtifactRefContractViolation {
                bytes: len,
                limit: self.artifact_ref_limit,
            });
        }
        Ok(())
    }
}

fn find_lf(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|byte| *byte == b'\n')
}

impl Decoder for AetherLineCodec {
    type Item = RawLine;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<RawLine>, Self::Error> {
        loop {
            match find_lf(src) {
                Some(position) => {
                    let line = src.split_to(position + 1);
                    let bytes = line.len();
                    let mut content = &line[..position];
                    if content.last() == Some(&b'\r') {
                        content = &content[..content.len() - 1];
                    }
                    if content.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    self.check_complete_line(content)?;
                    let text = std::str::from_utf8(content)
                        .map_err(|error| FrameError::InvalidUtf8 {
                            valid_up_to: error.valid_up_to(),
                        })?
                        .to_owned();
                    return Ok(Some(RawLine { text, bytes }));
                }
                None => {
                    self.check_partial_line(src)?;
                    return Ok(None);
                }
            }
        }
    }

    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<RawLine>, Self::Error> {
        match self.decode(src) {
            Ok(None) => {
                let bytes = src.len();
                src.clear();
                if bytes == 0 {
                    Ok(None)
                } else {
                    Err(FrameError::IncompleteLine { bytes })
                }
            }
            other => other,
        }
    }
}

impl Encoder<String> for AetherLineCodec {
    type Error = FrameError;

    fn encode(&mut self, item: String, dst: &mut BytesMut) -> Result<(), Self::Error> {
        if item.contains('\n') || item.contains('\r') {
            return Err(FrameError::InvalidOutboundFrame(
                "帧内容不允许包含 CR/LF（防帧注入）".to_owned(),
            ));
        }
        if item.len() > self.max_frame {
            return Err(FrameError::InvalidOutboundFrame(format!(
                "帧超过上限 {} 字节",
                self.max_frame
            )));
        }
        dst.reserve(item.len() + 1);
        dst.extend_from_slice(item.as_bytes());
        dst.extend_from_slice(b"\n");
        Ok(())
    }
}

/// 评审修订 #6 探测器：行前缀的**顶层** `"type"` 字段是否等于 `"artifact_ref"`。
///
/// - 只看顶层对象（嵌套对象的同名键不命中）；
/// - 字符串整体消费，字符串内的 `"type":"artifact_ref"` 不命中；
/// - 前缀不完整（键/值尚未出现）→ `false`（按非引用类处置，防缓冲膨胀）。
///
/// **边界（常量级决策 ③，证据 §2.3）**：
/// - 漏报：判别键出现在 >2MiB 位置的行会在被探测到之前先以 `LineTooLong` 断连
///   （上限内则每轮增量探测，最终可命中，见 `artifact_probe_detects_discriminator_late_in_line`）；
/// - 误报：普通 1–2MiB 行若在顶层携带 `"type":"artifact_ref"` 会被判契约违约并断连
///   （接受该误报——内存上界仍为 2MiB，且合法引用帧按契约远小于 1MiB；
///   见 `artifact_ref_claim_false_positive_is_memory_bounded`）；
/// - 嵌套/字符串形态不误报（`artifact_probe_only_matches_top_level_type`）。
pub fn probe_artifact_ref(prefix: &[u8]) -> bool {
    let mut index = 0usize;
    let mut depth: i32 = 0;
    while index < prefix.len() {
        match prefix[index] {
            b'"' => {
                let Some((token, next)) = read_string(prefix, index) else {
                    return false;
                };
                index = next;
                // 仅当字符串后跟 `:` 才是键（值字符串不参与判别）。
                let mut probe = index;
                while probe < prefix.len() && prefix[probe].is_ascii_whitespace() {
                    probe += 1;
                }
                let is_key = prefix.get(probe) == Some(&b':');
                if depth == 1 && is_key && token == b"\"type\"" {
                    return match read_string_value(prefix, index) {
                        Some(value) => value == ARTIFACT_REF_TYPE,
                        None => false,
                    };
                }
            }
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    false
}

/// 从 `start`（`"` 位置）读取一个 JSON 字符串 token（含引号），返回 token 与下一位置。
fn read_string(buf: &[u8], start: usize) -> Option<(Vec<u8>, usize)> {
    if buf.get(start) != Some(&b'"') {
        return None;
    }
    let mut index = start + 1;
    let mut escaped = false;
    while index < buf.len() {
        let byte = buf[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        match byte {
            b'\\' => {
                escaped = true;
                index += 1;
            }
            b'"' => {
                return Some((buf[start..=index].to_vec(), index + 1));
            }
            _ => index += 1,
        }
    }
    None
}

/// 读取 `"type": "value"` 的值字符串（跳过空白与冒号），返回裸值。
fn read_string_value(buf: &[u8], start: usize) -> Option<String> {
    let mut index = start;
    while index < buf.len() && buf[index].is_ascii_whitespace() {
        index += 1;
    }
    if buf.get(index) != Some(&b':') {
        return None;
    }
    index += 1;
    while index < buf.len() && buf[index].is_ascii_whitespace() {
        index += 1;
    }
    let (token, _) = read_string(buf, index)?;
    let inner = token.get(1..token.len().checked_sub(1)?)?;
    std::str::from_utf8(inner).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(codec: &mut AetherLineCodec, bytes: &[u8]) -> Vec<Result<RawLine, FrameError>> {
        let mut buffer = BytesMut::from(bytes);
        let mut out = Vec::new();
        loop {
            match codec.decode(&mut buffer) {
                Ok(Some(line)) => out.push(Ok(line)),
                Ok(None) => break,
                Err(error) => {
                    out.push(Err(error));
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn decodes_lf_frames_and_tolerates_crlf_and_blank_lines() {
        let mut codec = AetherLineCodec::default();
        let lines = decode_all(&mut codec, b"{\"a\":1}\r\n\n   \n{\"b\":2}\n{\"c\":3}");
        let texts: Vec<&str> = lines
            .iter()
            .map(|line| line.as_ref().map(|raw| raw.text.as_str()).unwrap_or(""))
            .collect();
        // 仅完整行产出；末尾未终止行留给 decode_eof（半行/断流处置）。
        assert_eq!(texts, ["{\"a\":1}", "{\"b\":2}"]);
        assert_eq!(lines[0].as_ref().map(|raw| raw.bytes), Ok(9));
        let mut buffer = BytesMut::from(&b"{\"c\":3}"[..]);
        assert_eq!(
            codec.decode_eof(&mut buffer),
            Err(FrameError::IncompleteLine { bytes: 7 })
        );
    }

    #[test]
    fn incomplete_trailing_line_is_reported_at_eof() {
        let mut codec = AetherLineCodec::default();
        let mut buffer = BytesMut::from(&b"{\"complete\":true}\n{\"half\""[..]);
        let first = codec.decode(&mut buffer).unwrap().unwrap();
        assert_eq!(first.text, "{\"complete\":true}");
        assert_eq!(
            codec.decode_eof(&mut buffer),
            Err(FrameError::IncompleteLine { bytes: 7 })
        );
        assert!(buffer.is_empty(), "残行必须被丢弃（D6 半行/断流）");
    }

    #[test]
    fn clean_eof_after_complete_line_is_ok() {
        let mut codec = AetherLineCodec::default();
        let mut buffer = BytesMut::from(&b"{}\n"[..]);
        assert!(codec.decode(&mut buffer).unwrap().is_some());
        assert_eq!(codec.decode_eof(&mut buffer), Ok(None));
    }

    #[test]
    fn invalid_utf8_is_a_frame_error() {
        let mut codec = AetherLineCodec::default();
        let lines = decode_all(&mut codec, b"{\"x\":\"\xff\"}\n");
        assert!(matches!(
            lines.first(),
            Some(Err(FrameError::InvalidUtf8 { .. }))
        ));
    }

    #[test]
    fn line_over_2mib_disconnects() {
        let mut codec = AetherLineCodec::new(1024, 512);
        let big = format!("{{\"pad\":\"{}\"}}\n", "x".repeat(2048));
        let lines = decode_all(&mut codec, big.as_bytes());
        assert!(matches!(
            lines.first(),
            Some(Err(FrameError::LineTooLong { limit: 1024 }))
        ));
    }

    #[test]
    fn non_artifact_line_between_1_and_2_mib_parses_normally() {
        // DoD3：1–2MiB 非引用行正常解析（不得再按「大行不缓冲」拒绝）。
        let artifact_limit = 32 * 1024;
        let max_frame = 64 * 1024;
        let mut codec = AetherLineCodec::new(max_frame, artifact_limit);
        let padding = "y".repeat(48 * 1024);
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"log\",\"params\":{{\"pad\":\"{padding}\"}}}}\n"
        );
        assert!(line.len() > artifact_limit && line.len() < max_frame);
        let lines = decode_all(&mut codec, line.as_bytes());
        assert_eq!(
            lines.len(),
            1,
            "1–2MiB（此处按比例缩小）非引用行必须正常解析"
        );
        let raw = lines[0].as_ref().unwrap();
        assert!(raw.bytes > artifact_limit);
        assert!(raw.text.contains("\"method\":\"log\""));
    }

    #[test]
    fn oversized_non_artifact_line_is_not_fully_buffered() {
        // 超过 2MiB 的任意行：不继续缓冲、立即报错（缓冲上限 = 帧上限 + 单次读取块）。
        let max_frame = 16 * 1024;
        let mut codec = AetherLineCodec::new(max_frame, 8 * 1024);
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":{{\"payload\":\"{}\"}}}}\n",
            "y".repeat(64 * 1024)
        );
        let bytes = line.as_bytes();
        let mut buffer = BytesMut::new();
        let mut error = None;
        let mut fed = 0usize;
        while fed < bytes.len() && error.is_none() {
            let end = (fed + 512).min(bytes.len());
            buffer.extend_from_slice(&bytes[fed..end]);
            fed = end;
            match codec.decode(&mut buffer) {
                Ok(Some(_)) => panic!("不得产出超过 2MiB 的完整行"),
                Ok(None) => {}
                Err(err) => error = Some(err),
            }
        }
        let error = error.expect("必须在超过帧上限后立刻报错");
        match error {
            FrameError::LineTooLong { limit } => assert_eq!(limit, max_frame),
            other => panic!("错误类型不符: {other:?}"),
        }
        assert!(
            buffer.len() <= max_frame + 512,
            "已缓冲 {} 必须受帧上限约束（不继续缓冲）",
            buffer.len()
        );
        assert!(
            buffer.len() < bytes.len(),
            "不得缓冲完整行（{} 字节）",
            bytes.len()
        );
    }

    #[test]
    fn artifact_ref_under_1mib_is_parsed() {
        let artifact_limit = 16 * 1024;
        let mut codec = AetherLineCodec::new(1024 * 1024, artifact_limit);
        let padding = "z".repeat(8 * 1024);
        let line = format!(
            "{{\"type\":\"artifact_ref\",\"refs\":[{{\"path\":\"a.txt\",\"size\":1}}],\"pad\":\"{padding}\"}}"
        );
        assert!(
            line.len() < artifact_limit,
            "引用帧必须 <1MiB（按比例缩小）"
        );
        let lines = decode_all(&mut codec, format!("{line}\n").as_bytes());
        assert_eq!(lines.len(), 1);
        let value: serde_json::Value =
            serde_json::from_str(&lines[0].as_ref().unwrap().text).unwrap();
        assert_eq!(value["type"], ARTIFACT_REF_TYPE);
    }

    #[test]
    fn artifact_ref_at_or_over_1mib_is_contract_violation() {
        // DoD3：1–2MiB 声称 artifact_ref → 契约违约 → 断连记错。
        let artifact_limit = 16 * 1024;
        let mut codec = AetherLineCodec::new(1024 * 1024, artifact_limit);
        let padding = "z".repeat(32 * 1024);
        let line = format!("{{\"type\":\"artifact_ref\",\"refs\":[],\"pad\":\"{padding}\"}}\n");
        assert!(line.len() > artifact_limit);
        let lines = decode_all(&mut codec, line.as_bytes());
        assert!(matches!(
            lines.first(),
            Some(Err(FrameError::ArtifactRefContractViolation { limit, .. })) if *limit == artifact_limit
        ));
    }

    #[test]
    fn artifact_probe_only_matches_top_level_type() {
        assert!(probe_artifact_ref(b"{\"type\":\"artifact_ref\","));
        assert!(probe_artifact_ref(
            b"{\"jsonrpc\":\"2.0\", \"type\" : \"artifact_ref\" }"
        ));
        assert!(!probe_artifact_ref(b"{\"type\":\"event\","));
        assert!(!probe_artifact_ref(
            b"{\"params\":{\"type\":\"artifact_ref\"},"
        ));
        assert!(!probe_artifact_ref(
            b"{\"note\":\"\\\"type\\\":\\\"artifact_ref\\\"\","
        ));
        assert!(!probe_artifact_ref(
            b"{\"note\":\"type\",\"value\":\"artifact_ref\"}"
        ));
        assert!(!probe_artifact_ref(b"{\"type\":"));
        assert!(!probe_artifact_ref(b"[\"type\",\"artifact_ref\"]"));
        assert!(probe_artifact_ref(
            b"{\"other\":123,\"nested\":{\"a\":[1,2]},\"type\":\"artifact_ref\"}"
        ));
    }

    #[test]
    fn custom_reader_matches_lines_codec_for_normal_frames() {
        use tokio_util::codec::{Decoder as _, LinesCodec};

        // 常量级决策 ①（证据 §2.1）：≤上限帧的行语义必须与 LinesCodec 一致。
        let payload = b"{\"a\":1}\r\n{\"b\":2}\n{\"c\":3}\n";
        let mut custom = AetherLineCodec::default();
        let mut stock = LinesCodec::new();
        let mut custom_buffer = BytesMut::from(&payload[..]);
        let mut stock_buffer = BytesMut::from(&payload[..]);

        let mut custom_lines: Vec<String> = Vec::new();
        while let Some(line) = custom.decode(&mut custom_buffer).unwrap() {
            custom_lines.push(line.text);
        }
        let mut stock_lines: Vec<String> = Vec::new();
        while let Some(line) = stock.decode(&mut stock_buffer).unwrap() {
            stock_lines.push(line);
        }
        assert_eq!(
            custom_lines, stock_lines,
            "行语义（LF/CRLF/UTF-8）必须与 LinesCodec 一致"
        );
    }

    #[test]
    fn stock_lines_codec_would_buffer_line_that_custom_reader_rejects() {
        use tokio_util::codec::{Decoder as _, LinesCodec};

        // 反事实（常量级决策 ①/②，证据 §2.1–2.2）：stock LinesCodec 会完整缓冲并产出
        // >2MiB 行；自定义读取器 + 读块限流在帧上限处立即报错，缓冲上界受控。
        let line = format!("{{\"pad\":\"{}\"}}", "x".repeat(2 * 1024 * 1024 + 4096));
        let mut stock = LinesCodec::new();
        let mut stock_buffer = BytesMut::from(format!("{line}\n").as_bytes());
        assert!(
            stock.decode(&mut stock_buffer).unwrap().is_some(),
            "反事实：stock LinesCodec 会完整缓冲并产出 >2MiB 行"
        );

        let mut custom = AetherLineCodec::default();
        let bytes = format!("{line}\n").into_bytes();
        let mut buffer = BytesMut::new();
        let mut fed = 0usize;
        let mut error = None;
        while fed < bytes.len() && error.is_none() {
            let end = (fed + READ_CHUNK_BYTES).min(bytes.len());
            buffer.extend_from_slice(&bytes[fed..end]);
            fed = end;
            match custom.decode(&mut buffer) {
                Ok(None) => {}
                Ok(Some(_)) => panic!("不得产出 >2MiB 完整行"),
                Err(err) => error = Some(err),
            }
        }
        let error = error.expect("自定义读取器必须立即报错");
        assert!(matches!(
            error,
            FrameError::LineTooLong { limit } if limit == MAX_FRAME_BYTES
        ));
        assert!(
            buffer.len() <= MAX_FRAME_BYTES + READ_CHUNK_BYTES,
            "缓冲上界必须为 帧上限 + READ_CHUNK_BYTES（实际 {}）",
            buffer.len()
        );
        assert!(buffer.len() < bytes.len(), "不得缓冲完整超限行");
    }

    #[test]
    fn artifact_probe_detects_discriminator_late_in_line() {
        // 边界 ③ 漏报侧（证据 §2.3）：判别键位于 1MiB 之后但仍在帧上限内时，
        // 增量探测在读到键的当轮命中，不得放行。
        let artifact_limit = 256 * 1024;
        let max_frame = 1024 * 1024;
        let mut codec = AetherLineCodec::new(max_frame, artifact_limit);
        let padding = "p".repeat(512 * 1024);
        let line = format!("{{\"pad\":\"{padding}\",\"type\":\"artifact_ref\"}}\n");
        assert!(line.len() > artifact_limit && line.len() < max_frame);

        let bytes = line.as_bytes();
        let mut buffer = BytesMut::new();
        let mut error = None;
        for chunk in bytes.chunks(64 * 1024) {
            buffer.extend_from_slice(chunk);
            if let Err(err) = codec.decode(&mut buffer) {
                error = Some(err);
                break;
            }
        }
        assert!(
            matches!(error, Some(FrameError::ArtifactRefContractViolation { .. })),
            "晚出现的判别键必须被增量探测命中: {error:?}"
        );
    }

    #[test]
    fn artifact_ref_claim_false_positive_is_memory_bounded() {
        // 边界 ③ 误报侧（接受，证据 §2.3）：普通 1–2MiB 行若在顶层携带
        // `"type":"artifact_ref"`，按契约违约断连；内存上界仍为 帧上限 + 读取块。
        let max_frame = 1024 * 1024;
        let artifact_limit = 256 * 1024;
        let mut codec = AetherLineCodec::new(max_frame, artifact_limit);
        let padding = "q".repeat(512 * 1024);
        let line = format!("{{\"type\":\"artifact_ref\",\"pad\":\"{padding}\"}}\n");
        let bytes = line.as_bytes();
        let mut buffer = BytesMut::new();
        let mut fed = 0usize;
        let mut error = None;
        while fed < bytes.len() && error.is_none() {
            let end = (fed + 64 * 1024).min(bytes.len());
            buffer.extend_from_slice(&bytes[fed..end]);
            fed = end;
            if let Err(err) = codec.decode(&mut buffer) {
                error = Some(err);
            }
        }
        assert!(matches!(
            error,
            Some(FrameError::ArtifactRefContractViolation { .. })
        ));
        assert!(
            buffer.len() <= max_frame + 64 * 1024,
            "误报路径同样受内存上界约束（实际 {}）",
            buffer.len()
        );
    }

    #[test]
    fn encoder_appends_lf_and_rejects_injection() {
        let mut codec = AetherLineCodec::default();
        let mut buffer = BytesMut::new();
        codec.encode("{\"a\":1}".to_owned(), &mut buffer).unwrap();
        assert_eq!(&buffer[..], b"{\"a\":1}\n");
        let mut buffer = BytesMut::new();
        assert!(matches!(
            codec.encode("bad\nnext".to_owned(), &mut buffer),
            Err(FrameError::InvalidOutboundFrame(_))
        ));
        assert!(matches!(
            codec.encode("bad\rnext".to_owned(), &mut buffer),
            Err(FrameError::InvalidOutboundFrame(_))
        ));
    }

    #[test]
    fn default_limits_match_d6_constants() {
        let codec = AetherLineCodec::default();
        assert_eq!(codec.max_frame(), 2 * 1024 * 1024);
        assert_eq!(codec.artifact_ref_limit(), 1024 * 1024);
        assert_eq!(MAX_FRAME_BYTES, 2 * 1024 * 1024);
        assert_eq!(ARTIFACT_REF_LIMIT, 1024 * 1024);
        assert_eq!(ARTIFACT_REF_TYPE, "artifact_ref");
        assert_eq!(
            READ_CHUNK_BYTES,
            64 * 1024,
            "读块上限为常量级决策（证据 §2.2）：缓冲上界 = 帧上限 + 本值"
        );
    }

    #[test]
    fn io_error_conversion() {
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "closed");
        let frame: FrameError = error.into();
        assert!(matches!(frame, FrameError::Io(_)));
    }

    #[tokio::test]
    async fn chunk_limited_reader_caps_each_read_and_preserves_eof() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut writer, reader) = tokio::io::duplex(1024 * 1024);
        let sender = tokio::spawn(async move {
            writer.write_all(&vec![b'x'; 16 * 1024]).await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let mut limited = ChunkLimitedReader::new(reader, 4096);
        let mut buffer = vec![0u8; 128 * 1024];

        let first = limited.read(&mut buffer).await.unwrap();
        assert_eq!(first, 4096, "单次读取必须受 chunk 限制");
        let second = limited.read(&mut buffer).await.unwrap();
        assert_eq!(second, 4096);
        assert!(limited.chunk == 4096);
        let inner = limited.into_inner();
        drop(inner);
        sender.await.unwrap();
    }

    #[tokio::test]
    async fn chunk_limited_reader_returns_eof_after_data() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"hello").await.unwrap();
        writer.shutdown().await.unwrap();
        let mut limited = ChunkLimitedReader::new(reader, 4096);
        let mut buffer = [0u8; 32];
        assert_eq!(limited.read(&mut buffer).await.unwrap(), 5);
        assert_eq!(limited.read(&mut buffer).await.unwrap(), 0);
    }
}
