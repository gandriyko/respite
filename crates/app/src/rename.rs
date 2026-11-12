//! F2: rename a file variable (`@name =` and every `{{name}}`) or a request name
//! (`# @name x` and every `{{x.response…}}`) in the current file.

use std::ops::Range;

use gpui::*;
use gpui_component::{
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, RopeExt as _},
};

use crate::Respite;

#[derive(Debug, PartialEq)]
pub enum Symbol {
    Var(String),
    Request(String),
}

impl Symbol {
    fn name(&self) -> &str {
        match self {
            Symbol::Var(n) | Symbol::Request(n) => n,
        }
    }
}

/// What the byte `offset` of `src` is on.
pub fn symbol_at(src: &str, offset: usize) -> Option<Symbol> {
    let file = crate::highlight::parse(src);
    let is_request = |n: &str| file.requests.iter().any(|r| r.name.as_deref() == Some(n));
    let line_start = src[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line = src[line_start..].lines().next().unwrap_or("");
    let col = offset - line_start;
    // Inside `{{ … }}`.
    if let Some(open) = line[..col.min(line.len())].rfind("{{")
        && let Some(close) = line[open..].find("}}").map(|c| c + open)
        && col <= close + 2
    {
        let inner = line[open + 2..close].trim();
        let head = inner.split('.').next().unwrap_or("");
        if inner.contains('.') && is_request(head) {
            return Some(Symbol::Request(head.into()));
        }
        return (!inner.is_empty() && !inner.starts_with('$') && !inner.contains(['.', ' '])).then(|| Symbol::Var(inner.into()));
    }
    let t = line.trim_start();
    if let Some((name, _)) = t.strip_prefix('@').and_then(|r| r.split_once('=')) {
        return Some(Symbol::Var(name.trim().into()));
    }
    let directive = t.strip_prefix('#').or_else(|| t.strip_prefix("//")).map(str::trim_start)?;
    let name = directive.strip_prefix("@name")?.trim();
    (!name.is_empty()).then(|| Symbol::Request(name.into()))
}

/// `src` with `sym` renamed to `new`.
pub fn rename(src: &str, sym: &Symbol, new: &str) -> String {
    let old = sym.name();
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        let t = line.trim_start();
        let indent = &line[..line.len() - t.len()];
        let line = match sym {
            Symbol::Var(_) if t.strip_prefix('@').and_then(|r| r.split_once('=')).is_some_and(|(n, _)| n.trim() == old) => {
                let eq = t.find('=').unwrap();
                format!("{indent}@{new} {}", &t[eq..])
            }
            Symbol::Request(_) => {
                let directive = t.strip_prefix('#').or_else(|| t.strip_prefix("//")).map(str::trim_start);
                match directive.and_then(|d| d.strip_prefix("@name")).filter(|v| v.trim() == old) {
                    Some(_) => {
                        let at = line.find("@name").unwrap();
                        format!("{}@name {new}{}", &line[..at], if line.ends_with('\n') { "\n" } else { "" })
                    }
                    None => line.to_string(),
                }
            }
            _ => line.to_string(),
        };
        out += &replace_refs(&line, sym, new);
    }
    out
}

/// `{{old}}` (variable) or `{{old.…}}` (request) inside braces.
fn replace_refs(line: &str, sym: &Symbol, new: &str) -> String {
    let (mut out, mut rest) = (String::new(), line);
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}").map(|c| c + open) else { break };
        let inner = &rest[open + 2..close];
        let trimmed = inner.trim();
        let renamed = match sym {
            Symbol::Var(old) if trimmed == old => Some(inner.replacen(old.as_str(), new, 1)),
            Symbol::Request(old) if trimmed.strip_prefix(old.as_str()).is_some_and(|r| r.starts_with('.')) => Some(inner.replacen(old.as_str(), new, 1)),
            _ => None,
        };
        out += &rest[..open + 2];
        out += renamed.as_deref().unwrap_or(inner);
        out += "}}";
        rest = &rest[close + 2..];
    }
    out + rest
}

