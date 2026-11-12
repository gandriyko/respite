//! Respite GUI: sidebar (projects → .http files → requests), tabs of open files,
//! editor with ▶ / status overlays, response panel.
//! ▶ and the status inlay are overlays positioned from the editor's last layout
//! (gpui-component has no custom gutter elements or inlay hints).

mod doc;
mod highlight;
mod workspace;

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, LazyLock},
};

use doc::{Doc, Status};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Root, Theme, ThemeMode, WindowExt as _, h_resizable,
    input::{Editor, EditorState, InputEvent, Position, RopeExt as _},
    resizable_panel,
    tab::{Tab, TabBar},
};
use http_env::Environments;
use http_exec::{Exchange, Executor, Store};
use workspace::Project;

actions!(respite, [RunRequest, SmartPaste, PasteCurl, CopyAsCurl, SaveFile, CloseTab, ToggleSidebar, NextTab]);

const SAMPLE: &str = include_str!("../sample.http");
const TABS: [&str; 3] = ["Body", "Headers", "Request"];

/// reqwest needs tokio; GPUI has its own executor. Requests run here, results are awaited from GPUI.
static RT: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Runtime::new().expect("tokio runtime"));

/// Aborts the tokio task when the GPUI task awaiting it is dropped (cancel / re-run).
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One line of the sidebar tree.
enum Row {
    Project(usize),
    File { path: PathBuf, rel: String },
    Request { path: PathBuf, line: usize, method: String, label: String },
    Note(&'static str),
}

struct Respite {
    env_name: Option<String>,
    /// One per workspace root (history and cookies are per project).
    executors: HashMap<PathBuf, Arc<Executor>>,
    docs: Vec<Doc>,
    current: Option<usize>,
    projects: Vec<Project>,
    /// Expanded project roots and files in the sidebar.
    expanded: HashSet<PathBuf>,
    /// `(request_line, method, label)` of files that are not open, parsed when expanded.
    outlines: HashMap<PathBuf, Vec<(usize, String, String)>>,
    rows: Vec<Row>,
    sidebar: bool,
    /// A dirty tab whose close was requested once; the next close discards it.
    close_armed: Option<PathBuf>,
    /// Read-only editors (selectable, Cmd+F, folding): body with tree-sitter highlighting,
    /// headers/request with the `.http` highlighter.
    resp_body: Entity<EditorState>,
    resp_meta: Entity<EditorState>,
    tab: usize,
    /// (exchange pointer, tab) currently loaded into the response editors.
    shown: Option<(usize, usize)>,
}

impl Respite {
    fn new(root: PathBuf, open: Option<PathBuf>, env_name: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut projects = workspace::projects(Store::default_base().as_deref(), &root);
        let files = workspace::scan(&root);
        let first = files.first().cloned();
        projects[0].files = Some(files);
        let readonly = |http: bool, window: &mut Window, cx: &mut App| {
            cx.new(|cx| {
                // Folding only for bodies (JSON nodes); in headers/request it would fold the whole text.
                let mut s = EditorState::new(window, cx).line_number(false).soft_wrap(true).folding(!http);
                if http {
                    s.set_highlighter(SharedString::from("http"), cx);
                    s.set_highlighter_factory(Rc::new(|_: &str| Some(Box::new(highlight::HttpHighlighter::default()) as _)), cx);
                }
                s.set_readonly(true, cx);
                s
            })
        };
        let mut this = Self {
            env_name,
            executors: HashMap::new(),
            docs: vec![],
            current: None,
            projects,
            expanded: HashSet::from([root.clone()]),
            outlines: HashMap::new(),
            rows: vec![],
            sidebar: true,
            close_armed: None,
            resp_body: readonly(false, window, cx),
            resp_meta: readonly(true, window, cx),
            tab: 0,
            shown: None,
        };
        match open.or(first) {
            Some(path) => this.open(path, None, window, cx),
            None => this.open(root.join("untitled.http"), Some(SAMPLE.into()), window, cx),
        };
        this
    }

    fn executor(&mut self, root: &Path) -> Arc<Executor> {
        let env = self.env_name.clone();
        self.executors
            .entry(root.to_path_buf())
            .or_insert_with(|| {
                let store = Store::default_base().and_then(|base| {
                    Store::open(&base, root).inspect_err(|e| eprintln!("respite: history disabled: {e}")).ok()
                });
                Arc::new(Executor::new(store, env.as_deref(), "gui"))
            })
            .clone()
    }

