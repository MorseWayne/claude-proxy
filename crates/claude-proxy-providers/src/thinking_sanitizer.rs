const START_MARKER: &str = "[thinking]";
const END_MARKER: &str = "[/thinking]";

#[derive(Debug, Default)]
pub(crate) struct ThinkingSanitizer {
    hidden: bool,
    quote: QuoteState,
    buffer: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct QuoteState {
    quote: Option<char>,
    escaped: bool,
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
            } else if let Some(index) =
                find_marker_outside_quoted_literals(&self.buffer, START_MARKER, self.quote)
            {
                visible.push_str(&self.buffer[..index]);
                self.quote = advance_quote_state(self.quote, &self.buffer[..index]);
                self.buffer.drain(..index + START_MARKER.len());
                self.hidden = true;
                self.quote = QuoteState::default();
            } else {
                let keep = longest_suffix_prefix(&self.buffer, START_MARKER);
                let emit_len = self.buffer.len() - keep;
                visible.push_str(&self.buffer[..emit_len]);
                self.quote = advance_quote_state(self.quote, &self.buffer[..emit_len]);
                self.buffer.drain(..emit_len);
                break;
            }
        }

        visible
    }

    pub(crate) fn finish(&mut self) -> String {
        if self.hidden {
            self.hidden = false;
            self.quote = QuoteState::default();
            self.buffer.clear();
            String::new()
        } else {
            self.quote = QuoteState::default();
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

fn find_marker_outside_quoted_literals(
    text: &str,
    marker: &str,
    mut quote: QuoteState,
) -> Option<usize> {
    for (index, ch) in text.char_indices() {
        if quote.quote.is_none() && text[index..].starts_with(marker) {
            return Some(index);
        }
        quote = advance_quote_char(quote, ch);
    }
    None
}

fn advance_quote_state(mut quote: QuoteState, text: &str) -> QuoteState {
    for ch in text.chars() {
        quote = advance_quote_char(quote, ch);
    }
    quote
}

fn advance_quote_char(mut quote: QuoteState, ch: char) -> QuoteState {
    if let Some(active) = quote.quote {
        if quote.escaped {
            quote.escaped = false;
        } else if ch == '\\' {
            quote.escaped = true;
        } else if ch == active || ch == '\n' {
            quote.quote = None;
        }
    } else if ch == '"' {
        quote.quote = Some(ch);
        quote.escaped = false;
    }
    quote
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

    #[test]
    fn preserves_thinking_markers_inside_quoted_code_literals() {
        let mut sanitizer = ThinkingSanitizer::new();
        let mut visible = String::new();

        visible.push_str(&sanitizer.push(r#"assert_eq!(split_tagged_thinking("hello "#));
        visible.push_str(&sanitizer.push("[thinking]plan[/think"));
        visible.push_str(&sanitizer.push(r#"ing] world"), segments);"#));
        visible.push_str(&sanitizer.finish());

        assert_eq!(
            visible,
            r#"assert_eq!(split_tagged_thinking("hello [thinking]plan[/thinking] world"), segments);"#
        );
    }

    #[test]
    fn removes_real_marker_after_apostrophe() {
        assert_eq!(
            sanitize_thinking_markers("I'm ready [thinking]secret[/thinking] answer"),
            "I'm ready  answer"
        );
    }
}
