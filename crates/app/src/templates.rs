//! Live templates (as in JetBrains): `gtr`, `ptr`, `mptr`, `fptr` alone on a line + Tab expand
//! into a request; further Tabs select its fields in turn. Tab elsewhere indents as usual.

use std::ops::Range;

use gpui::*;
use gpui_component::input::{EditorState, IndentInline, RopeExt as _};

use crate::Respite;

/// `${…}` marks a field (selected in turn).
const TEMPLATES: &[(&str, &str)] = &[
    ("gtr", "GET ${http://localhost:8080/api/item}\nAccept: application/json\n"),
    ("ptr", "POST ${http://localhost:8080/api/item}\nContent-Type: application/json\n\n${{}}\n"),
    (
        "mptr",
        "POST ${http://localhost:8080/api/item}\nContent-Type: multipart/form-data; boundary=WebAppBoundary\n\n--WebAppBoundary\nContent-Disposition: form-data; name=\"${field-name}\"\n\n${field-value}\n--WebAppBoundary--\n",
    ),
    ("fptr", "POST ${http://localhost:8080/api/item}\nContent-Type: application/x-www-form-urlencoded\n\n${id=99&content=new-element}\n"),
];

/// Template text without the markers, and its fields' byte ranges.
pub fn expand(template: &str) -> (String, Vec<Range<usize>>) {
    let (mut text, mut fields, mut rest) = (String::new(), vec![], template);
    while let Some(open) = rest.find("${") {
        text += &rest[..open];
        // Fields may contain `{}`: the marker ends at the `}` that balances it.
        let body = &rest[open + 2..];
        let mut depth = 0;
        let close = body
            .char_indices()
            .find(|&(_, c)| {
                match c {
                    '{' => depth += 1,
                    '}' if depth == 0 => return true,
                    '}' => depth -= 1,
                    _ => {}
                }
                false
            })
            .map_or(body.len(), |(i, _)| i);
        fields.push(text.len()..text.len() + close);
        text += &body[..close];
        rest = body.get(close + 1..).unwrap_or("");
    }
    (text + rest, fields)
}

pub struct Session {
    editor: Entity<EditorState>,
    /// The selected field, then the ones still ahead (byte ranges as of `len`).
    current: Range<usize>,
    ahead: Vec<Range<usize>>,
    /// Text length when `current` was selected: typing in it shifts the fields ahead.
    len: usize,
}

impl Respite {
    /// Tab: next template field, or expand a template name, or indent.
    pub(crate) fn tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.doc().filter(|d| d.http).map(|d| d.editor.clone()) else {
            window.dispatch_action(Box::new(IndentInline), cx);
            return;
        };
        if let Some(s) = self.template.take().filter(|s| s.editor == editor) {
            let (len, cursor) = editor.read_with(cx, |e, _| (e.text().len(), e.cursor()));
            let delta = len as isize - s.len as isize;
            let end = (s.current.end as isize + delta) as usize;
            // Still in the field (typed over it): shift the rest by what was typed and go on.
            if (s.current.start..=end).contains(&cursor)
                && let Some((next, ahead)) = s.ahead.split_first()
            {
                let shift = |r: &Range<usize>| (r.start as isize + delta) as usize..(r.end as isize + delta) as usize;
                let next = shift(next);
                editor.update(cx, |e, cx| e.set_selected_range(next.clone(), cx));
                if !ahead.is_empty() {
                    self.template = Some(Session { editor: editor.clone(), current: next, ahead: ahead.iter().map(shift).collect(), len });
                }
                return;
            }
        }
        let (src, cursor) = editor.read_with(cx, |e, _| (e.value().to_string(), e.cursor()));
        let line_start = src[..cursor].rfind('\n').map_or(0, |i| i + 1);
        let line_end = src[cursor..].find('\n').map_or(src.len(), |i| cursor + i);
        let word = src[line_start..cursor].trim();
        let Some((_, template)) = TEMPLATES.iter().find(|(k, _)| *k == word).filter(|_| src[cursor..line_end].trim().is_empty()) else {
            window.dispatch_action(Box::new(IndentInline), cx);
            return;
        };
        let (text, fields) = expand(template);
        let start = line_start + src[line_start..cursor].find(word).unwrap_or(0);
        editor.update(cx, |e, cx| {
            let rope = e.text();
            let range = lsp_types::Range::new(rope.offset_to_position(start), rope.offset_to_position(line_end));
            // The template ends with a newline; the line already has one.
            let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
            e.apply_lsp_edits(&vec![lsp_types::TextEdit { range, new_text: text }], window, cx);
        });
        let fields: Vec<Range<usize>> = fields.into_iter().map(|r| start + r.start..start + r.end).collect();
        let len = editor.read(cx).text().len();
        if let Some((first, ahead)) = fields.split_first() {
            editor.update(cx, |e, cx| e.set_selected_range(first.clone(), cx));
            if !ahead.is_empty() {
                self.template = Some(Session { editor, current: first.clone(), ahead: ahead.to_vec(), len });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TEMPLATES, expand};

    #[test]
    fn fields() {
        let (text, f) = expand(TEMPLATES[1].1);
        assert_eq!(text, "POST http://localhost:8080/api/item\nContent-Type: application/json\n\n{}\n");
        assert_eq!(f.iter().map(|r| &text[r.clone()]).collect::<Vec<_>>(), ["http://localhost:8080/api/item", "{}"]);
        for (_, t) in TEMPLATES {
            let (text, _) = expand(t);
            assert!(!text.contains("${"));
            assert!(http_syntax::parse(&text).diagnostics.is_empty(), "{text}");
        }
    }
}