    /// Switch to `path`'s tab, opening it first if needed (`text` overrides the file content).
    fn open(&mut self, path: PathBuf, text: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        let ix = match self.docs.iter().position(|d| *d.path == path) {
            Some(ix) => ix,
            None => {
                let text = match text.map(Ok).unwrap_or_else(|| std::fs::read_to_string(&path)) {
                    Ok(t) => t,
                    Err(e) => {
                        window.push_notification(format!("{}: {e}", path.display()), cx);
                        return None;
                    }
                };
                let root = http_env::workspace_root(&path);
                // ponytail: env loaded when the file opens; reload on env-file change comes with `notify` watching.
                let env = Environments::load(&path, &root, &[]).variables(self.env_name.as_deref());
                let exec = self.executor(&root);
                let editor = cx.new(|cx| {
                    let mut s = EditorState::new(window, cx).language("http").line_number(true).default_value(text);
                    s.set_highlighter_factory(Rc::new(|_: &str| Some(Box::new(highlight::HttpHighlighter::default()) as _)), cx);
                    s.set_gutter_decorations_width(px(3.), cx);
                    s
                });
                let subs = vec![
                    cx.subscribe(&editor, |this, editor, ev: &InputEvent, cx| {
                        if let (InputEvent::Change, Some(d)) = (ev, this.docs.iter_mut().find(|d| d.editor == editor)) {
                            d.dirty = true;
                            d.reparse(cx);
                        }
                    }),
                    // Cursor moves and scrolling only notify; re-render overlays and the active block.
                    cx.observe(&editor, |this, editor, cx| {
                        if let Some(d) = this.docs.iter_mut().find(|d| d.editor == editor) {
                            d.sync_active(cx);
                        }
                        cx.notify();
                    }),
                ];
                self.docs.push(Doc::new(path.clone(), editor, env, exec, subs, cx));
                self.docs.len() - 1
            }
        };
        self.current = Some(ix);
        self.expanded.insert(path);
        self.docs[ix].editor.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
        Some(ix)
    }

    fn doc(&self) -> Option<&Doc> {
        self.current.map(|ix| &self.docs[ix])
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.docs.len() {
            self.current = Some(ix);
            self.docs[ix].editor.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.current.map(|ix| &mut self.docs[ix]) else { return };
        let text = d.editor.read(cx).value();
        match std::fs::write(&*d.path, text.as_bytes()) {
            Ok(()) => {
                d.dirty = false;
                let path = (*d.path).clone();
                self.outlines.remove(&path);
                // A new file (e.g. the untitled sample) shows up in its project.
                for p in self.projects.iter_mut().filter(|p| p.files.is_some() && path.starts_with(&p.root)) {
                    p.files = Some(workspace::scan(&p.root));
                }
            }
            Err(e) => window.push_notification(format!("Save failed: {}: {e}", d.path.display()), cx),
        }
        cx.notify();
    }

    /// A dirty tab needs a second close (or a save) — no silent data loss.
    fn close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.docs.get(ix) else { return };
        if d.dirty && self.close_armed.as_deref() != Some(d.path.as_path()) {
            self.close_armed = Some((*d.path).clone());
            let msg = format!("Unsaved changes in {}. Cmd+S saves, closing again discards them.", d.name());
            window.push_notification(msg, cx);
            return;
        }
        self.close_armed = None;
        let path = (*self.docs.remove(ix).path).clone();
        self.outlines.remove(&path);
        self.current = match self.docs.len() {
            0 => None,
            n => Some(ix.min(n - 1)),
        };
        if let Some(ix) = self.current {
            self.select(ix, window, cx);
        }
        cx.notify();
    }

    fn go_to(&mut self, path: PathBuf, line: usize, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.open(path, None, window, cx) else { return };
        self.docs[ix].editor.update(cx, |s, cx| {
            s.set_cursor_position(Position::new(line.saturating_sub(1) as u32, 0), window, cx);
            s.focus(window, cx);
        });
        if run {
            self.run(ix, line, cx);
        }
    }

