//! One open `.http` file: its editor, parse, environment and request statuses.

use std::{
    collections::HashMap,
    ops::Range,
    path::PathBuf,
    sync::Arc,
};

use gpui::*;
use gpui_component::{
    ActiveTheme as _,
    input::{EditorState, GutterDecoration, GutterDecorationCollection, RopeExt as _},
};
use http_env::Variable;
use http_exec::{Exchange, Executor};
use http_syntax::{HttpFile, Request};

#[derive(Clone)]
pub enum Status {
    Running,
    Done(Arc<Exchange>),
    Failed(String),
}

pub struct Doc {
    pub path: Arc<PathBuf>,
    pub editor: Entity<EditorState>,
    pub file: Arc<HttpFile>,
    pub env: Arc<HashMap<String, Variable>>,
    /// Every environment (env files in the env folder).
    pub env_names: Vec<String>,
    /// The environment `env` holds: the file's `# @env` when it was last applied.
    pub env_name: Option<String>,
    pub exec: Arc<Executor>,
    /// `request_line` of the block under the cursor.
    pub active: Option<usize>,
    /// Byte range currently decorated as the active block.
    applied: Option<Range<usize>>,
    /// 0-based first / last line of the active block, for its background tint.
    block_lines: Option<(usize, usize)>,
    block_bar: GutterDecorationCollection,
    /// Keyed by request line; `reparse` moves entries along with their block.
    pub status: HashMap<usize, Status>,
    /// Running requests: request line → (run id, task). The id finds the block again when the
    /// response arrives, even if lines above were edited meanwhile.
    pub tasks: HashMap<usize, (u64, Task<()>)>,
    /// Edited since load / last save.
    pub dirty: bool,
    /// The file's mtime when it was last read or written: a newer one means it changed on disk.
    pub mtime: Option<std::time::SystemTime>,
    /// `false` for env files (`respite*.json`): plain JSON, no requests.
    pub http: bool,
    fold_counts: FoldCounts,
    /// Shared with the editor's completion / hover / definition providers.
    lang: crate::lang::Shared,
    pub _subs: Vec<Subscription>,
}

/// Body fold start row (0-based) → lines it hides; shared with the editor's fold placeholder.
pub type FoldCounts = std::rc::Rc<std::cell::RefCell<HashMap<usize, usize>>>;

/// A file's environment: the selected one's variables, every environment name, and the
/// variables of all environments together, and the selected environment's name.
pub type EnvView = (HashMap<String, Variable>, Vec<String>, HashMap<String, Variable>, Option<String>);

impl Doc {
    pub fn new(
        path: PathBuf,
        editor: Entity<EditorState>,
        (env, env_names, all, env_name): EnvView,
        exec: Arc<Executor>,
        subs: Vec<Subscription>,
        fold_counts: FoldCounts,
        cx: &mut App,
    ) -> Self {
        let block_bar = editor.update(cx, |s, cx| s.create_gutter_decorations_collection(vec![], cx));
        let http = !crate::workspace::is_env_file(&path);
        let (path, env) = (Arc::new(path), Arc::new(env));
        let lang =
            std::rc::Rc::new(std::cell::RefCell::new(crate::lang::Ctx { path: path.clone(), env: env.clone(), all: Arc::new(all), env_name: env_name.clone() }));
        let fixes = std::rc::Rc::new(crate::fixes::Fixes(lang.clone()));
        editor.update(cx, |s, _| {
            let lsp = s.lsp_mut();
            lsp.code_action_providers.push(fixes);
            if http {
                let provider = std::rc::Rc::new(crate::lang::Lang(lang.clone()));
                lsp.completion_provider = Some(provider.clone());
                lsp.hover_provider = Some(provider.clone());
                lsp.definition_provider = Some(provider);
            }
        });
        let mut doc = Self {
            path,
            editor,
            file: Arc::default(),
            env,
            env_names,
            env_name,
            exec,
            active: None,
            applied: None,
            block_lines: None,
            block_bar,
            status: HashMap::new(),
            tasks: HashMap::new(),
            dirty: false,
            mtime: None,
            http,
            fold_counts,
            lang,
            _subs: subs,
        };
        doc.mtime = mtime(&doc.path);
        doc.reparse(cx);
        doc.restore_responses();
        doc
    }

