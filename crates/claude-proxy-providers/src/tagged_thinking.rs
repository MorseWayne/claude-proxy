#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextSegment {
    Text(String),
    Thinking(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Thinking,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct QuoteState {
    quote: Option<char>,
    escaped: bool,
}

pub(crate) struct TaggedThinkingSplitter {
    mode: Mode,
    quote: QuoteState,
    pending: String,
}

impl Default for TaggedThinkingSplitter {
    fn default() -> Self {
        Self {
            mode: Mode::Text,
            quote: QuoteState::default(),
            pending: String::new(),
        }
    }
}

impl TaggedThinkingSplitter {
    pub(crate) fn push(&mut self, text: &str) -> Vec<TextSegment> {
        self.pending.push_str(text);
        self.drain(false)
    }

    pub(crate) fn finish(&mut self) -> Vec<TextSegment> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Vec<TextSegment> {
        let mut segments = Vec::new();

        loop {
            let tags = match self.mode {
                Mode::Text => OPEN_TAGS,
                Mode::Thinking => CLOSE_TAGS,
            };

            let tag = match self.mode {
                Mode::Text => find_tag_outside_quoted_literals(&self.pending, tags, self.quote),
                Mode::Thinking => find_tag(&self.pending, tags),
            };

            if let Some((start, len)) = tag {
                self.push_segment(&mut segments, start);
                self.pending.drain(..start + len);
                self.mode = match self.mode {
                    Mode::Text => Mode::Thinking,
                    Mode::Thinking => Mode::Text,
                };
                continue;
            }

            if self.mode == Mode::Thinking {
                if finish {
                    self.pending.clear();
                    self.mode = Mode::Text;
                    self.quote = QuoteState::default();
                }
                break;
            }

            let emit_len = if finish {
                self.pending.len()
            } else {
                safe_emit_len(&self.pending, tags)
            };

            if emit_len == 0 {
                break;
            }

            self.push_segment(&mut segments, emit_len);
            self.pending.drain(..emit_len);
        }

        segments
    }

    fn push_segment(&mut self, segments: &mut Vec<TextSegment>, len: usize) {
        if len == 0 {
            return;
        }
        let value = self.pending[..len].to_string();
        match self.mode {
            Mode::Text => {
                self.quote = advance_quote_state(self.quote, &value);
                push_text_segment(segments, value);
            }
            Mode::Thinking => push_thinking_segment(segments, value),
        }
    }
}

pub(crate) fn split_tagged_thinking(text: &str) -> Vec<TextSegment> {
    let mut splitter = TaggedThinkingSplitter::default();
    let mut segments = splitter.push(text);
    segments.extend(splitter.finish());
    segments
}

const OPEN_TAGS: &[&str] = &["[thinking]", "<thinking>"];
const CLOSE_TAGS: &[&str] = &["[/thinking]", "</thinking>"];

fn find_tag(text: &str, tags: &[&str]) -> Option<(usize, usize)> {
    tags.iter()
        .filter_map(|tag| text.find(tag).map(|index| (index, tag.len())))
        .min_by_key(|(index, _)| *index)
}

fn find_tag_outside_quoted_literals(
    text: &str,
    tags: &[&str],
    mut quote: QuoteState,
) -> Option<(usize, usize)> {
    for (index, ch) in text.char_indices() {
        if quote.quote.is_none()
            && let Some(tag) = tags.iter().find(|tag| text[index..].starts_with(**tag))
        {
            return Some((index, tag.len()));
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

fn safe_emit_len(text: &str, tags: &[&str]) -> usize {
    text.len()
        .saturating_sub(pending_tag_prefix_len(text, tags))
}

fn pending_tag_prefix_len(text: &str, tags: &[&str]) -> usize {
    let max_len = tags
        .iter()
        .map(|tag| tag.len().saturating_sub(1))
        .max()
        .unwrap_or(0)
        .min(text.len());

    (1..=max_len)
        .rev()
        .find(|len| {
            let start = text.len() - len;
            text.is_char_boundary(start) && tags.iter().any(|tag| tag.starts_with(&text[start..]))
        })
        .unwrap_or(0)
}

fn push_text_segment(segments: &mut Vec<TextSegment>, value: String) {
    if let Some(TextSegment::Text(existing)) = segments.last_mut() {
        existing.push_str(&value);
    } else {
        segments.push(TextSegment::Text(value));
    }
}

fn push_thinking_segment(segments: &mut Vec<TextSegment>, value: String) {
    if let Some(TextSegment::Thinking(existing)) = segments.last_mut() {
        existing.push_str(&value);
    } else {
        segments.push(TextSegment::Thinking(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_tagged_thinking() {
        assert_eq!(
            split_tagged_thinking("hello [thinking]plan[/thinking] world"),
            vec![
                TextSegment::Text("hello ".to_string()),
                TextSegment::Thinking("plan".to_string()),
                TextSegment::Text(" world".to_string()),
            ]
        );
    }

    #[test]
    fn handles_split_tags_across_chunks() {
        let mut splitter = TaggedThinkingSplitter::default();
        assert_eq!(
            splitter.push("hello [thin"),
            vec![TextSegment::Text("hello ".to_string())]
        );
        assert!(splitter.push("king]plan[/thin").is_empty());
        assert_eq!(
            splitter.push("king] world"),
            vec![
                TextSegment::Thinking("plan".to_string()),
                TextSegment::Text(" world".to_string())
            ]
        );
        assert!(splitter.finish().is_empty());
    }

    #[test]
    fn leaves_unmatched_closing_tag_as_text() {
        assert_eq!(
            split_tagged_thinking("hello [/thinking] world"),
            vec![TextSegment::Text("hello [/thinking] world".to_string())]
        );
    }

    #[test]
    fn drops_unclosed_opening_tag_on_finish() {
        assert_eq!(
            split_tagged_thinking("hello [thinking]plan"),
            vec![TextSegment::Text("hello ".to_string())]
        );
    }

    #[test]
    fn supports_xml_style_tags() {
        assert_eq!(
            split_tagged_thinking("<thinking>plan</thinking>answer"),
            vec![
                TextSegment::Thinking("plan".to_string()),
                TextSegment::Text("answer".to_string()),
            ]
        );
    }

    #[test]
    fn preserves_thinking_tags_inside_quoted_code_literals() {
        let summary = r#"```rust
const OPEN_TAGS: &[&str] = &["[thinking]", "<thinking>"];
const CLOSE_TAGS: &[&str] = &["[/thinking]", "</thinking>"];
assert_eq!(split_tagged_thinking("hello [thinking]plan[/thinking] world"), segments);
```"#;

        assert_eq!(
            split_tagged_thinking(summary),
            vec![TextSegment::Text(summary.to_string())]
        );
    }

    #[test]
    fn preserves_split_thinking_tags_inside_quoted_code_literals() {
        let mut splitter = TaggedThinkingSplitter::default();
        let mut segments = Vec::new();

        segments.extend(splitter.push(r#"assert_eq!(split_tagged_thinking("hello "#));
        segments.extend(splitter.push("[thin"));
        segments.extend(splitter.push("king]plan[/thin"));
        segments.extend(splitter.push(r#"king] world"), segments);"#));
        segments.extend(splitter.finish());

        assert!(
            segments
                .iter()
                .all(|segment| matches!(segment, TextSegment::Text(_)))
        );
        let visible = segments
            .into_iter()
            .map(|segment| match segment {
                TextSegment::Text(text) => text,
                TextSegment::Thinking(_) => unreachable!(),
            })
            .collect::<String>();
        assert_eq!(
            visible,
            r#"assert_eq!(split_tagged_thinking("hello [thinking]plan[/thinking] world"), segments);"#
        );
    }

    #[test]
    fn treats_marker_after_apostrophe_as_real_tag() {
        assert_eq!(
            split_tagged_thinking("I'm ready [thinking]plan[/thinking] answer"),
            vec![
                TextSegment::Text("I'm ready ".to_string()),
                TextSegment::Thinking("plan".to_string()),
                TextSegment::Text(" answer".to_string()),
            ]
        );
    }
}
