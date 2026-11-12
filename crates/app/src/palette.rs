//! Navigation: one search dialog for Cmd+P (files), Cmd+Shift+O (requests in the file) and
//! Cmd+Shift+P (commands); Alt+↑/↓ between requests; the breadcrumb bar above the editor.

use std::{cell::Cell, rc::Rc};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, WindowExt as _, h_flex,
    input::{Input, InputEvent, InputState, Position},
    v_flex,
};

use crate::Respite;

type Run = Rc<dyn Fn(&mut Respite, &mut Window, &mut Context<Respite>)>;
/// Picks the item at an index into the item list (closes the dialog, then runs it).
type Choose = Rc<dyn Fn(usize, &mut Window, &mut App)>;

pub struct Item {
    pub label: String,
    pub detail: String,
    pub run: Run,
}

/// Matches kept; typing narrows further.
const SHOWN: usize = 50;
/// Rows on screen at once: a window that follows the selection, so the selected row is
/// always fully visible (a clipped list would hide it at the bottom).
const VISIBLE: usize = 8;

/// Lower is better; `None` = no match. Substring beats a scattered (fuzzy) match.
fn score(hay: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let (hay, query) = (hay.to_lowercase(), query.to_lowercase());
    if let Some(at) = hay.find(&query) {
        return Some(at);
    }
    let mut chars = hay.chars();
    query.chars().all(|q| chars.any(|h| h == q)).then_some(1000)
}

fn matches(items: &[Item], query: &str) -> Vec<usize> {
    let mut hits: Vec<(usize, usize)> =
        items.iter().enumerate().filter_map(|(i, it)| Some((score(&format!("{} {}", it.label, it.detail), query)?, i))).collect();
    hits.sort_by_key(|(s, _)| *s);
    hits.into_iter().take(SHOWN).map(|(_, i)| i).collect()
}