    /// Each request's newest recorded response, so the panel shows it before the first run.
    /// Matched by label (name / title / `METHOD url`); among equal labels, the same line wins.
    fn restore_responses(&mut self) {
        let Some(store) = self.exec.store() else { return };
        for e in store.entries_of(&self.path) {
            let same: Vec<usize> = self.file.requests.iter().filter(|r| r.label() == e.label).map(|r| r.request_line).collect();
            let Some(line) = same.iter().copied().find(|&l| l == e.line).or(same.first().copied()) else { continue };
            if !self.status.contains_key(&line)
                && let Some(ex) = store.exchange(e)
            {
                self.status.insert(line, Status::Done(Arc::new(ex)));
            }
        }
    }

    pub fn name(&self) -> String {
        self.path.file_name().map_or_else(|| "untitled".into(), |n| n.to_string_lossy().into_owned())
    }

    /// Shares the highlighter's parse of the same text (see `highlight::parse`).
    pub fn reparse(&mut self, cx: &mut App) {
        if !self.http {
            self.publish_json_error(cx);
            return;
        }
        let old = std::mem::replace(&mut self.file, crate::highlight::parse(&self.editor.read(cx).value()));
        if !self.status.is_empty() || !self.tasks.is_empty() {
            // A deleted block drops its status, and its task (which cancels the request).
            let moved = moved_lines(&old, &self.file);
            self.status = self.status.drain().filter_map(|(l, s)| Some((*moved.get(&l)?, s))).collect();
            self.tasks = self.tasks.drain().filter_map(|(l, t)| Some((*moved.get(&l)?, t))).collect();
        }
        // A fold shows its first and last line, so it hides the ones between.
        *self.fold_counts.borrow_mut() =
            crate::highlight::body_folds(&self.file).iter().map(|f| (f.start_line, f.end_line - f.start_line - 1)).collect();
        self.publish_diagnostics(cx);
        self.sync_active(cx);
    }

    /// `respite check` findings as editor underlines. The editor drops them on every edit, so
    /// this runs after each reparse (and after an environment switch).
    fn publish_diagnostics(&self, cx: &mut App) {
        let dir = self.path.parent().unwrap_or(std::path::Path::new("."));
        let all = self.lang.borrow().all.clone();
        let mut found = self.file.diagnostics.clone();
        found.extend(http_env::check_vars(&self.file, &all, dir));
        if let Some(env) = self.file.env.as_ref().filter(|e| !self.env_names.contains(e)) {
            let message = format!("no environment '{env}': create it from the environment menu");
            found.push(http_syntax::Diagnostic { line: 1, severity: http_syntax::Severity::Error, code: "unknown_environment", message });
        }
        self.editor.update(cx, |s, cx| {
            if let Some(set) = s.diagnostics_mut() {
                set.clear();
                set.extend(found.iter().map(crate::lang::diagnostic));
                cx.notify();
            }
        });
    }

