use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SseFrameTooLarge {
    pub(crate) limit: u64,
}

#[derive(Debug)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    start: usize,
    max_frame_bytes: u64,
}

impl SseDecoder {
    pub(crate) fn new(max_frame_bytes: u64) -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            max_frame_bytes,
        }
    }

    /// Feed transport bytes into the decoder without ever retaining an
    /// incomplete logical frame larger than the configured budget.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), SseFrameTooLarge> {
        self.compact_consumed();
        let pending = &self.buffer[self.start..];
        let tail_start = last_frame_end(pending).unwrap_or(0);
        validate_frame_budget(
            pending[tail_start..]
                .iter()
                .copied()
                .chain(chunk.iter().copied()),
            self.max_frame_bytes,
        )?;
        self.buffer.extend_from_slice(chunk);
        Ok(())
    }

    pub(crate) fn next_frame(&mut self) -> Option<String> {
        let search = &self.buffer[self.start..];
        let (relative_end, delimiter_len) = find_frame_end(search)?;
        let frame_start = self.start;
        let frame_end = frame_start + relative_end;
        self.start = frame_end + delimiter_len;
        let frame = String::from_utf8_lossy(&self.buffer[frame_start..frame_end]).into_owned();
        self.compact_consumed();
        Some(frame)
    }

    pub(crate) fn finish(&mut self) -> Result<Option<String>, SseFrameTooLarge> {
        let remaining = &self.buffer[self.start..];
        let remaining_len = u64::try_from(remaining.len()).map_err(|_| SseFrameTooLarge {
            limit: self.max_frame_bytes,
        })?;
        if remaining_len > self.max_frame_bytes {
            self.buffer.clear();
            self.start = 0;
            return Err(SseFrameTooLarge {
                limit: self.max_frame_bytes,
            });
        }
        if remaining.iter().all(|byte| matches!(byte, b'\r' | b'\n')) {
            self.buffer.clear();
            self.start = 0;
            return Ok(None);
        }

        let frame = String::from_utf8_lossy(remaining).into_owned();
        self.buffer.clear();
        self.start = 0;
        Ok(Some(frame))
    }

    fn compact_consumed(&mut self) {
        if self.start == 0 {
            return;
        }
        if self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
            return;
        }
        if self.start >= self.buffer.len() / 2 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
    }
}

fn last_frame_end(buffer: &[u8]) -> Option<usize> {
    let mut offset = 0;
    let mut last_end = None;
    while let Some((relative_end, delimiter_len)) = find_frame_end(&buffer[offset..]) {
        offset += relative_end + delimiter_len;
        last_end = Some(offset);
    }
    last_end
}

