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
    input::{
        EditorState, GutterDecoration, GutterDecorationCollection, RangeDecoration, RangeDecorationCollection,
        RangeDecorationStyle, RopeExt as _,
    },
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
    pub exec: Arc<Executor>,
    /// `request_line` of the block under the cursor.
    pub active: Option<usize>,
    /// Byte range currently decorated as the active block.
    applied: Option<Range<usize>>,
    block_fill: RangeDecorationCollection,
    block_bar: GutterDecorationCollection,
    // ponytail: keyed by request line, so statuses drift when lines above are edited;
    // key by block identity once ResponseStore exists.
    pub status: HashMap<usize, Status>,
    pub tasks: HashMap<usize, Task<()>>,
    /// Edited since load / last save.
    pub dirty: bool,
    pub _subs: Vec<Subscription>,
}

impl Doc {
    pub fn new(
        path: PathBuf,
        editor: Entity<EditorState>,
        env: HashMap<String, Variable>,
        exec: Arc<Executor>,
        subs: Vec<Subscription>,
        cx: &mut App,
    ) -> Self {
        let (block_fill, block_bar) = editor.update(cx, |s, cx| {
            (s.create_range_decorations_collection(vec![], cx), s.create_gutter_decorations_collection(vec![], cx))
        });
        let mut doc = Self {
            path: Arc::new(path),
            editor,
            file: Arc::default(),
            env: Arc::new(env),
            exec,
            active: None,
            applied: None,
            block_fill,
            block_bar,
            status: HashMap::new(),
            tasks: HashMap::new(),
            dirty: false,
            _subs: subs,
        };
        doc.reparse(cx);
        doc
    }

    pub fn name(&self) -> String {
        self.path.file_name().map_or_else(|| "untitled".into(), |n| n.to_string_lossy().into_owned())
    }

    // ponytail: parses on every keystroke on the UI thread; move to background + 30 ms debounce
    // when large files stutter.
    pub fn reparse(&mut self, cx: &mut App) {
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
        let (tint, bar) = (cx.theme().primary.opacity(0.07), cx.theme().primary);
        self.block_fill.set(
            range.iter().map(|r| RangeDecoration::new(r.clone()).with_style(RangeDecorationStyle::Fill).with_color(tint)).collect(),
            cx,
        );
        self.block_bar.set(range.iter().map(|r| GutterDecoration::new(r.clone()).with_color(bar)).collect(), cx);
    }
}
