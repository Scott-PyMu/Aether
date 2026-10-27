//! JSONL 帧编解码（ADR-029 §4.1）：**2MiB 上限的有界读取器**。
//!
//! 口径：
//! - UTF-8，一行一个 JSON 对象；LF 分隔（容忍 CRLF）；
//! - 任一请求/响应/通知帧 **≤2MiB**（[`MAX_FRAME_BYTES`]）；`artifact_ref` 引用帧
//!   另由业务层保证 `<1MiB`；
//! - 读取器逐块消费 `BufRead::fill_buf`，**超限立即停止累积**并丢弃该行剩余字节，
//!   不把整行读入内存（内存上界 = 2MiB + 1 块）；
//! - 超限只失败该请求（调用方返回 `too_large`），连接继续可读（DoD-7）。
//!
//! 空行被跳过（JSONL 生产方不应发送；跳过可避免尾部换行误判）。

use std::io::BufRead;

use serde_json::Value;

/// 单帧上限：2MiB（ADR-029 §4.1；D6 线协议同口径）。
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// 帧读取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// 一整行（不含分隔符）。
    Line(Vec<u8>),
    /// 超限帧：已丢弃该行全部字节（`discarded` 为丢弃的字节数）。
    Oversized { limit: usize, discarded: u64 },
}

/// 读取错误（IO 层）。
#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "读取 JSONL 帧失败：{error}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for FrameError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// 有界 JSONL 读取器。
pub struct FrameReader<R: BufRead> {
    inner: R,
    frames: u64,
    discarded_frames: u64,
}

impl<R: BufRead> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            frames: 0,
            discarded_frames: 0,
        }
    }

    /// 已成功读取的帧数（不含超限丢弃）。
    pub fn frames_read(&self) -> u64 {
        self.frames
    }

    /// 已丢弃的超限帧数。
    pub fn oversized_frames(&self) -> u64 {
        self.discarded_frames
    }

    /// 读取下一帧；EOF 返回 `Ok(None)`。
    ///
    /// 超限：丢弃整行（不缓冲），返回 [`Frame::Oversized`]；后续调用继续读下一帧。
    pub fn read_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            let available = self.inner.fill_buf()?;
            if available.is_empty() {
                if buffer.is_empty() {
                    return Ok(None);
                }
                // 末行无换行符：按帧返回（JSON 解析决定合法性）。
                self.frames = self.frames.saturating_add(1);
                strip_cr(&mut buffer);
                return Ok(Some(Frame::Line(std::mem::take(&mut buffer))));
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.unwrap_or(available.len());

            if buffer.len() + take > MAX_FRAME_BYTES {
                let mut discarded = u64::try_from(buffer.len() + take).unwrap_or(u64::MAX);
                let consumed = newline.map_or(available.len(), |index| index + 1);
                self.inner.consume(consumed);
                if newline.is_some() {
                    return Ok(Some(self.record_oversized(discarded)));
                }
                // 该行尚未结束：继续丢弃到换行（逐块，不累积）。
                loop {
                    let available = self.inner.fill_buf()?;
                    if available.is_empty() {
                        return Ok(Some(self.record_oversized(discarded)));
                    }
                    match available.iter().position(|byte| *byte == b'\n') {
                        Some(index) => {
                            discarded = discarded.saturating_add(u64::try_from(index).unwrap_or(0));
                            let consumed = index + 1;
                            self.inner.consume(consumed);
                            return Ok(Some(self.record_oversized(discarded)));
                        }
                        None => {
                            discarded = discarded
                                .saturating_add(u64::try_from(available.len()).unwrap_or(0));
                            let len = available.len();
                            self.inner.consume(len);
                        }
                    }
                }
            }

            buffer.extend_from_slice(&available[..take]);
            match newline {
                Some(index) => {
                    self.inner.consume(index + 1);
                    strip_cr(&mut buffer);
                    if buffer.is_empty() {
                        // 空行：跳过（不产生帧）。
                        continue;
                    }
                    self.frames = self.frames.saturating_add(1);
                    return Ok(Some(Frame::Line(std::mem::take(&mut buffer))));
                }
                None => {
                    let len = available.len();
                    self.inner.consume(len);
                }
            }
        }
    }

    fn record_oversized(&mut self, discarded: u64) -> Frame {
        self.discarded_frames = self.discarded_frames.saturating_add(1);
        Frame::Oversized {
            limit: MAX_FRAME_BYTES,
            discarded,
        }
    }
}

fn strip_cr(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}

