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
    // ponytail: keyed by request line, so statuses drift when lines above are edited;
    // key by block identity once ResponseStore exists.
    pub status: HashMap<usize, Status>,
    pub tasks: HashMap<usize, Task<()>>,
    /// Edited since load / last save.
    pub dirty: bool,
    /// `false` for `*.env.json`: plain JSON, no requests, no ▶.
    pub http: bool,
    pub _subs: Vec<Subscription>,
}

impl Doc {
    pub fn new(
        path: PathBuf,
        editor: Entity<EditorState>,
        (env, env_names): (HashMap<String, Variable>, Vec<String>),
        exec: Arc<Executor>,
        subs: Vec<Subscription>,
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

    // ponytail: parses on every keystroke on the UI thread; move to background + 30 ms debounce
    // when large files stutter.
    pub fn reparse(&mut self, cx: &mut App) {
        if !self.http {
            return;
        }
        self.file = Arc::new(http_syntax::parse(&self.editor.read(cx).value()));
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