/// The smallest byte range of `old` whose replacement gives `new` (one undo step, cursor kept).
pub(crate) fn changed(old: &str, new: &str) -> (Range<usize>, String) {
    let pre = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    let pre = (0..=pre).rev().find(|&i| old.is_char_boundary(i) && new.is_char_boundary(i)).unwrap_or(0);
    let max = old.len().min(new.len()) - pre;
    let suf = old.bytes().rev().zip(new.bytes().rev()).take(max).take_while(|(a, b)| a == b).count();
    let suf = (0..=suf).rev().find(|&i| old.is_char_boundary(old.len() - i) && new.is_char_boundary(new.len() - i)).unwrap_or(0);
    (pre..old.len() - suf, new[pre..new.len() - suf].to_string())
}

impl Respite {
    /// F2 on a variable or request name.
    pub(crate) fn rename_symbol(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc().filter(|d| d.http) else { return };
        let editor = d.editor.clone();
        let (src, offset) = {
            let s = editor.read(cx);
            (s.value().to_string(), s.cursor())
        };
        let Some(sym) = symbol_at(&src, offset) else {
            window.push_notification("Put the cursor on a {{variable}}, an @variable or a # @name to rename it", cx);
            return;
        };
        if let Symbol::Var(n) = &sym
            && !d.file.variables.iter().any(|v| v.name == *n)
        {
            window.push_notification(format!("{n} is not defined in this file (an environment variable?): rename it in its env file"), cx);
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).default_value(sym.name().to_string()));
        let sym = std::rc::Rc::new(sym);
        let apply = {
            let (input, editor, sym) = (input.clone(), editor.clone(), sym.clone());
            move |window: &mut Window, cx: &mut App| {
                let new = input.read(cx).value().trim().to_string();
                window.close_dialog(cx);
                if new.is_empty() || new.contains(|c: char| c.is_whitespace() || "{}.@#".contains(c)) || new == sym.name() {
                    return;
                }
                editor.update(cx, |s, cx| {
                    let old = s.value().to_string();
                    let (range, text) = changed(&old, &rename(&old, &sym, &new));
                    let rope = s.text();
                    let range = lsp_types::Range::new(rope.offset_to_position(range.start), rope.offset_to_position(range.end));
                    let cursor = s.cursor_position();
                    s.apply_lsp_edits(&vec![lsp_types::TextEdit { range, new_text: text }], window, cx);
                    s.set_cursor_position(cursor, window, cx);
                });
            }
        };
        let apply = std::rc::Rc::new(apply);
        self.picker_sub = Some(cx.subscribe_in(&input, window, {
            let apply = apply.clone();
            move |_, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    apply(window, cx);
                }
            }
        }));
        let title = match &*sym {
            Symbol::Var(n) => format!("Rename variable {n}"),
            Symbol::Request(n) => format!("Rename request {n}"),
        };
        let field = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let apply = apply.clone();
            dialog.title(title.clone()).w(px(420.)).child(Input::new(&field)).footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("rename-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("rename-ok").label("Rename").primary().on_click(move |_, window, cx| apply(window, cx))),
            )
        });
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{Symbol, changed, rename, symbol_at};

    const SRC: &str = "@host = a\n@url = {{host}}/x\n\n### Login\n# @name login\nPOST {{ host }}/login\n\n###\nGET {{url}}\nAuthorization: {{login.response.body.$.token}}\nX-Host: {{hostname}}\n";

    #[test]
    fn rename_variable_and_request() {
        let at = |needle: &str| symbol_at(SRC, SRC.find(needle).unwrap() + 1);
        assert_eq!(at("host = a"), Some(Symbol::Var("host".into())));
        assert_eq!(at("{ host }"), Some(Symbol::Var("host".into())));
        assert_eq!(at("login.response"), Some(Symbol::Request("login".into())));
        assert_eq!(at("name login"), Some(Symbol::Request("login".into())));
        assert_eq!(at("POST"), None);

        let out = rename(SRC, &Symbol::Var("host".into()), "server");
        assert_eq!(out, SRC.replace("@host =", "@server =").replace("{{host}}", "{{server}}").replace("{{ host }}", "{{ server }}"));
        assert!(out.contains("{{hostname}}"));

        let out = rename(SRC, &Symbol::Request("login".into()), "auth");
        assert_eq!(out, SRC.replace("@name login", "@name auth").replace("{{login.", "{{auth."));

        let (r, t) = changed("a {{x}} b {{x}} c", "a {{yy}} b {{yy}} c");
        assert_eq!((r, t.as_str()), (4..13, "yy}} b {{yy"));
    }
}