    fn run(&mut self, ix: usize, request_line: usize, cx: &mut Context<Self>) {
        let d = &mut self.docs[ix];
        let Some(index) = d.file.requests.iter().position(|r| r.request_line == request_line) else { return };
        let (exec, file, path, env) = (d.exec.clone(), d.file.clone(), d.path.clone(), d.env.clone());
        let key = path.clone();
        let handle = RT.spawn(async move {
            // ponytail: no @prompt dialog yet, so prompt variables come out undefined.
            let overrides = HashMap::new();
            let cx = http_exec::Context { path: &path, file: &file, env: &env, overrides: &overrides };
            exec.run(&cx, index).await
        });
        let abort = AbortOnDrop(handle.abort_handle());
        d.status.insert(request_line, Status::Running);
        d.tasks.insert(
            request_line,
            cx.spawn(async move |this, cx| {
                let _abort = abort;
                let status = match handle.await {
                    Ok(Ok(ex)) => Status::Done(Arc::new(ex)),
                    Ok(Err(e)) => Status::Failed(e.to_string()),
                    Err(e) => Status::Failed(e.to_string()),
                };
                this.update(cx, |this, cx| {
                    if let Some(d) = this.docs.iter_mut().find(|d| d.path == key) {
                        d.status.insert(request_line, status);
                    }
                    cx.notify();
                })
                .ok();
            }),
        );
        cx.notify();
    }

    fn cancel(&mut self, ix: usize, request_line: usize, cx: &mut Context<Self>) {
        if let Some(d) = self.docs.get_mut(ix) {
            d.tasks.remove(&request_line); // dropping the task cancels it
            d.status.remove(&request_line);
        }
        cx.notify();
    }

