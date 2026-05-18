use serde_json::Value;

#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    pub(crate) fn next_frame(&mut self) -> Option<String> {
        let (end, delimiter_len) = find_frame_end(&self.buffer)?;
        let frame = self.buffer.drain(..end + delimiter_len).collect::<Vec<_>>();
        Some(String::from_utf8_lossy(&frame[..end]).into_owned())
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        if self.buffer.iter().all(|byte| matches!(byte, b'\r' | b'\n')) {
            self.buffer.clear();
            return None;
        }

        let frame = std::mem::take(&mut self.buffer);
        Some(String::from_utf8_lossy(&frame).into_owned())
    }
}

fn find_frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|pos| (pos, 2))
        .into_iter()
        .chain(
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|pos| (pos, 4)),
        )
        .min_by_key(|(pos, _)| *pos)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SseFrame {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

pub(crate) fn parse_sse_frame(text: &str) -> Option<SseFrame> {
    if text.trim().is_empty() {
        return None;
    }

    let mut event = None;
    let mut data_lines = Vec::new();

    for line in text.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim().to_string());
        }
    }

    if event.is_none() && data_lines.is_empty() {
        return None;
    }

    Some(SseFrame {
        event,
        data: data_lines.join("\n"),
    })
}

pub(crate) fn parse_sse_json_value(text: &str) -> Option<Value> {
    let frame = parse_sse_frame(text)?;
    let data = frame.data.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_returns_complete_frames_and_keeps_partial_tail() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: {\"a\":1}\n\n");
        decoder.push(b"event: message_delta\ndata: {\"b\":");

        assert_eq!(decoder.next_frame().unwrap(), "data: {\"a\":1}");
        assert!(decoder.next_frame().is_none());

        decoder.push(b"2}\n\n");
        assert_eq!(
            decoder.next_frame().unwrap(),
            "event: message_delta\ndata: {\"b\":2}"
        );
        assert!(decoder.finish().is_none());
    }

    #[test]
    fn decoder_supports_crlf_frame_delimiters() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"data: {\"a\":1}\r\n\r\n");

        assert_eq!(decoder.next_frame().unwrap(), "data: {\"a\":1}");
    }

    #[test]
    fn parse_frame_accepts_data_without_space_and_multiline_data() {
        let frame = parse_sse_frame("event:message\ndata:{\"a\":1}\ndata:{\"b\":2}").unwrap();

        assert_eq!(frame.event.as_deref(), Some("message"));
        assert_eq!(frame.data, "{\"a\":1}\n{\"b\":2}");
    }

    #[test]
    fn parse_json_value_ignores_done_sentinel() {
        assert!(parse_sse_json_value("data: [DONE]").is_none());
    }
}
