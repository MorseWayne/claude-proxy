use claude_proxy_config::settings::{REASONING_MARKER_MODE_EXTRA_KEY, ReasoningMarkerMode};
use claude_proxy_core::MessagesRequest;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextSegment {
    Text(String),
    Reasoning(String),
}

pub(crate) fn marker_mode_from_request(request: &MessagesRequest) -> ReasoningMarkerMode {
    request
        .extra
        .get(REASONING_MARKER_MODE_EXTRA_KEY)
        .and_then(Value::as_str)
        .and_then(|value| {
            serde_json::from_value::<ReasoningMarkerMode>(Value::String(value.into())).ok()
        })
        .unwrap_or_default()
}

pub(crate) fn split_text(text: &str, mode: ReasoningMarkerMode) -> Vec<TextSegment> {
    let mut splitter = ReasoningTextSplitter::new(mode);
    let mut segments = splitter.push(text);
    segments.extend(splitter.finish());
    segments
}

pub(crate) struct ReasoningTextSplitter {
    mode: ReasoningMarkerMode,
    legacy: LegacyMarkerSplitter,
}

impl Default for ReasoningTextSplitter {
    fn default() -> Self {
        Self::new(ReasoningMarkerMode::Strict)
    }
}

impl ReasoningTextSplitter {
    pub(crate) fn new(mode: ReasoningMarkerMode) -> Self {
        Self {
            mode,
            legacy: LegacyMarkerSplitter::default(),
        }
    }

    pub(crate) fn push(&mut self, text: &str) -> Vec<TextSegment> {
        match self.mode {
            ReasoningMarkerMode::Strict | ReasoningMarkerMode::Disabled => {
                non_empty_text_segment(text)
            }
            ReasoningMarkerMode::LegacyTags => self.legacy.push(text),
            ReasoningMarkerMode::SanitizeOnly => text_only(self.legacy.push(text)),
        }
    }

    pub(crate) fn finish(&mut self) -> Vec<TextSegment> {
        match self.mode {
            ReasoningMarkerMode::Strict | ReasoningMarkerMode::Disabled => Vec::new(),
            ReasoningMarkerMode::LegacyTags => self.legacy.finish(),
            ReasoningMarkerMode::SanitizeOnly => text_only(self.legacy.finish()),
        }
    }
}

fn non_empty_text_segment(text: &str) -> Vec<TextSegment> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![TextSegment::Text(text.to_string())]
    }
}