    /// Cmd+V: a curl command becomes a request block, anything else pastes as usual.
    // ponytail: one Cmd+Z undoes the whole conversion (spec wants substitutions undone first).
    fn smart_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text());
        // Only the request editor converts; the read-only response editors ignore Paste anyway.
        let in_editor = self.doc().is_some_and(|d| d.editor.focus_handle(cx).is_focused(window));
        if !(in_editor && text.is_some_and(|t| http_curl::is_curl(&t) && self.insert_curl(&t, window, cx))) {
            window.dispatch_action(Box::new(gpui_component::input::Paste), cx);
        }
    }

    fn paste_curl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
        if !self.insert_curl(&text, window, cx) {
            window.push_notification("No cURL command in the clipboard", cx);
        }
    }

    /// Every curl command in `text` → blocks, inserted after the block under the cursor
    /// (or at the cursor outside blocks). Known variable values become `{{name}}`.
    fn insert_curl(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(d) = self.doc() else { return false };
        let dir = d.path.parent().unwrap_or(Path::new("."));
        let vars = http_env::known_values(&d.file, &d.env, dir);
        let opts = http_curl::Options { vars: &vars, ..Default::default() };
        let Ok(blocks) = http_curl::import_all(text, &opts) else { return false };
        let blocks = blocks.join("\n");
        let block_end = d.cursor_request(cx).map(|r| *r.lines.end() - 1);
        d.editor.clone().update(cx, |s, cx| {
            let insert = match block_end {
                Some(row) => {
                    let (start, end) = (s.text().line_start_offset(row), s.text().line_end_offset(row));
                    let blank = s.text().slice(start..end).to_string().trim().is_empty();
                    let pos = s.text().offset_to_position(end);
                    s.set_cursor_position(pos, window, cx);
                    // Trailing newline keeps a blank line before the next `###`.
                    format!("{}{}\n", if blank { "\n" } else { "\n\n" }, blocks.trim_end())
                }
                None => blocks,
            };
            s.insert(insert, window, cx);
        });
        true
    }

    /// Cmd+Shift+C: variables substituted; falls back to the raw block when a referenced
    /// response is not cached yet.
    fn copy_as_curl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc() else { return };
        let Some(index) = d.cursor_request(cx).and_then(|r| d.file.requests.iter().position(|x| x.request_line == r.request_line))
        else {
            return;
        };
        let req = &d.file.requests[index];
        let overrides = HashMap::new();
        let ctx = http_exec::Context { path: &d.path, file: &d.file, env: &d.env, overrides: &overrides };
        let (cmd, note) = match d.exec.prepare(&ctx, index) {
            Ok(sent) => (http_curl::export(&sent, req), "Copied as cURL".to_string()),
            Err(e) => (http_curl::export_raw(req), format!("Copied as cURL without substitution: {e}")),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(cmd));
        window.push_notification(note, cx);
    }

    fn status_color(&self, code: u16, cx: &App) -> Hsla {
        match code {
            200..=299 => cx.theme().success,
            300..=499 => cx.theme().warning,
            _ => cx.theme().danger,
        }
    }

    // ---- sidebar ----

    fn toggle(&mut self, key: &Path) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_path_buf());
        }
    }

    fn toggle_project(&mut self, pi: usize, cx: &mut Context<Self>) {
        let p = &mut self.projects[pi];
        if p.files.is_none() || !self.expanded.contains(&p.root) {
            p.files = Some(workspace::scan(&p.root)); // rescan on expand: picks up new files
        }
        let root = p.root.clone();
        self.toggle(&root);
        cx.notify();
    }

    /// Flattened tree: projects → files → requests (open files show their live parse).
    fn build_rows(&mut self) -> Vec<Row> {
        let mut rows = vec![];
        for (pi, p) in self.projects.iter().enumerate() {
            rows.push(Row::Project(pi));
            if !self.expanded.contains(&p.root) {
                continue;
            }
            let files = p.files.as_deref().unwrap_or_default();
            if files.is_empty() {
                rows.push(Row::Note("No .http or .rest files"));
            }
            for f in files {
                let rel = f.strip_prefix(&p.root).unwrap_or(f).to_string_lossy().into_owned();
                rows.push(Row::File { path: f.clone(), rel });
                if !self.expanded.contains(f) {
                    continue;
                }
                let outline: Vec<(usize, String, String)> = match self.docs.iter().find(|d| *d.path == *f) {
                    Some(d) => outline(&d.file),
                    None => self
                        .outlines
                        .entry(f.clone())
                        .or_insert_with(|| outline(&http_syntax::parse(&std::fs::read_to_string(f).unwrap_or_default())))
                        .clone(),
                };
                for (line, method, label) in outline {
                    rows.push(Row::Request { path: f.clone(), line, method, label });
                }
            }
        }
        rows
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover) = (theme.muted_foreground, theme.accent);
        let chevron = |open: bool| div().w(px(12.)).text_color(muted).child(if open { "▾" } else { "▸" });
        let row = div().id(("row", ix)).h(px(24.)).w_full().flex().items_center().gap_1().px_2().text_sm().cursor_pointer().hover(move |s| s.bg(hover));
        match &self.rows[ix] {
            Row::Project(pi) => {
                let (p, pi) = (&self.projects[*pi], *pi);
                row.child(chevron(self.expanded.contains(&p.root)))
                    .child(div().font_weight(FontWeight::BOLD).truncate().child(p.name.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_project(pi, cx)))
                    .into_any_element()
            }
            Row::File { path, rel } => {
                let doc = self.docs.iter().position(|d| *d.path == *path);
                let current = doc.is_some() && doc == self.current;
                let dirty = doc.is_some_and(|i| self.docs[i].dirty);
                let p = path.clone();
                row.pl(px(18.))
                    .when(current, |r| r.bg(theme.accent))
                    .child(chevron(self.expanded.contains(path)))
                    .child(div().flex_1().truncate().child(rel.clone()))
                    .when(dirty, |r| r.child(div().text_color(muted).child("●")))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let is_current = this.doc().is_some_and(|d| *d.path == p);
                        if is_current {
                            this.toggle(&p);
                        } else if this.open(p.clone(), None, window, cx).is_some() {
                            this.expanded.insert(p.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Row::Request { path, line, method, label } => {
                let status = self.docs.iter().find(|d| *d.path == *path).and_then(|d| d.status.get(line).cloned());
                let status = match status {
                    Some(Status::Done(ex)) => Some((ex.response.status.to_string(), self.status_color(ex.response.status, cx))),
                    Some(Status::Failed(_)) => Some(("✕".into(), theme.danger)),
                    Some(Status::Running) => Some(("…".into(), theme.warning)),
                    None => None,
                };
                let (p, line) = (path.clone(), *line);
                row.pl(px(36.))
                    .child(div().w(px(52.)).flex_none().text_xs().font_weight(FontWeight::BOLD).text_color(method_color(method, cx)).child(method.clone()))
                    .child(div().flex_1().truncate().child(label.clone()))
                    .children(status.map(|(s, c)| div().text_xs().text_color(c).child(s)))
                    // Click jumps to the request; double click also runs it.
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        this.go_to(p.clone(), line, ev.click_count() >= 2, window, cx)
                    }))
                    .into_any_element()
            }
            Row::Note(text) => row.pl(px(18.)).text_color(muted).child(*text).into_any_element(),
        }
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.rows = self.build_rows();
        let list = uniform_list("sidebar", self.rows.len(), cx.processor(|this, range: Range<usize>, _, cx| {
            range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
        }));
        div().size_full().py_1().bg(cx.theme().sidebar).child(list.size_full()).into_any_element()
    }

    // ---- center ----

    fn tabs(&self, cx: &mut Context<Self>) -> TabBar {
        let muted = cx.theme().muted_foreground;
        self.docs.iter().enumerate().fold(
            TabBar::new("docs").selected_index(self.current.unwrap_or(0)).on_click(cx.listener(|this, ix: &usize, window, cx| this.select(*ix, window, cx))),
            |bar, (i, d)| {
                let close = div()
                    .id(("close", i))
                    .px_1()
                    .text_color(muted)
                    .hover(|s| s.text_color(gpui::white()))
                    .child(if d.dirty { "●" } else { "×" })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close(i, window, cx);
                    }));
                bar.child(Tab::new().label(d.name()).suffix(close))
            },
        )
    }

    fn center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let body = match self.doc() {
            Some(d) => div().flex_1().min_h_0().child(Editor::new(&d.editor).size_full()),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .child("Open a file from the sidebar"),
        };
        let tabs = (!self.docs.is_empty()).then(|| self.tabs(cx));
        div().size_full().flex().flex_col().children(tabs).child(body).into_any_element()
    }

    fn overlays(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let (Some(ix), Some(d)) = (self.current, self.doc()) else { return vec![] };
        let s = d.editor.read(cx);
        let (text, bounds) = (s.text(), s.input_bounds());
        let mut out = vec![];
        for r in &d.file.requests {
            let line = r.request_line;
            let (start, end) = (text.line_start_offset(line - 1), text.line_end_offset(line - 1));
            let Some(lb) = s.range_to_bounds(&(start..start)) else { continue };
            if lb.top() < bounds.top() || lb.bottom() > bounds.bottom() {
                continue; // scrolled out or folded
            }
            let status = d.status.get(&line).cloned();
            let running = matches!(status, Some(Status::Running));
            let button = div()
                .id(("run", line))
                .cursor_pointer()
                .text_sm()
                .text_color(if running { cx.theme().warning } else { cx.theme().success })
                .child(if running { "◼" } else { "▶" })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if running { this.cancel(ix, line, cx) } else { this.run(ix, line, cx) }
                }));
            // Between line numbers and text (fold chevrons sit on `###` lines, not request lines).
            // ponytail: follows horizontal scroll; pin to text_bounds().left() if that bothers.
            out.push(deferred(anchored().position(point(lb.left() - px(16.), lb.top())).child(button)).into_any_element());

            let hint = match &status {
                Some(Status::Done(ex)) => Some((summary(ex), self.status_color(ex.response.status, cx))),
                Some(Status::Failed(e)) => Some((format!("✕ {}", truncate(e, 80)), cx.theme().danger)),
                _ => None,
            };
            if let (Some((text, color)), Some(eb)) = (hint, s.range_to_bounds(&(end..end))) {
                let hint = div().text_sm().text_color(color).child(text);
                out.push(deferred(anchored().position(point(eb.right() + px(24.), eb.top())).child(hint)).into_any_element());
            }
        }
        out
    }

    // ---- response ----

    fn response_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let d = self.doc();
        let req = d.and_then(|d| d.active.and_then(|l| d.file.requests.iter().find(|r| r.request_line == l)));
        let muted = cx.theme().muted_foreground;
        let title = req.map_or("—".into(), |r| {
            let label = r.label();
            // Imported blocks are titled "METHOD /path" already.
            if label.starts_with(&format!("{} ", r.method)) { label } else { format!("{} · {label}", r.method) }
        });
        let env = format!("Environment: {}", self.env_name.as_deref().unwrap_or("No environment"));
        let panel = div()
            .size_full()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .bg(cx.theme().background)
            .child(div().font_weight(FontWeight::BOLD).child(title))
            .child(div().text_xs().text_color(muted).child(env));
        match req.and_then(|r| d?.status.get(&r.request_line)) {
            None => panel.child(div().text_color(muted).child("Cmd+Enter to send")),
            Some(Status::Running) => panel.child(div().text_color(cx.theme().warning).child("Running… (click ◼ to cancel)")),
            Some(Status::Failed(e)) => panel.child(div().text_color(cx.theme().danger).child(e.clone())),
            Some(Status::Done(ex)) => {
                let res = &ex.response;
                let deps = (!ex.dependencies.is_empty())
                    .then(|| div().text_xs().text_color(muted).child(format!("Ran first: {}", ex.dependencies.join(", "))));
                let tabs = TABS.iter().fold(
                    TabBar::new("response-tabs").underline().selected_index(self.tab).on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.tab = *ix;
                        cx.notify();
                    })),
                    |bar, t| bar.child(Tab::new().label(*t)),
                );
                let view = if self.tab == 0 { &self.resp_body } else { &self.resp_meta };
                panel
                    .child(div().text_color(self.status_color(res.status, cx)).child(format!("{} · {}", summary(ex), res.http_version)))
                    .children(deps)
                    .child(tabs)
                    .child(div().flex_1().min_h_0().child(Editor::new(view).size_full().bordered(false)))
            }
        }
    }

    /// Load the active block's exchange into the response editors (only when it or the tab changed:
    /// `set_value` resets selection and scroll).
    fn sync_response(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ex = self.doc().and_then(|d| match d.status.get(&d.active?) {
            Some(Status::Done(ex)) => Some(ex.clone()),
            _ => None,
        });
        let Some(ex) = ex else {
            self.shown = None;
            return;
        };
        let key = (Arc::as_ptr(&ex) as usize, self.tab);
        if self.shown == Some(key) {
            return;
        }
        if self.shown.map(|k| k.0) != Some(key.0) {
            let (lang, text) = (body_language(&ex.response), pretty_body(&ex.response.body));
            self.resp_body.update(cx, |s, cx| {
                s.set_highlighter(SharedString::from(lang), cx);
                s.set_value(text, window, cx);
            });
        }
        self.shown = Some(key);
        let meta = match self.tab {
            1 => {
                let r = &ex.response;
                let headers: String = r.headers.iter().map(|(n, v)| format!("{n}: {v}\n")).collect();
                format!("{} {} {}\n{headers}", r.http_version, r.status, r.reason)
            }
            2 => {
                let q = &ex.request;
                let headers: String = q.headers.iter().map(|(n, v)| format!("{n}: {v}\n")).collect();
                let body = String::from_utf8_lossy(&q.body);
                let body = if body.is_empty() { String::new() } else { format!("\n{body}\n") };
                format!("{} {}\n{headers}{body}", q.method, q.url)
            }
            _ => return,
        };
        self.resp_meta.update(cx, |s, cx| s.set_value(meta, window, cx));
    }
}

