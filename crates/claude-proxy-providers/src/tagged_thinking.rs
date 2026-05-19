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

pub(crate) struct TaggedThinkingSplitter {
    mode: Mode,
    pending: String,
}

impl Default for TaggedThinkingSplitter {
    fn default() -> Self {
        Self {
            mode: Mode::Text,
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

            if let Some((start, len)) = find_tag(&self.pending, tags) {
                self.push_segment(&mut segments, start);
                self.pending.drain(..start + len);
                self.mode = match self.mode {
                    Mode::Text => Mode::Thinking,
                    Mode::Thinking => Mode::Text,
                };
                continue;
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

    fn push_segment(&self, segments: &mut Vec<TextSegment>, len: usize) {
        if len == 0 {
            return;
        }
        let value = self.pending[..len].to_string();
        match self.mode {
            Mode::Text => push_text_segment(segments, value),
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
        assert_eq!(
            splitter.push("king]plan[/thin"),
            vec![TextSegment::Thinking("plan".to_string())]
        );
        assert_eq!(
            splitter.push("king] world"),
            vec![TextSegment::Text(" world".to_string())]
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
    fn treats_unclosed_opening_tag_as_thinking_until_finish() {
        assert_eq!(
            split_tagged_thinking("hello [thinking]plan"),
            vec![
                TextSegment::Text("hello ".to_string()),
                TextSegment::Thinking("plan".to_string()),
            ]
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
}