fn text_only(segments: Vec<TextSegment>) -> Vec<TextSegment> {
    let mut visible = Vec::new();
    for segment in segments {
        if let TextSegment::Text(text) = segment {
            push_text_segment(&mut visible, text);
        }
    }
    visible
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Mode {
    #[default]
    Text,
    Reasoning {
        close: &'static str,
    },
}

#[derive(Debug, Default)]
struct LegacyMarkerSplitter {
    mode: Mode,
    context: TextContext,
    pending: String,
}

impl LegacyMarkerSplitter {
    fn push(&mut self, text: &str) -> Vec<TextSegment> {
        self.pending.push_str(text);
        self.drain(false)
    }

    fn finish(&mut self) -> Vec<TextSegment> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Vec<TextSegment> {
        let mut segments = Vec::new();

        loop {
            match self.mode {
                Mode::Text => {
                    if let Some(hit) = find_open_marker(&self.pending, self.context) {
                        self.push_segment(&mut segments, hit.start);
                        self.pending.drain(..hit.start + hit.open.len());
                        self.mode = Mode::Reasoning { close: hit.close };
                        self.context = TextContext::default();
                        continue;
                    }

                    let emit_len = if finish {
                        self.pending.len()
                    } else {
                        safe_emit_len(&self.pending, OPEN_TAGS)
                    };
                    if emit_len == 0 {
                        break;
                    }
                    self.push_segment(&mut segments, emit_len);
                    self.pending.drain(..emit_len);
                }
                Mode::Reasoning { close } => {
                    if let Some(index) = self.pending.find(close) {
                        self.push_segment(&mut segments, index);
                        self.pending.drain(..index + close.len());
                        self.mode = Mode::Text;
                        self.context = TextContext::default();
                        continue;
                    }

                    if finish {
                        self.pending.clear();
                        self.mode = Mode::Text;
                        self.context = TextContext::default();
                    }
                    break;
                }
            }
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
                self.context.advance(&value);
                push_text_segment(segments, value);
            }
            Mode::Reasoning { .. } => push_reasoning_segment(segments, value),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MarkerHit {
    start: usize,
    open: &'static str,
    close: &'static str,
}

const MARKERS: &[(&str, &str)] = &[
    ("[thinking]", "[/thinking]"),
    ("<thinking>", "</thinking>"),
    ("[analysis]", "[/analysis]"),
    ("<analysis>", "</analysis>"),
    ("[reasoning]", "[/reasoning]"),
    ("<reasoning>", "</reasoning>"),
];

const OPEN_TAGS: &[&str] = &[
    "[thinking]",
    "<thinking>",
    "[analysis]",
    "<analysis>",
    "[reasoning]",
    "<reasoning>",
];

fn find_open_marker(text: &str, mut context: TextContext) -> Option<MarkerHit> {
    let mut index = 0;
    while index < text.len() {
        if !context.is_protected()
            && let Some((open, close)) = MARKERS
                .iter()
                .find(|(open, _)| text[index..].starts_with(*open))
        {
            return Some(MarkerHit {
                start: index,
                open,
                close,
            });
        }

        let ch = text[index..].chars().next()?;
        if context.quote.is_active() {
            context.advance_char(ch);
            index += ch.len_utf8();
            continue;
        }

        if ch == '`' || ch == '~' {
            let run_len = count_run(&text[index..], ch);
            context.advance_marker_run(ch, run_len);
            index += run_len;
        } else {
            context.advance_char(ch);
            index += ch.len_utf8();
        }
    }
    None
}

fn count_run(text: &str, target: char) -> usize {
    text.chars().take_while(|ch| *ch == target).count()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TextContext {
    quote: QuoteState,
    markdown: MarkdownState,
}

impl TextContext {
    fn is_protected(self) -> bool {
        self.quote.is_active() || self.markdown.in_code()
    }

    fn advance(&mut self, text: &str) {
        let mut index = 0;
        while index < text.len() {
            let ch = text[index..]
                .chars()
                .next()
                .expect("index is always within a char boundary");
            if self.quote.is_active() {
                self.advance_char(ch);
                index += ch.len_utf8();
            } else if ch == '`' || ch == '~' {
                let run_len = count_run(&text[index..], ch);
                self.advance_marker_run(ch, run_len);
                index += run_len;
            } else {
                self.advance_char(ch);
                index += ch.len_utf8();
            }
        }
    }

    fn advance_marker_run(&mut self, ch: char, len: usize) {
        if self.markdown.in_code() || ch == '`' || ch == '~' {
            self.markdown.advance_marker_run(ch, len);
        }
        self.quote = QuoteState::default();
    }

    fn advance_char(&mut self, ch: char) {
        if !self.markdown.in_code() {
            self.quote.advance(ch);
        }
        self.markdown.advance_char(ch);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct QuoteState {
    quote: Option<char>,
    escaped: bool,
}

impl QuoteState {
    fn is_active(self) -> bool {
        self.quote.is_some()
    }

    fn advance(&mut self, ch: char) {
        if let Some(active) = self.quote {
            if self.escaped {
                self.escaped = false;
            } else if ch == '\\' {
                self.escaped = true;
            } else if ch == active || ch == '\n' {
                self.quote = None;
            }
        } else if ch == '"' {
            self.quote = Some(ch);
            self.escaped = false;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MarkdownState {
    inline_backticks: usize,
    fence_char: Option<char>,
    at_line_start: bool,
    line_prefix_spaces: usize,
}

impl Default for MarkdownState {
    fn default() -> Self {
        Self {
            inline_backticks: 0,
            fence_char: None,
            at_line_start: true,
            line_prefix_spaces: 0,
        }
    }
}

impl MarkdownState {
    fn in_code(self) -> bool {
        self.inline_backticks > 0 || self.fence_char.is_some()
    }

    fn advance_marker_run(&mut self, ch: char, len: usize) {
        if let Some(fence) = self.fence_char {
            if ch == fence && len >= 3 && self.is_fence_position() {
                self.fence_char = None;
            }
            self.at_line_start = false;
            return;
        }

        if self.inline_backticks > 0 {
            if ch == '`' && len == self.inline_backticks {
                self.inline_backticks = 0;
            }
            self.at_line_start = false;
            return;
        }

        if (ch == '`' || ch == '~') && len >= 3 && self.is_fence_position() {
            self.fence_char = Some(ch);
        } else if ch == '`' {
            self.inline_backticks = len;
        }
        self.at_line_start = false;
    }

    fn advance_char(&mut self, ch: char) {
        if ch == '\n' {
            self.inline_backticks = 0;
            self.at_line_start = true;
            self.line_prefix_spaces = 0;
        } else if self.at_line_start && ch == ' ' && self.line_prefix_spaces < 4 {
            self.line_prefix_spaces += 1;
        } else {
            self.at_line_start = false;
        }
    }

    fn is_fence_position(self) -> bool {
        self.at_line_start && self.line_prefix_spaces <= 3
    }
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

fn push_reasoning_segment(segments: &mut Vec<TextSegment>, value: String) {
    if let Some(TextSegment::Reasoning(existing)) = segments.last_mut() {
        existing.push_str(&value);
    } else {
        segments.push(TextSegment::Reasoning(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_preserves_marker_text() {
        assert_eq!(
            split_text(
                "hello [thinking]plan[/thinking] world",
                ReasoningMarkerMode::Strict
            ),
            vec![TextSegment::Text(
                "hello [thinking]plan[/thinking] world".to_string()
            )]
        );
    }

    #[test]
    fn legacy_extracts_all_marker_families() {
        assert_eq!(
            split_text(
                "a [thinking]one[/thinking] b <analysis>two</analysis> c [reasoning]three[/reasoning]",
                ReasoningMarkerMode::LegacyTags,
            ),
            vec![
                TextSegment::Text("a ".to_string()),
                TextSegment::Reasoning("one".to_string()),
                TextSegment::Text(" b ".to_string()),
                TextSegment::Reasoning("two".to_string()),
                TextSegment::Text(" c ".to_string()),
                TextSegment::Reasoning("three".to_string()),
            ]
        );
    }

    #[test]
    fn sanitize_only_drops_marker_bodies() {
        assert_eq!(
            split_text(
                "visible [analysis]secret[/analysis] text",
                ReasoningMarkerMode::SanitizeOnly,
            ),
            vec![TextSegment::Text("visible  text".to_string())]
        );
    }

    #[test]
    fn legacy_preserves_markers_inside_inline_code() {
        let text = "use `<thinking>...</thinking>` here";

        assert_eq!(
            split_text(text, ReasoningMarkerMode::LegacyTags),
            vec![TextSegment::Text(text.to_string())]
        );
    }

    #[test]
    fn legacy_preserves_split_markers_inside_inline_code() {
        let mut splitter = ReasoningTextSplitter::new(ReasoningMarkerMode::LegacyTags);
        let mut segments = Vec::new();

        segments.extend(splitter.push("use `<thin"));
        segments.extend(splitter.push("king>...</thin"));
        segments.extend(splitter.push("king>` here"));
        segments.extend(splitter.finish());

        assert_eq!(
            text_from_segments(&segments),
            "use `<thinking>...</thinking>` here"
        );
        assert!(
            segments
                .iter()
                .all(|segment| matches!(segment, TextSegment::Text(_)))
        );
    }

    #[test]
    fn legacy_preserves_markers_inside_fenced_code() {
        let text = "```md\n[thinking]example[/thinking]\n```\nanswer";

        assert_eq!(
            split_text(text, ReasoningMarkerMode::LegacyTags),
            vec![TextSegment::Text(text.to_string())]
        );
    }

    #[test]
    fn legacy_preserves_markers_inside_quoted_literals() {
        let text = r#"assert_eq!("hello [thinking]plan[/thinking]");"#;

        assert_eq!(
            split_text(text, ReasoningMarkerMode::LegacyTags),
            vec![TextSegment::Text(text.to_string())]
        );
    }

    #[test]
    fn legacy_drops_unclosed_marker_on_finish() {
        assert_eq!(
            split_text("visible [thinking]secret", ReasoningMarkerMode::LegacyTags),
            vec![TextSegment::Text("visible ".to_string())]
        );
    }

    fn text_from_segments(segments: &[TextSegment]) -> String {
        segments
            .iter()
            .filter_map(|segment| match segment {
                TextSegment::Text(text) => Some(text.as_str()),
                TextSegment::Reasoning(_) => None,
            })
            .collect()
    }
}
