//! `.http` highlighter on top of `http_syntax::parse`, plugged into the editor's
//! `InputHighlighter` seam (no tree-sitter grammar needed).
// ponytail: full reparse per edit, O(file): ~0.7 ms for 6k lines, ~2.3 ms for 24k (release);
// incremental only if real files outgrow that.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use http_syntax::HttpFile;

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

    /// Multi-cursor typing: the default calls `update` per cursor; only the final text matters.
    fn update_batch(&mut self, edits: &[(InputEdit, Rope)], folding: bool, window: &mut Window, cx: &mut Context<EditorState>) {
        if let Some((edit, text)) = edits.last() {
            self.update(Some(*edit), text, folding, window, cx);
        }
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

thread_local! {
    static LAST: std::cell::RefCell<(String, Arc<HttpFile>)> = Default::default();
}

/// `http_syntax::parse`, memoized on the last text: the highlighter and the doc both parse
/// after every edit, so the second one is a string compare.
pub fn parse(src: &str) -> Arc<HttpFile> {
    LAST.with_borrow_mut(|(text, file)| {
        if text != src {
            *file = Arc::new(http_syntax::parse(src));
            *text = src.to_owned();
        }
        file.clone()
    })
}

fn compute(src: &str) -> (Vec<Span>, Vec<FoldRange>) {
    let file = parse(src);
    let request_lines: HashSet<usize> = file.requests.iter().map(|r| r.request_line).collect();
    let header_lines: HashSet<usize> = file.requests.iter().flat_map(|r| r.headers.iter().map(|h| h.line)).collect();
    let body_lines: HashMap<usize, Body> = file
        .requests
        .iter()
        .filter_map(|r| r.body.as_ref().map(|b| (b, body_kind(r, b))))
        .flat_map(|(b, kind)| (b.line..b.line + b.text.lines().count()).map(move |l| (l, kind)))
        .collect();
    // Multipart: the lines between a `--boundary` and the first blank line are part headers.
    let mut part_headers = false;
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
        } else if let Some(kind) = body_lines.get(&ln) {
            if t.starts_with("< ") || t.starts_with("<@") {
                spans.push((base..end, Some("comment"))); // body from a file
            } else {
                match kind {
                    Body::Json => json(&mut spans, line, off),
                    Body::Form => form(&mut spans, line, off),
                    Body::Xml => xml(&mut spans, line, off),
                    Body::Code(lang) => code(&mut spans, line, off, *lang),
                    Body::Multipart if t.starts_with("--") => {
                        part_headers = true;
                        spans.push((base..end, Some("keyword")));
                    }
                    Body::Multipart if t.is_empty() => part_headers = false,
                    Body::Multipart if part_headers => {
                        let colon = t.find(':').unwrap_or(t.len());
                        spans.push((base..base + colon, Some("property")));
                        vars(&mut spans, &t[colon..], base + colon, None);
                    }
                    // A part's content: JSON parts look like JSON, the rest stays plain.
                    Body::Multipart if t.starts_with(['{', '[', '"', '}', ']']) => json(&mut spans, line, off),
                    Body::Multipart | Body::Plain => vars(&mut spans, line, off, None),
                }
            }
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

    let blocks = file.requests.iter().filter(|r| r.lines.end() > r.lines.start()).map(|r| FoldRange::new(r.lines.start() - 1, r.lines.end() - 1));
    let folds = blocks.chain(body_folds(&file)).collect();
    (spans, folds)
}

/// Body folds (0-based lines), so a big JSON hides without hiding its request. A fold keeps its
/// first and last line (`{` … `}`).
pub fn body_folds(file: &http_syntax::HttpFile) -> Vec<FoldRange> {
    file.requests
        .iter()
        .filter_map(|r| r.body.as_ref())
        .map(|b| (b.line - 1, b.text.lines().count()))
        .filter(|&(_, n)| n >= 3)
        .map(|(start, n)| FoldRange::new(start, start + n - 1))
        .collect()
}

#[derive(Clone, Copy)]
enum Body {
    Json,
    Form,
    Multipart,
    Xml,
    Code(Lang),
    Plain,
}

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    GraphQl,
    Js,
}

/// From `Content-Type` (or VS Code's `X-Request-Type: GraphQL`); without one, a body that opens
/// with `{` / `[` is JSON and one that opens with a tag is XML.
fn body_kind(r: &http_syntax::Request, b: &http_syntax::Body) -> Body {
    let header = |n: &str| r.headers.iter().find(|h| h.name.eq_ignore_ascii_case(n)).map(|h| h.value.to_ascii_lowercase());
    if header("x-request-type").is_some_and(|v| v == "graphql") {
        return Body::Code(Lang::GraphQl);
    }
    let t = b.text.trim_start();
    match header("content-type").as_deref() {
        Some(c) if c.contains("graphql") && !c.contains("json") => Body::Code(Lang::GraphQl),
        Some(c) if c.contains("json") => Body::Json,
        Some(c) if c.starts_with("multipart/") => Body::Multipart,
        Some(c) if c.contains("x-www-form-urlencoded") => Body::Form,
        Some(c) if c.contains("xml") || c.contains("html") => Body::Xml,
        Some(c) if c.contains("javascript") || c.contains("ecmascript") => Body::Code(Lang::Js),
        Some(_) => Body::Plain,
        None if t.starts_with(['{', '[']) => Body::Json,
        None if t.starts_with('<') && t[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '?' || c == '!') => Body::Xml,
        None => Body::Plain,
    }
}