/// `(request_line, method, label without a leading "METHOD ")` per request.
fn outline(file: &http_syntax::HttpFile) -> Vec<(usize, String, String)> {
    file.requests
        .iter()
        .map(|r| {
            let label = r.label();
            let label = label.strip_prefix(&format!("{} ", r.method)).map(str::to_string).unwrap_or(label);
            (r.request_line, r.method.clone(), label)
        })
        .collect()
}

fn method_color(method: &str, cx: &App) -> Hsla {
    match method {
        "GET" | "HEAD" | "OPTIONS" => cx.theme().success,
        "POST" => cx.theme().warning,
        "DELETE" => cx.theme().danger,
        _ => cx.theme().primary,
    }
}

/// Highlighter language for a response body.
fn body_language(r: &http_exec::Response) -> &'static str {
    let ct = r.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.to_ascii_lowercase());
    match ct.as_deref() {
        Some(c) if c.contains("json") => "json",
        Some(c) if c.contains("html") || c.contains("xml") => "html",
        _ if serde_json::from_slice::<serde_json::Value>(&r.body).is_ok() => "json",
        _ => "text",
    }
}

/// `200 OK · 142 ms · 3.4 KB`
fn summary(ex: &Exchange) -> String {
    let r = &ex.response;
    let size = match r.body.len() {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.),
        n => format!("{:.1} MB", n as f64 / 1024. / 1024.),
    };
    format!("{} {} · {} ms · {size}", r.status, r.reason, r.total.as_millis())
}