    /// Env file: a JSON error underlined where the parser stopped (the file is skipped until fixed).
    fn publish_json_error(&self, cx: &mut App) {
        let err = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&self.editor.read(cx).value()).err();
        self.editor.update(cx, |s, cx| {
            let Some(set) = s.diagnostics_mut() else { return };
            set.clear();
            if let Some(e) = err {
                let (line, col) = (e.line().saturating_sub(1) as u32, e.column().saturating_sub(1) as u32);
                let msg = e.to_string();
                let msg = msg.rsplit_once(" at line ").map_or(msg.as_str(), |(m, _)| m);
                set.push(lsp_types::Diagnostic {
                    range: lsp_types::Range::new(lsp_types::Position::new(line, col), lsp_types::Position::new(line, col + 1)),
                    severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                    code: Some(lsp_types::NumberOrString::String("invalid_json".into())),
                    source: Some("respite".into()),
                    message: format!("invalid JSON, the file is skipped: {msg}"),
                    ..Default::default()
                });
            }
            cx.notify();
        });
    }

    /// After an environment switch: new values for run, hover and diagnostics.
    pub fn set_env(&mut self, (env, env_names, all, env_name): EnvView, exec: Arc<Executor>, cx: &mut App) {
        (self.env, self.env_names, self.env_name, self.exec) = (Arc::new(env), env_names, env_name.clone(), exec);
        {
            let mut lang = self.lang.borrow_mut();
            (lang.env, lang.all, lang.env_name) = (self.env.clone(), Arc::new(all), env_name);
        }
        if self.http {
            self.publish_diagnostics(cx);
        }
    }

    pub fn cursor_request(&self, cx: &App) -> Option<&Request> {
        let line = self.editor.read(cx).cursor_position().line as usize + 1;
        self.file.request_at(line)
    }

    pub fn sync_active(&mut self, cx: &mut App) {
        let req = self.cursor_request(cx).map(|r| (r.request_line, r.lines.clone()));
        self.active = req.as_ref().map(|r| r.0);
        self.block_lines = req.as_ref().map(|(_, l)| (l.start() - 1, l.end() - 1));
        let range = req.map(|(_, lines)| {
            let t = self.editor.read(cx).text();
            t.line_start_offset(lines.start() - 1)..t.line_end_offset(lines.end() - 1)
        });
        // Setting decorations notifies the editor, which re-enters the observer:
        // touch them only when the block actually changed, or it loops forever.
        if range == self.applied {
            return;
        }
        self.applied = range.clone();
        let bar = cx.theme().primary;
        self.block_bar.set(range.iter().map(|r| GutterDecoration::new(r.clone()).with_color(bar)).collect(), cx);
    }

    /// A faint full-width tint over the active block, laid over the editor (whose own range
    /// fill is ragged: it follows each line's text). Geometry is read at paint time, after the
    /// editor's prepaint, so it tracks scrolling and wrapping.
    pub fn block_tint(&self) -> impl IntoElement {
        let (editor, lines) = (self.editor.clone(), self.block_lines);
        canvas(
            |_, _, _| {},
            move |bounds, _, window, cx| {
                let Some((first, last)) = lines else { return };
                let s = editor.read(cx);
                let Some(vis) = s.visible_row_range().filter(|v| first < v.end && last >= v.start) else { return };
                let text = s.text();
                // An offset above the viewport resolves to the first visible line, so `top` clamps.
                let Some(top) = s.range_to_bounds(&(text.line_start_offset(first)..text.line_start_offset(first))) else { return };
                let end = text.line_end_offset(last);
                let bottom = (last < vis.end).then(|| s.range_to_bounds(&(end..end))).flatten().map_or(bounds.bottom(), |b| b.bottom());
                let left = top.left().max(bounds.left());
                let rect = Bounds::from_corners(point(left, top.top().max(bounds.top())), point(bounds.right(), bottom.min(bounds.bottom())));
                window.paint_quad(fill(rect, cx.theme().primary.opacity(0.07)));
            },
        )
        .absolute()
        .inset_0()
    }
}

impl Doc {
    /// Request lines running now. By status: a finished request's task stays in `tasks`.
    pub fn running_lines(&self) -> Vec<usize> {
        self.status.iter().filter(|(_, s)| matches!(s, Status::Running)).map(|(l, _)| *l).collect()
    }