/// One line of XML / HTML: tags, attributes, values, comments, `{{vars}}` in text. Line-local:
/// a tag or comment spanning lines is coloured on its first line only.
fn xml(spans: &mut Vec<Span>, line: &str, at: usize) {
    let mut i = 0;
    while i < line.len() {
        let rest = &line[i..];
        if rest.starts_with("<!--") {
            let end = rest.find("-->").map_or(line.len(), |p| i + p + 3);
            spans.push((at + i..at + end, Some("comment")));
            i = end;
        } else if rest.starts_with('<') && !rest.starts_with("< ") {
            // `<name` or `</name` / `<?xml`, then attributes up to `>`.
            let name_end = rest[1..].find(|c: char| c.is_whitespace() || c == '>' || c == '/' && !rest[1..].starts_with('/')).map_or(line.len(), |p| i + 1 + p);
            spans.push((at + i..at + name_end, Some("tag")));
            i = name_end;
            while i < line.len() && !line[i..].starts_with('>') {
                let r = &line[i..];
                if r.starts_with('"') || r.starts_with('\'') {
                    let q = &r[..1];
                    let end = r[1..].find(q).map_or(line.len(), |p| i + p + 2);
                    vars(spans, &line[i..end], at + i, Some("string"));
                    i = end;
                } else if r.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == ':') {
                    let end = r.find(|c: char| !(c.is_alphanumeric() || "_:-.".contains(c))).map_or(line.len(), |p| i + p);
                    spans.push((at + i..at + end, Some("attribute")));
                    i = end;
                } else {
                    let n = r.chars().next().map_or(1, char::len_utf8);
                    spans.push((at + i..at + i + n, Some("tag")));
                    i += n;
                }
            }
            if i < line.len() {
                spans.push((at + i..at + i + 1, Some("tag")));
                i += 1;
            }
        } else {
            let end = rest[1..].find('<').map_or(line.len(), |p| i + 1 + p);
            vars(spans, &line[i..end], at + i, None);
            i = end;
        }
    }
}

const GRAPHQL_KEYWORDS: &[&str] = &[
    "query", "mutation", "subscription", "fragment", "on", "true", "false", "null", "schema", "type", "input", "enum", "interface", "union", "scalar",
    "directive", "extend", "implements",
];
const JS_KEYWORDS: &[&str] = &[
    "const", "let", "var", "function", "return", "if", "else", "for", "while", "do", "break", "continue", "new", "this", "typeof", "instanceof", "in",
    "of", "class", "extends", "import", "export", "from", "default", "async", "await", "try", "catch", "finally", "throw", "switch", "case", "true",
    "false", "null", "undefined", "yield", "delete", "void", "static",
];

/// One line of GraphQL or JavaScript: keywords, strings, numbers, comments, calls, `$vars`.
fn code(spans: &mut Vec<Span>, line: &str, at: usize, lang: Lang) {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let rest = &line[i..];
        if (lang == Lang::GraphQl && rest.starts_with('#')) || (lang == Lang::Js && rest.starts_with("//")) {
            spans.push((at + i..at + line.len(), Some("comment")));
            return;
        }
        match b[i] {
            q @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                vars(spans, &line[start..i], at + start, Some("string"));
            }
            b'{' if b.get(i + 1) == Some(&b'{') => {
                i = rest.find("}}").map_or(b.len(), |p| i + p + 2);
                spans.push((at + start..at + i, Some("variable")));
            }
            b'0'..=b'9' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                spans.push((at + start..at + i, Some("number")));
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c == b'@' => {
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                    i += 1;
                }
                let word = &line[start..i];
                let keywords = if lang == Lang::GraphQl { GRAPHQL_KEYWORDS } else { JS_KEYWORDS };
                let style = if keywords.contains(&word) {
                    Some("keyword")
                } else if word.starts_with('$') && lang == Lang::GraphQl {
                    Some("variable")
                } else if word.starts_with('@') {
                    Some("attribute")
                } else if line[i..].trim_start().starts_with('(') {
                    Some("function")
                } else if word.starts_with(|c: char| c.is_ascii_uppercase()) {
                    Some("type")
                } else if lang == Lang::GraphQl && line[i..].trim_start().starts_with(':') {
                    Some("property")
                } else {
                    None
                };
                spans.push((at + start..at + i, style));
            }
            _ => i += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
}

