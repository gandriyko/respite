//! `.http` highlighter on top of `http_syntax::parse`, plugged into the editor's
//! `InputHighlighter` seam (no tree-sitter grammar needed).
// ponytail: full reparse per edit, O(file); incremental when 5k-line files lag.

use std::collections::HashSet;
use std::ops::Range;

use gpui::{Context, HighlightStyle, SharedString, Window};
use gpui_component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope,
};

type Span = (Range<usize>, Option<&'static str>);

#[derive(Default)]
pub struct HttpHighlighter {
    spans: Vec<Span>,
    folds: Vec<FoldRange>,
}

impl InputHighlighter for HttpHighlighter {
    fn language(&self) -> SharedString {
        "http".into()
    }

    fn update(&mut self, _: Option<InputEdit>, text: &Rope, _: bool, _: &mut Window, _: &mut Context<EditorState>) {
        (self.spans, self.folds) = compute(&text.to_string());
    }

    fn styles(&self, range: &Range<usize>, resolver: &dyn HighlightStyleResolver) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut out = vec![];
        let mut pos = range.start;
        let first = self.spans.partition_point(|(r, _)| r.end <= range.start);
        for (r, name) in self.spans[first..].iter().take_while(|(r, _)| r.start < range.end) {
            let (s, e) = (r.start.max(range.start), r.end.min(range.end));
            if s > pos {
                out.push((pos..s, HighlightStyle::default()));
            }
            out.push((s..e, name.and_then(|n| resolver.style(n)).unwrap_or_default()));
            pos = e;
        }
        if pos < range.end {
            out.push((pos..range.end, HighlightStyle::default()));
        }
        out
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        self.folds.clone()
    }
}

fn compute(src: &str) -> (Vec<Span>, Vec<FoldRange>) {
    let file = http_syntax::parse(src);
    let request_lines: HashSet<usize> = file.requests.iter().map(|r| r.request_line).collect();
    let header_lines: HashSet<usize> = file.requests.iter().flat_map(|r| r.headers.iter().map(|h| h.line)).collect();
    let body_lines: HashSet<usize> = file
        .requests
        .iter()
        .filter_map(|r| r.body.as_ref())
        .flat_map(|b| b.line..b.line + b.text.lines().count())
        .collect();
    let var_lines: HashSet<usize> = file.variables.iter().map(|v| v.line).collect();

    let mut spans = vec![];
    let mut off = 0;
    for (i, raw) in src.split_inclusive('\n').enumerate() {
        let ln = i + 1;
        let line = raw.trim_end_matches(['\n', '\r']);
        let t = line.trim_start();
        let base = off + line.len() - t.len();
        let end = off + line.len();
        if line.starts_with("###") {
            spans.push((off..end, Some("title")));
        } else if body_lines.contains(&ln) {
            vars(&mut spans, line, off, None);
        } else if request_lines.contains(&ln) {
            for (j, (at, tok)) in tokens(line).into_iter().enumerate() {
                let style = if j == 0 && tok.bytes().all(|b| b.is_ascii_uppercase()) && tok.len() > 1 {
                    Some("keyword")
                } else if tok.starts_with("HTTP/") {
                    Some("comment")
                } else {
                    Some("string")
                };
                vars(&mut spans, tok, off + at, style);
            }
        } else if header_lines.contains(&ln) {
            let colon = t.find(':').unwrap_or(t.len());
            spans.push((base..base + colon, Some("property")));
            vars(&mut spans, &t[colon..], base + colon, None);
        } else if t.starts_with('#') || t.starts_with("//") {
            let is_directive = t.trim_start_matches(['#', '/']).trim_start().starts_with('@');
            spans.push((base..end, Some(if is_directive { "attribute" } else { "comment" })));
        } else if var_lines.contains(&ln) {
            let eq = t.find('=').unwrap_or(t.len());
            spans.push((base..base + eq, Some("variable")));
            vars(&mut spans, &t[eq..], base + eq, None);
        } else if t.starts_with('?') || t.starts_with('&') {
            vars(&mut spans, t, base, Some("string"));
        } else if t.starts_with('>') || t.starts_with('<') {
            spans.push((base..end, Some("comment")));
        }
        off += raw.len();
    }
    spans.retain(|(r, _)| !r.is_empty());

    let folds = file
        .requests
        .iter()
        .filter(|r| r.lines.end() > r.lines.start())
        .map(|r| FoldRange::new(r.lines.start() - 1, r.lines.end() - 1))
        .collect();
    (spans, folds)
}

/// Whitespace-separated tokens with their byte offsets.
fn tokens(s: &str) -> Vec<(usize, &str)> {
    let mut out = vec![];
    let mut start = None;
    for (i, c) in s.char_indices().chain([(s.len(), ' ')]) {
        match (c.is_whitespace(), start) {
            (true, Some(st)) => {
                out.push((st, &s[st..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    out
}

/// Split `s` (at byte offset `at`) into `{{var}}` spans and `style` spans.
fn vars(spans: &mut Vec<Span>, s: &str, at: usize, style: Option<&'static str>) {
    let mut pos = 0;
    while let Some(open) = s[pos..].find("{{").map(|p| p + pos) {
        let Some(close) = s[open..].find("}}").map(|p| p + open + 2) else { break };
        spans.push((at + pos..at + open, style));
        spans.push((at + open..at + close, Some("variable")));
        pos = close;
    }
    spans.push((at + pos..at + s.len(), style));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_ordered_and_classified() {
        let src = "@host = x\n### Users\n# @name list\nGET https://{{host}}/u HTTP/1.1\nAccept: {{t}}\n\n{\"a\": \"{{$uuid}}\"}\n";
        let (spans, folds) = compute(src);
        assert!(spans.windows(2).all(|w| w[0].0.end <= w[1].0.start));
        let of = |s: &str| spans.iter().find(|(r, _)| &src[r.clone()] == s).map(|(_, n)| *n);
        assert_eq!(of("@host "), Some(Some("variable")));
        assert_eq!(of("### Users"), Some(Some("title")));
        assert_eq!(of("# @name list"), Some(Some("attribute")));
        assert_eq!(of("GET"), Some(Some("keyword")));
        assert_eq!(of("{{host}}"), Some(Some("variable")));
        assert_eq!(of("HTTP/1.1"), Some(Some("comment")));
        assert_eq!(of("Accept"), Some(Some("property")));
        assert_eq!(of("{{$uuid}}"), Some(Some("variable")));
        assert_eq!(folds.len(), 1);
    }
}
