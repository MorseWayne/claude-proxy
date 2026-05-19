const START_MARKER: &str = "[thinking]";
const END_MARKER: &str = "[/thinking]";

#[derive(Debug, Default)]
pub(crate) struct ThinkingSanitizer {
    hidden: bool,
    buffer: String,
}

impl ThinkingSanitizer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, text: &str) -> String {
        self.buffer.push_str(text);
        let mut visible = String::new();

        loop {
            if self.hidden {
                if let Some(index) = self.buffer.find(END_MARKER) {
                    self.buffer.drain(..index + END_MARKER.len());
                    self.hidden = false;
                } else {
                    let keep = longest_suffix_prefix(&self.buffer, END_MARKER);
                    self.buffer.drain(..self.buffer.len() - keep);
                    break;
                }
            } else if let Some(index) = self.buffer.find(START_MARKER) {
                visible.push_str(&self.buffer[..index]);
                self.buffer.drain(..index + START_MARKER.len());
                self.hidden = true;
            } else {
                let keep = longest_suffix_prefix(&self.buffer, START_MARKER);
                let emit_len = self.buffer.len() - keep;
                visible.push_str(&self.buffer[..emit_len]);
                self.buffer.drain(..emit_len);
                break;
            }
        }

        visible
    }

    pub(crate) fn finish(&mut self) -> String {
        if self.hidden {
            self.hidden = false;
            self.buffer.clear();
            String::new()
        } else {
            std::mem::take(&mut self.buffer)
        }
    }
}

pub(crate) fn sanitize_thinking_markers(text: &str) -> String {
    let mut sanitizer = ThinkingSanitizer::new();
    let mut visible = sanitizer.push(text);
    visible.push_str(&sanitizer.finish());
    visible
}

fn longest_suffix_prefix(text: &str, marker: &str) -> usize {
    (1..marker.len())
        .rev()
        .find(|&len| text.ends_with(&marker[..len]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_complete_thinking_marker() {
        let text = "hello [thinking]\nsecret\n[/thinking] world";

        assert_eq!(sanitize_thinking_markers(text), "hello  world");
    }

    #[test]
    fn removes_split_thinking_marker() {
        let mut sanitizer = ThinkingSanitizer::new();
        let mut visible = String::new();

        visible.push_str(&sanitizer.push("hello [think"));
        visible.push_str(&sanitizer.push("ing]secret"));
        visible.push_str(&sanitizer.push("[/think"));
        visible.push_str(&sanitizer.push("ing] world"));
        visible.push_str(&sanitizer.finish());

        assert_eq!(visible, "hello  world");
    }

    #[test]
    fn drops_unclosed_thinking_marker() {
        assert_eq!(
            sanitize_thinking_markers("visible [thinking]secret"),
            "visible "
        );
    }

    #[test]
    fn flushes_incomplete_start_prefix_when_finished() {
        let mut sanitizer = ThinkingSanitizer::new();
        let mut visible = sanitizer.push("visible [thin");
        visible.push_str(&sanitizer.finish());

        assert_eq!(visible, "visible [thin");
    }
}
