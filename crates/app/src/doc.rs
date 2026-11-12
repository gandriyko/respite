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
    /// Environments visible from this file's env-file chain.
    pub env_names: Vec<String>,
    pub exec: Arc<Executor>,
    /// `request_line` of the block under the cursor.
    pub active: Option<usize>,
    /// Byte range currently decorated as the active block.
    applied: Option<Range<usize>>,
    block_bar: GutterDecorationCollection,
    /// Keyed by request line; `reparse` moves entries along with their block.
    pub status: HashMap<usize, Status>,
    /// Running requests: request line → (run id, task). The id finds the block again when the
    /// response arrives, even if lines above were edited meanwhile.
    pub tasks: HashMap<usize, (u64, Task<()>)>,
    /// Edited since load / last save.
    pub dirty: bool,
    /// `false` for env files (`respite*.json`): plain JSON, no requests, no ▶.
    pub http: bool,
    fold_counts: FoldCounts,
    pub _subs: Vec<Subscription>,
}

/// Body fold start row (0-based) → lines it hides; shared with the editor's fold placeholder.
pub type FoldCounts = std::rc::Rc<std::cell::RefCell<HashMap<usize, usize>>>;

impl Doc {
    pub fn new(
        path: PathBuf,
        editor: Entity<EditorState>,
        (env, env_names): (HashMap<String, Variable>, Vec<String>),
        exec: Arc<Executor>,
        subs: Vec<Subscription>,
        fold_counts: FoldCounts,
        cx: &mut App,
    ) -> Self {
        let block_bar = editor.update(cx, |s, cx| s.create_gutter_decorations_collection(vec![], cx));
        let http = !crate::workspace::is_env_file(&path);
        let mut doc = Self {
            path: Arc::new(path),
            editor,
            file: Arc::default(),
            env: Arc::new(env),
            env_names,
            exec,
            active: None,
            applied: None,
            block_bar,
            status: HashMap::new(),
            tasks: HashMap::new(),
            dirty: false,
            http,
            fold_counts,
            _subs: subs,
        };
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
        self.sync_active(cx);
    }

    pub fn cursor_request(&self, cx: &App) -> Option<&Request> {
        let line = self.editor.read(cx).cursor_position().line as usize + 1;
        self.file.request_at(line)
    }

    pub fn sync_active(&mut self, cx: &mut App) {
        let req = self.cursor_request(cx).map(|r| (r.request_line, r.lines.clone()));
        self.active = req.as_ref().map(|r| r.0);
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
        // Only a gutter bar marks the block (as in Zed / JetBrains): gpui-component has no
        // full-width line background, and a text-range fill paints a ragged edge plus an extra
        // cell for every line's `\n`.
        // ponytail: bring a background back if the editor gains full-width line decorations.
        let bar = cx.theme().primary;
        self.block_bar.set(range.iter().map(|r| GutterDecoration::new(r.clone()).with_color(bar)).collect(), cx);
    }
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