/// 编码单行（紧凑 JSON + LF）——stdout 只允许 JSONL（ADR-029 §4.1）。
///
/// `serde_json` 默认转义字符串内的换行，因此编码结果保证单行。
pub fn encode_line(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(input: &[u8]) -> FrameReader<&[u8]> {
        FrameReader::new(input)
    }

    fn line_text(frame: Frame) -> String {
        match frame {
            Frame::Line(bytes) => String::from_utf8(bytes).expect("UTF-8"),
            Frame::Oversized { .. } => panic!("期望普通帧"),
        }
    }

    #[test]
    fn reads_lines_and_tolerates_crlf_and_blank_lines() {
        let mut reader = reader(b"{\"a\":1}\r\n\n{\"b\":2}\n\n");
        assert_eq!(
            line_text(reader.read_frame().unwrap().unwrap()),
            "{\"a\":1}"
        );
        assert_eq!(
            line_text(reader.read_frame().unwrap().unwrap()),
            "{\"b\":2}"
        );
        assert!(reader.read_frame().unwrap().is_none(), "EOF");
        assert_eq!(reader.frames_read(), 2);
        assert_eq!(reader.oversized_frames(), 0);
    }

    #[test]
    fn trailing_line_without_newline_is_still_a_frame() {
        let mut reader = reader(b"{\"a\":1}");
        assert_eq!(
            line_text(reader.read_frame().unwrap().unwrap()),
            "{\"a\":1}"
        );
        assert!(reader.read_frame().unwrap().is_none());
    }

    #[test]
    fn frame_at_exact_limit_is_accepted() {
        // 构造恰好 MAX_FRAME_BYTES 字节的一行（合法 JSON 形状不重要，编解码由上层校验）。
        let mut payload = vec![b'a'; MAX_FRAME_BYTES];
        payload[MAX_FRAME_BYTES - 1] = b'}';
        payload[0] = b'{';
        payload.push(b'\n');
        let mut reader = reader(&payload);
        match reader.read_frame().unwrap().unwrap() {
            Frame::Line(bytes) => assert_eq!(bytes.len(), MAX_FRAME_BYTES),
            Frame::Oversized { .. } => panic!("恰好 2MiB 必须接受"),
        }
    }

    #[test]
    fn oversized_frame_is_rejected_without_buffering_and_next_frame_still_reads() {
        let mut payload = vec![b'a'; MAX_FRAME_BYTES + 1];
        payload[0] = b'{';
        payload[MAX_FRAME_BYTES] = b'}';
        payload.push(b'\n');
        payload.extend_from_slice(b"{\"after\":true}\n");
        let mut reader = reader(&payload);
        match reader.read_frame().unwrap().unwrap() {
            Frame::Oversized { limit, discarded } => {
                assert_eq!(limit, MAX_FRAME_BYTES);
                assert_eq!(discarded, (MAX_FRAME_BYTES + 1) as u64);
            }
            Frame::Line(_) => panic!("超限帧必须拒绝"),
        }
        // 断言重连续读（DoD-7）：后续帧不受影响。
        assert_eq!(
            line_text(reader.read_frame().unwrap().unwrap()),
            "{\"after\":true}"
        );
        assert!(reader.read_frame().unwrap().is_none());
        assert_eq!(reader.oversized_frames(), 1);
        assert_eq!(reader.frames_read(), 1);
    }

    #[test]
    fn oversized_line_spanning_many_blocks_is_discarded_until_newline() {
        // 10MiB 单行：读取器必须在 2MiB 处停止累积（内存有界），并吞到换行。
        let mut payload = Vec::with_capacity(10 * 1024 * 1024 + 2);
        payload.extend_from_slice(&vec![b'x'; 10 * 1024 * 1024]);
        assert!(!payload.contains(&b'\n'), "构造无内嵌换行");
        payload.push(b'\n');
        payload.extend_from_slice(b"{\"ok\":1}\n");
        let cursor = std::io::Cursor::new(payload);
        let mut reader = FrameReader::new(std::io::BufReader::with_capacity(8192, cursor));
        match reader.read_frame().unwrap().unwrap() {
            Frame::Oversized { discarded, .. } => assert_eq!(discarded, 10 * 1024 * 1024),
            Frame::Line(_) => panic!("必须超限"),
        }
        assert_eq!(
            line_text(reader.read_frame().unwrap().unwrap()),
            "{\"ok\":1}"
        );
    }

    #[test]
    fn encode_line_is_single_line_with_trailing_newline() {
        let bytes = encode_line(&serde_json::json!({"text": "a\nb"}));
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(*bytes.last().unwrap(), b'\n');
        let parsed: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("合法 JSON");
        assert_eq!(parsed["text"], serde_json::json!("a\nb"));
    }
}