fn validate_frame_budget(
    bytes: impl Iterator<Item = u8>,
    max_frame_bytes: u64,
) -> Result<(), SseFrameTooLarge> {
    let mut frame_len = 0_u64;
    let mut previous = [0_u8; 4];
    let mut seen = 0_usize;
    for byte in bytes {
        frame_len = frame_len.checked_add(1).ok_or(SseFrameTooLarge {
            limit: max_frame_bytes,
        })?;
        previous.rotate_left(1);
        previous[3] = byte;
        seen = seen.saturating_add(1);
        let delimiter_len = if seen >= 2 && previous[2..] == *b"\n\n" {
            2
        } else if seen >= 4 && previous == *b"\r\n\r\n" {
            4
        } else {
            0
        };
        if delimiter_len > 0 {
            let payload_len = frame_len.saturating_sub(delimiter_len);
            if payload_len > max_frame_bytes {
                return Err(SseFrameTooLarge {
                    limit: max_frame_bytes,
                });
            }
            frame_len = 0;
            previous = [0; 4];
            seen = 0;
        } else if frame_len > max_frame_bytes.saturating_add(3) {
            return Err(SseFrameTooLarge {
                limit: max_frame_bytes,
            });
        }
    }
    let suffix_len = if seen >= 3 && previous[1..] == *b"\r\n\r" {
        3
    } else if seen >= 2 && previous[2..] == *b"\r\n" {
        2
    } else if seen >= 1 && matches!(previous[3], b'\r' | b'\n') {
        1
    } else {
        0
    };
    if frame_len.saturating_sub(suffix_len) > max_frame_bytes {
        return Err(SseFrameTooLarge {
            limit: max_frame_bytes,
        });
    }
    Ok(())
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

pub(crate) fn is_sse_done(text: &str) -> bool {
    parse_sse_frame(text)
        .map(|frame| frame.data.trim() == "[DONE]")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: u64 = 1024;

    #[test]
    fn decoder_returns_complete_frames_and_keeps_partial_tail() {
        let mut decoder = SseDecoder::new(LIMIT);
        decoder.push(b"data: {\"a\":1}\n\n").unwrap();
        decoder
            .push(b"event: message_delta\ndata: {\"b\":")
            .unwrap();

        assert_eq!(decoder.next_frame().unwrap(), "data: {\"a\":1}");
        assert!(decoder.next_frame().is_none());

        decoder.push(b"2}\n\n").unwrap();
        assert_eq!(
            decoder.next_frame().unwrap(),
            "event: message_delta\ndata: {\"b\":2}"
        );
        assert!(decoder.finish().unwrap().is_none());
    }

    #[test]
    fn decoder_supports_crlf_frame_delimiters() {
        let mut decoder = SseDecoder::new(LIMIT);
        decoder.push(b"data: {\"a\":1}\r\n\r\n").unwrap();

        assert_eq!(decoder.next_frame().unwrap(), "data: {\"a\":1}");
    }

    #[test]
    fn decoder_supports_crlf_delimiter_split_across_chunks() {
        let mut decoder = SseDecoder::new(LIMIT);
        decoder.push(b"data: {\"a\":1}\r\n").unwrap();
        decoder.push(b"\r\n").unwrap();

        assert_eq!(decoder.next_frame().unwrap(), "data: {\"a\":1}");
        assert!(decoder.finish().unwrap().is_none());
    }

    #[test]
    fn decoder_returns_adjacent_frames_with_mixed_delimiters() {
        let mut decoder = SseDecoder::new(LIMIT);
        decoder
            .push(b"data: one\n\nevent: two\r\ndata: three\r\n\r\n\n\n")
            .unwrap();

        assert_eq!(decoder.next_frame().unwrap(), "data: one");
        assert_eq!(decoder.next_frame().unwrap(), "event: two\r\ndata: three");
        assert_eq!(decoder.next_frame().unwrap(), "");
        assert!(decoder.finish().unwrap().is_none());
    }

    #[test]
    fn decoder_finish_returns_partial_tail_after_consumed_frames() {
        let mut decoder = SseDecoder::new(LIMIT);
        decoder.push(b"data: one\n\ndata: partial").unwrap();

        assert_eq!(decoder.next_frame().unwrap(), "data: one");
        assert_eq!(decoder.finish().unwrap().unwrap(), "data: partial");
    }

    #[test]
    fn decoder_rejects_incomplete_frame_before_copying_over_limit() {
        let mut decoder = SseDecoder::new(8);
        decoder.push(b"12345678").unwrap();
        assert_eq!(decoder.push(b"9"), Err(SseFrameTooLarge { limit: 8 }));
        assert_eq!(decoder.finish().unwrap().unwrap(), "12345678");
    }

    #[test]
    fn decoder_finish_does_not_treat_partial_delimiter_as_free_payload() {
        let mut decoder = SseDecoder::new(8);
        decoder.push(b"12345678\r").unwrap();

        assert_eq!(decoder.finish(), Err(SseFrameTooLarge { limit: 8 }));
    }

    #[test]
    fn decoder_resets_budget_after_complete_frame() {
        let mut decoder = SseDecoder::new(8);
        decoder.push(b"12345678\n\n12345678").unwrap();

        assert_eq!(decoder.next_frame().unwrap(), "12345678");
        assert_eq!(decoder.finish().unwrap().unwrap(), "12345678");
    }

    #[test]
    fn decoder_allows_large_chunk_with_multiple_small_frames() {
        let mut decoder = SseDecoder::new(9);
        decoder.push(b"data: 1\n\ndata: 2\n\n").unwrap();
        assert_eq!(decoder.next_frame().unwrap(), "data: 1");
        assert_eq!(decoder.next_frame().unwrap(), "data: 2");
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
