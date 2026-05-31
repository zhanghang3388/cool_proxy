//! AWS Event Stream（vnd.amazon.eventstream）二进制分帧解析器。
//!
//! Kiro / CodeWhisperer 的 `generateAssistantResponse` 返回的是 AWS event-stream，
//! 不是 SSE。每一帧的结构：
//!
//! ```text
//! [4B total_len][4B headers_len][4B prelude_crc]
//! [headers ...][payload ...][4B message_crc]
//! ```
//!
//! 我们只关心 header 里的 `:event-type`（字符串值，类型 7）和 payload（JSON）。
//! 解析器是增量的：喂进字节，吐出已完整的帧；不够一帧就留在内部缓冲等下次。

/// 一个解析出来的事件帧。
#[derive(Debug, Clone)]
pub struct EventFrame {
    /// header `:event-type` 的值，例如 "assistantResponseEvent"。可能为空。
    pub event_type: String,
    /// payload 原始字节（通常是一段 JSON）。
    pub payload: Vec<u8>,
}

/// 增量帧解析器。把上游字节流 `feed` 进来，逐帧取出。
#[derive(Default)]
pub struct EventStreamParser {
    buf: Vec<u8>,
}

impl EventStreamParser {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 追加新字节，返回这次能完整解析出的所有帧。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<EventFrame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            // 至少要够 prelude（12B）才能读出总长度
            if self.buf.len() < 12 {
                break;
            }
            let total_len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                as usize;
            // 防御：异常长度直接清空缓冲，避免卡死 / 无限增长
            if total_len < 16 || total_len > 64 * 1024 * 1024 {
                self.buf.clear();
                break;
            }
            if self.buf.len() < total_len {
                break; // 等更多字节
            }
            let headers_len =
                u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;

            let headers_start = 12usize;
            let headers_end = headers_start + headers_len;
            let payload_start = headers_end;
            let payload_end = total_len - 4; // 去掉尾部 message CRC

            if headers_end <= self.buf.len()
                && payload_start <= payload_end
                && payload_end <= self.buf.len()
            {
                let event_type = extract_event_type(&self.buf[headers_start..headers_end]);
                let payload = self.buf[payload_start..payload_end].to_vec();
                out.push(EventFrame {
                    event_type,
                    payload,
                });
            }
            // 不管这一帧是否解析成功，都按 total_len 推进，避免死循环
            self.buf.drain(0..total_len);
        }
        out
    }
}

/// 从 header 区段里扫出 `:event-type` 的字符串值。
///
/// header 编码：`[1B name_len][name][1B value_type][value...]`
/// value_type == 7 表示 string：`[2B value_len][value]`。其余类型按其固定/变长长度跳过。
fn extract_event_type(headers: &[u8]) -> String {
    let mut off = 0usize;
    while off < headers.len() {
        let name_len = headers[off] as usize;
        off += 1;
        if off + name_len > headers.len() {
            break;
        }
        let name = &headers[off..off + name_len];
        off += name_len;
        if off >= headers.len() {
            break;
        }
        let value_type = headers[off];
        off += 1;

        match value_type {
            // 7 = string, 6 = byte array：都是 [2B len][bytes]
            6 | 7 => {
                if off + 2 > headers.len() {
                    break;
                }
                let vlen = ((headers[off] as usize) << 8) | headers[off + 1] as usize;
                off += 2;
                if off + vlen > headers.len() {
                    break;
                }
                let value = &headers[off..off + vlen];
                off += vlen;
                if name == b":event-type" {
                    return String::from_utf8_lossy(value).into_owned();
                }
            }
            0 | 1 => {} // bool true/false：无额外字节
            2 => off += 1,  // byte
            3 => off += 2,  // short
            4 => off += 4,  // int
            5 => off += 8,  // long
            8 => off += 8,  // timestamp
            9 => off += 16, // uuid
            _ => break,     // 未知类型，无法安全跳过
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工拼一帧 AWS event-stream，验证分帧 + event-type 提取 + payload 还原。
    fn build_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        // header: name=":event-type"(11B), value_type=7, [2B len][value]
        let name = b":event-type";
        let mut headers = Vec::new();
        headers.push(name.len() as u8);
        headers.extend_from_slice(name);
        headers.push(7u8); // string
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());

        let headers_len = headers.len();
        let total_len = 12 + headers_len + payload.len() + 4;

        let mut frame = Vec::new();
        frame.extend_from_slice(&(total_len as u32).to_be_bytes());
        frame.extend_from_slice(&(headers_len as u32).to_be_bytes());
        frame.extend_from_slice(&0u32.to_be_bytes()); // prelude crc（解析器不校验）
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&0u32.to_be_bytes()); // message crc
        frame
    }

    #[test]
    fn parses_single_frame() {
        let body = br#"{"content":"hello"}"#;
        let frame = build_frame("assistantResponseEvent", body);
        let mut p = EventStreamParser::new();
        let frames = p.feed(&frame);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event_type, "assistantResponseEvent");
        assert_eq!(frames[0].payload, body);
    }

    #[test]
    fn parses_two_frames_in_one_feed() {
        let mut data = build_frame("assistantResponseEvent", br#"{"content":"a"}"#);
        data.extend(build_frame("assistantResponseEvent", br#"{"content":"b"}"#));
        let mut p = EventStreamParser::new();
        let frames = p.feed(&data);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].payload, br#"{"content":"b"}"#);
    }

    #[test]
    fn handles_split_across_feeds() {
        let frame = build_frame("toolUseEvent", br#"{"x":1}"#);
        let (a, b) = frame.split_at(7);
        let mut p = EventStreamParser::new();
        assert!(p.feed(a).is_empty()); // 不够一帧
        let frames = p.feed(b);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event_type, "toolUseEvent");
    }
}