/// One line of JSON: keys, strings, numbers, literals, `{{vars}}`. Line-local on purpose: JSON
/// strings can't hold a raw newline, so an unclosed `"` only colours the rest of its own line.
fn json(spans: &mut Vec<Span>, line: &str, at: usize) {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                let key = line[i..].trim_start().starts_with(':');
                vars(spans, &line[start..i], at + start, Some(if key { "property" } else { "string" }));
            }
            b'{' if b.get(i + 1) == Some(&b'{') => {
                i = line[i..].find("}}").map_or(b.len(), |p| i + p + 2);
                spans.push((at + start..at + i, Some("variable")));
            }
            b'-' | b'0'..=b'9' => {
                while i < b.len() && matches!(b[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    i += 1;
                }
                spans.push((at + start..at + i, Some("number")));
            }
            c if c.is_ascii_alphabetic() => {
                while i < b.len() && b[i].is_ascii_alphanumeric() {
                    i += 1;
                }
                let style = match &line[start..i] {
                    "true" | "false" => Some("boolean"),
                    "null" => Some("constant"),
                    _ => None,
                };
                spans.push((at + start..at + i, style));
            }
            _ => i += line[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
}

/// `a=1&b={{x}}`: names as properties, values as strings.
fn form(spans: &mut Vec<Span>, line: &str, at: usize) {
    let mut pos = 0;
    for pair in line.split_inclusive('&') {
        let eq = pair.find('=').unwrap_or(pair.len());
        vars(spans, &pair[..eq], at + pos, Some("property"));
        vars(spans, &pair[eq..], at + pos + eq, Some("string"));
        pos += pair.len();
    }
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

    #[test]
    fn bodies_by_content_type() {
        let src = "### J\nPOST /j\n\n{\"id\": 12, \"ok\": true, \"n\": null, \"s\": \"x {{v}}\", \"c\": {{c}}}\n\n\
                   ### F\nPOST /f\nContent-Type: application/x-www-form-urlencoded\n\nname=Ada&id={{id}}\n\n\
                   ### M\nPOST /m\nContent-Type: multipart/form-data; boundary=B\n\n--B\nContent-Disposition: form-data; name=\"f\"\n\n{\"k\": 1}\n< ./a.png\n--B--\n";
        let (spans, _) = compute(src);
        assert!(spans.windows(2).all(|w| w[0].0.end <= w[1].0.start));
        let of = |s: &str| spans.iter().find(|(r, _)| &src[r.clone()] == s).and_then(|(_, n)| *n);
        assert_eq!(of("\"id\""), Some("property"));
        assert_eq!(of("12"), Some("number"));
        assert_eq!(of("true"), Some("boolean"));
        assert_eq!(of("null"), Some("constant"));
        assert_eq!(of("\"x "), Some("string"));
        assert_eq!(of("{{v}}"), Some("variable"));
        assert_eq!(of("{{c}}"), Some("variable"));
        assert_eq!(of("name"), Some("property"));
        assert_eq!(of("=Ada&"), Some("string"));
        assert_eq!(of("--B"), Some("keyword"));
        assert_eq!(of("Content-Disposition"), Some("property"));
        assert_eq!(of("\"k\""), Some("property"));
        assert_eq!(of("< ./a.png"), Some("comment"));
        // Only the 6-line multipart body folds (JSON and form bodies are one line); lines 0-based.
        let f = body_folds(&http_syntax::parse(src));
        assert_eq!(f.iter().map(|f| (f.start_line, f.end_line)).collect::<Vec<_>>(), [(15, 20)]);
    }

    #[test]
    fn xml_graphql_js_bodies() {
        let src = "### X\nPOST /x\nContent-Type: application/xml\n\n<?xml version=\"1.0\"?>\n<user id=\"{{id}}\"><!-- c --><name>Ada</name></user>\n\n\
                   ### G\nPOST /g\nX-Request-Type: GraphQL\n\nquery Q($id: ID!) { user(id: $id) { name } } # c\n\n\
                   ### J\nPOST /j\nContent-Type: text/javascript\n\nconst x = f(\"s\", 1); // c\n";
        let (spans, _) = compute(src);
        assert!(spans.windows(2).all(|w| w[0].0.end <= w[1].0.start));
        let of = |s: &str| spans.iter().find(|(r, _)| &src[r.clone()] == s).and_then(|(_, n)| *n);
        assert_eq!(of("<?xml"), Some("tag"));
        assert_eq!(of("<user"), Some("tag"));
        assert_eq!(of("id"), Some("attribute"));
        assert_eq!(of("{{id}}"), Some("variable"));
        assert_eq!(of("<!-- c -->"), Some("comment"));
        assert_eq!(of("</name"), Some("tag"));
        assert_eq!(of("Ada"), None);
        assert_eq!(of("query"), Some("keyword"));
        assert_eq!(of("$id"), Some("variable"));
        assert_eq!(of("ID"), Some("type"));
        assert_eq!(of("user"), Some("function"));
        assert_eq!(of("# c"), Some("comment"));
        assert_eq!(of("const"), Some("keyword"));
        assert_eq!(of("f"), Some("function"));
        assert_eq!(of("\"s\""), Some("string"));
        assert_eq!(of("// c"), Some("comment"));
    }
}