/// JSON pretty-printed, else lossy UTF-8; capped so a huge body can't freeze the layout.
fn pretty_body(body: &[u8]) -> String {
    const CAP: usize = 256 * 1024;
    let text = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    match text.len() > CAP {
        true => format!("{}\n… (truncated, {} B)", truncate(&text, CAP), text.len()),
        false => text,
    }
}

fn truncate(s: &str, max: usize) -> &str {
    let end = (0..=max.min(s.len())).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    &s[..end]
}

impl Render for Respite {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_response(window, cx);
        let sidebar = self.sidebar(cx);
        let center = self.center(cx);
        div()
            .key_context("Respite")
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &RunRequest, _, cx| {
                if let (Some(ix), Some(line)) = (this.current, this.doc().and_then(|d| d.cursor_request(cx)).map(|r| r.request_line)) {
                    this.run(ix, line, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SmartPaste, window, cx| this.smart_paste(window, cx)))
            .on_action(cx.listener(|this, _: &PasteCurl, window, cx| this.paste_curl(window, cx)))
            .on_action(cx.listener(|this, _: &CopyAsCurl, window, cx| this.copy_as_curl(window, cx)))
            .on_action(cx.listener(|this, _: &SaveFile, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if let Some(ix) = this.current {
                    this.close(ix, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar = !this.sidebar;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                if let Some(ix) = this.current {
                    this.select((ix + 1) % this.docs.len(), window, cx);
                }
            }))
            .child(
                h_resizable("main")
                    .child(resizable_panel().size(px(260.)).size_range(px(160.)..px(520.)).visible(self.sidebar).child(sidebar))
                    .child(resizable_panel().child(center))
                    .child(resizable_panel().size(px(420.)).child(self.response_panel(cx))),
            )
            .children(self.overlays(cx))
    }
}

/// `respite <command> …` runs the CLI without starting the GUI;
/// `respite [open] [--env NAME] [file.http | dir]` opens the GUI.
fn main() -> std::process::ExitCode {
    if let Some(code) = respite_cli::main_if_cli() {
        return code;
    }
    // ponytail: hand-rolled GUI args; fold into clap when `open` grows options.
    let mut args = std::env::args().skip(1).filter(|a| a != "open");
    let (mut env_name, mut target) = (None, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--env" => env_name = args.next(),
            _ => target = Some(a),
        }
    }
    let cwd = std::env::current_dir().expect("current dir");
    let (root, open) = match target.map(|t| std::path::absolute(&t).unwrap_or_else(|_| cwd.join(&t))) {
        Some(p) if p.is_dir() => (p, None),
        Some(p) => (http_env::workspace_root(&p), Some(p)),
        None => (cwd, None),
    };
    gpui_platform::application().with_assets(gpui_kit_assets::Assets).run(move |cx: &mut App| {
        gpui_component::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        // `secondary-` is not supported by this GPUI version.
        let m = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };
        let key = |k: &str| -> &'static str { format!("{m}-{k}").leak() };
        // Editor keys go in its own "Input" context (later bindings win over its cmd-enter / cmd-v).
        cx.bind_keys([
            KeyBinding::new(key("enter"), RunRequest, Some("Input")),
            KeyBinding::new(key("v"), SmartPaste, Some("Input")),
            KeyBinding::new(key("shift-v"), gpui_component::input::Paste, Some("Input")),
            KeyBinding::new(key("alt-v"), PasteCurl, Some("Input")),
            KeyBinding::new(key("shift-c"), CopyAsCurl, Some("Input")),
        ]);
        for ctx in ["Respite", "Input"] {
            cx.bind_keys([
                KeyBinding::new(key("s"), SaveFile, Some(ctx)),
                KeyBinding::new(key("w"), CloseTab, Some(ctx)),
                KeyBinding::new(key("b"), ToggleSidebar, Some(ctx)),
                KeyBinding::new("ctrl-tab", NextTab, Some(ctx)),
            ]);
        }
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(880.)), cx)),
            titlebar: Some(TitlebarOptions { title: Some("Respite".into()), ..Default::default() }),
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| Respite::new(root, open, env_name, window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        })
        .unwrap();
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
    std::process::ExitCode::SUCCESS
}