impl Respite {
    fn open_picker(&mut self, placeholder: &str, items: Vec<Item>, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()));
        let selected = Rc::new(Cell::new(0usize));
        // The dialog is rebuilt on window refresh; typing has to trigger one.
        self.picker_sub = Some(cx.subscribe_in(&input, window, {
            let selected = selected.clone();
            move |_, _, ev: &InputEvent, window, _| {
                if matches!(ev, InputEvent::Change) {
                    selected.set(0);
                    window.refresh();
                }
            }
        }));
        let items = Rc::new(items);
        let view = cx.entity().downgrade();
        let field = input.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let shown = Rc::new(matches(&items, &field.read(cx).value()));
            let sel = selected.get().min(shown.len().saturating_sub(1));
            let choose: Choose = Rc::new({
                let (items, view) = (items.clone(), view.clone());
                move |i, window, cx| {
                    window.close_dialog(cx);
                    let run = items[i].run.clone();
                    view.update(cx, |this, cx| run(this, window, cx)).ok();
                }
            });
            let keys = {
                let (shown, selected, choose) = (shown.clone(), selected.clone(), choose.clone());
                move |ev: &KeyDownEvent, window: &mut Window, cx: &mut App| {
                    let n = shown.len();
                    match ev.keystroke.key.as_str() {
                        "down" if n > 0 => selected.set((sel + 1) % n),
                        "up" if n > 0 => selected.set((sel + n - 1) % n),
                        "enter" => {
                            if let Some(&i) = shown.get(sel) {
                                cx.stop_propagation();
                                choose(i, window, cx);
                            }
                            return;
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                    window.refresh();
                }
            };
            let (muted, active) = (cx.theme().muted_foreground, cx.theme().list_active);
            let first = (sel + 1).saturating_sub(VISIBLE);
            let rows = shown.iter().enumerate().skip(first).take(VISIBLE).map(|(row, &i)| {
                let (choose, selected) = (choose.clone(), selected.clone());
                h_flex()
                    .id(("pick", row))
                    .px_2()
                    .py_1()
                    .gap_3()
                    .rounded_md()
                    .cursor_pointer()
                    .when(row == sel, |el| el.bg(active))
                    // The mouse moves the selection: one highlighted row, never two.
                    .on_mouse_move(move |_, window, _| {
                        if selected.get() != row {
                            selected.set(row);
                            window.refresh();
                        }
                    })
                    .child(div().flex_none().child(items[i].label.clone()))
                    .child(div().min_w_0().truncate().text_sm().text_color(muted).child(items[i].detail.clone()))
                    .on_click(move |_, window, cx| choose(i, window, cx))
            });
            let more = (shown.len() > VISIBLE).then(|| {
                div().px_2().text_xs().text_color(muted).child(format!("{} of {} — type to narrow", sel + 1, shown.len()))
            });
            let empty = shown.is_empty().then(|| div().px_2().py_1().text_color(muted).child("No matches"));
            // No ✕: it sat on the search field. Esc or a click outside closes it.
            dialog.w(px(640.)).close_button(false).child(
                v_flex()
                    .gap_2()
                    .capture_key_down(keys)
                    .child(Input::new(&field))
                    .child(v_flex().children(rows).children(empty))
                    .children(more),
            )
        });
        input.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Cmd+P: every request file of every project.
    pub(crate) fn pick_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut items = vec![];
        for p in &mut self.projects {
            let root = p.root.clone();
            let name = if p.flat { String::new() } else { format!("{}/", p.name) };
            for f in p.files.get_or_insert_with(|| crate::workspace::scan(&root)).iter() {
                let path = f.clone();
                let rel = f.strip_prefix(&root).unwrap_or(f).parent().map(|d| d.display().to_string()).unwrap_or_default();
                items.push(Item {
                    label: f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    detail: format!("{name}{rel}"),
                    run: Rc::new(move |this, window, cx| {
                        this.open(path.clone(), None, window, cx);
                    }),
                });
            }
        }
        self.open_picker("Open file…", items, window, cx);
    }

    /// Cmd+Shift+O: the requests of the current file.
    pub(crate) fn pick_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc() else { return };
        let path = (*d.path).clone();
        let items = d
            .file
            .requests
            .iter()
            .map(|r| {
                let (path, line) = (path.clone(), r.request_line);
                Item {
                    label: r.label(),
                    detail: format!("{} {} · line {line}", r.method, r.url),
                    run: Rc::new(move |this, window, cx| this.go_to(path.clone(), line, false, window, cx)),
                }
            })
            .collect();
        self.open_picker("Go to request…", items, window, cx);
    }

    /// Cmd+T: requests of every request file in every project (open tabs: their unsaved text).
    // ponytail: parses every file per open; cache by mtime if workspaces grow to thousands of files.
    pub(crate) fn pick_workspace_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut items = vec![];
        let projects: Vec<_> = self.projects.iter_mut().map(|p| (p.root.clone(), p.files.get_or_insert_with(|| crate::workspace::scan(&p.root)).clone())).collect();
        for (root, files) in projects {
            for f in files.into_iter().filter(|f| !crate::workspace::is_env_file(f)) {
                let file = match self.docs.iter().find(|d| *d.path == f) {
                    Some(d) => d.file.clone(),
                    None => std::sync::Arc::new(http_syntax::parse(&std::fs::read_to_string(&f).unwrap_or_default())),
                };
                let rel = f.strip_prefix(&root).unwrap_or(&f).display().to_string();
                for r in &file.requests {
                    let (path, line) = (f.clone(), r.request_line);
                    items.push(Item {
                        label: r.label(),
                        detail: format!("{} {} · {rel}", r.method, r.url),
                        run: Rc::new(move |this, window, cx| this.go_to(path.clone(), line, false, window, cx)),
                    });
                }
            }
        }
        self.open_picker("Go to request in workspace…", items, window, cx);
    }

    /// Cmd+Shift+P: menu commands, by name.
    pub(crate) fn pick_command(&mut self, commands: Vec<(&'static str, Box<dyn Action>)>, window: &mut Window, cx: &mut Context<Self>) {
        let items = commands
            .into_iter()
            .map(|(name, action)| {
                let action: Rc<dyn Action> = Rc::from(action);
                Item {
                    label: name.into(),
                    detail: String::new(),
                    run: Rc::new(move |this, window, cx| {
                        // Editor commands act on the focused editor: give it focus back first.
                        if let Some(d) = this.doc() {
                            d.editor.update(cx, |s, cx| s.focus(window, cx));
                        }
                        window.dispatch_action(action.boxed_clone(), cx);
                    }),
                }
            })
            .collect();
        self.open_picker("Run command…", items, window, cx);
    }

    /// Alt+↑ / Alt+↓: the previous / next request's line, centred.
    pub(crate) fn step_request(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc() else { return };
        let here = d.editor.read(cx).cursor_position().line as usize + 1;
        let lines = d.file.requests.iter().map(|r| r.request_line);
        let target = if forward { lines.filter(|&l| l > here).min() } else { lines.filter(|&l| l < here).max() };
        let Some(line) = target else { return };
        let editor = d.editor.clone();
        editor.update(cx, |s, cx| s.set_cursor_position(Position::new(line as u32 - 1, 0), window, cx));
        crate::center_line(editor, line - 1, true, window, cx);
    }

    /// `project › folder › file › request` above the editor.
    pub(crate) fn breadcrumbs(&self, cx: &App) -> Option<impl IntoElement> {
        let d = self.doc()?;
        let mut parts: Vec<String> = vec![];
        if let Some(p) = self.projects.iter().filter(|p| d.path.starts_with(&p.root)).max_by_key(|p| p.root.components().count()) {
            if !p.flat {
                parts.push(p.name.clone());
            }
            let rel = d.path.strip_prefix(&p.root).unwrap_or(&d.path);
            parts.extend(rel.iter().map(|c| c.to_string_lossy().into_owned()));
        } else {
            parts.push(d.name());
        }
        if let Some(r) = d.active.and_then(|l| d.file.requests.iter().find(|r| r.request_line == l)) {
            parts.push(r.label());
        }
        let muted = cx.theme().muted_foreground;
        Some(
            h_flex()
                .px_3()
                .py_1()
                .gap_1()
                .text_sm()
                .text_color(muted)
                .overflow_hidden()
                .children(parts.into_iter().enumerate().map(|(i, p)| h_flex().gap_1().when(i > 0, |el| el.child("›")).child(p))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn ranking() {
        assert_eq!(score("users.http", ""), Some(0));
        assert!(score("users.http", "user") < score("get-users.http", "user"));
        assert!(score("users.http", "uht").is_some()); // fuzzy: u…h…t
        assert!(score("users.http", "user") < score("users.http", "uht"));
        assert_eq!(score("users.http", "xyz"), None);
    }
}