    /// A green ▶ in the gutter at each visible request line, right of its number (a red ■ while
    /// it runs; setting `editor.send_button`). Painted over the editor at paint time from the
    /// layout just made, so it follows edits, wrapping, folding and scrolling. A click runs (or
    /// cancels) that request.
    pub fn send_buttons(&self, view: WeakEntity<crate::Respite>) -> impl IntoElement {
        let (editor, path) = (self.editor.clone(), self.path.clone());
        let lines: Vec<usize> = self.file.requests.iter().map(|r| r.request_line).collect();
        let running: Vec<usize> = self.running_lines();
        canvas(
            move |bounds, window, cx| {
                let s = editor.read(cx);
                let Some(vis) = s.visible_row_range() else { return vec![] };
                let (text, map) = (s.text(), s.display_map());
                let mut out = vec![];
                for &line in &lines {
                    let row = line - 1;
                    if map.is_buffer_line_hidden(row) || !vis.contains(&map.buffer_line_to_display_row(row)) {
                        continue;
                    }
                    let start = text.line_start_offset(row);
                    let Some(at) = s.range_to_bounds(&(start..start)) else { continue };
                    // A square as tall as the line, ending just before the text: right of the number.
                    let side = at.size.height;
                    let rect = Bounds::new(point((at.left() - side - px(2.)).max(bounds.left()), at.top()), size(side, side));
                    if rect.top() < bounds.top() || rect.bottom() > bounds.bottom() {
                        continue;
                    }
                    let hitbox = window.insert_hitbox(rect, HitboxBehavior::BlockMouseExceptScroll);
                    out.push((rect, hitbox, line, running.contains(&line)));
                }
                out
            },
            move |_, buttons, window, cx| {
                for (rect, hitbox, line, busy) in buttons {
                    paint_arrow(busy, hitbox.is_hovered(window), rect, window, cx);
                    window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                    let (view, path) = (view.clone(), path.clone());
                    window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble && e.button == MouseButton::Left && hitbox.is_hovered(window) {
                            cx.stop_propagation();
                            view.update(cx, |this, cx| this.send_or_cancel(&path, line, window, cx)).ok();
                        }
                    });
                }
            },
        )
        .absolute()
        .inset_0()
    }
}

fn shape(label: &str, size: Pixels, color: Hsla, window: &mut Window) -> ShapedLine {
    let font = window.text_style().font();
    let run = TextRun { len: label.len(), font, color, background_color: None, underline: None, strikethrough: None };
    window.text_system().shape_line(SharedString::from(label.to_string()), size, &[run], None)
}

/// A green ▶ (red ■ while running) in the gutter, at the request line.
fn paint_arrow(busy: bool, hover: bool, rect: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let t = cx.theme();
    let color = if busy { t.danger } else { t.success };
    if hover {
        window.paint_quad(fill(rect, t.list_hover).corner_radii(px(4.)));
    }
    let s = shape(if busy { "■" } else { "▶" }, t.font_size * 1.15, color, window);
    let at = point(rect.left() + (rect.size.width - s.width) / 2., rect.top());
    let _ = s.paint(at, rect.size.height, TextAlign::Left, None, window, cx);
}

pub fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Old request line → new one after an edit. Same number of blocks: matched in order (an edit
/// above shifts lines, editing a block's URL keeps it). Otherwise a block was added or removed:
/// matched by label, in order; unmatched ones are gone.
fn moved_lines(old: &HttpFile, new: &HttpFile) -> HashMap<usize, usize> {
    if old.requests.len() == new.requests.len() {
        return old.requests.iter().zip(&new.requests).map(|(o, n)| (o.request_line, n.request_line)).collect();
    }
    let mut free: Vec<(String, usize)> = new.requests.iter().map(|r| (r.label(), r.request_line)).collect();
    old.requests
        .iter()
        .filter_map(|o| {
            let label = o.label();
            let i = free.iter().position(|(l, _)| *l == label)?;
            Some((o.request_line, free.remove(i).1))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::moved_lines;
    use std::collections::HashMap;

    #[test]
    fn statuses_follow_their_block() {
        let p = http_syntax::parse;
        let base = p("### A\nGET /a\n\n### B\nGET /b\n");
        // Lines inserted above: same blocks, shifted.
        let shifted = p("# note\n\n### A\nGET /a\n\n### B\nGET /b\n");
        assert_eq!(moved_lines(&base, &shifted), HashMap::from([(2, 4), (5, 7)]));
        // URL edited: still the same block.
        assert_eq!(moved_lines(&base, &p("### A\nGET /a2\n\n### B\nGET /b\n")), HashMap::from([(2, 2), (5, 5)]));
        // Block A deleted: B moves up, A's status goes.
        assert_eq!(moved_lines(&base, &p("### B\nGET /b\n")), HashMap::from([(5, 2)]));
        // Block inserted above: both move down.
        assert_eq!(moved_lines(&base, &p("### C\nGET /c\n\n### A\nGET /a\n\n### B\nGET /b\n")), HashMap::from([(2, 5), (5, 8)]));
    }
}
