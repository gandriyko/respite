//! Respite GUI: sidebar (projects → .http files → requests), tabs of open files,
//! editor with ▶ / status overlays, response panel.
//! ▶ and the status inlay are overlays positioned from the editor's last layout
//! (gpui-component has no custom gutter elements or inlay hints).

mod attach;
mod doc;
mod highlight;
mod history_page;
mod lang;
mod palette;
mod runs;
mod settings_page;
mod sidebar;
mod themes;
mod workspace;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, LazyLock},
};

use doc::{Doc, Status};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Icon, Root, Theme, ThemeMode, WindowExt as _, h_flex, h_resizable,
    input::{Editor, EditorState, InputEvent, InputState, Position, RopeExt as _},
    IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    menu::{DropdownMenu as _, PopupMenuItem},
    resizable_panel,
    tab::{Tab, TabBar},
};
use http_env::Environments;
use http_exec::{Exchange, Executor, Store};
use workspace::Project;

actions!(
    respite,
    [
        RunRequest, SmartPaste, PasteCurl, CopyAsCurl, SaveFile, CloseTab, ToggleSidebar, NextTab, OpenSettings, AddProject, Import, Quit,
        NextEnvironment, ToggleHistory, InsertFile, RunAll, RunSelected, RepeatLast, CancelRequests, GoToFile, GoToRequest,
        CommandPalette, PrevRequest, NextRequest
    ]
);

const SAMPLE: &str = include_str!("../sample.http");

/// Starting point for "Create …" in the environment menu (only the Respite format is offered).
/// The file to edit for environment `name` (`None` = Shared, `respite.json`): public before
/// private, the one deepest in the chain (closest to the `.http` file, it wins the merge).
fn env_file(paths: &[PathBuf], name: Option<&str>) -> Option<PathBuf> {
    let stem = name.map_or_else(|| "respite".to_string(), |n| format!("respite-{n}"));
    [format!("{stem}.json"), format!("{stem}.private.json")].iter().find_map(|f| {
        paths.iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy() == *f)).max_by_key(|p| p.components().count()).cloned()
    })
}

/// The env picker's "no named environment" entry: only `respite.json` (shared values) applies.
const SHARED: &str = "Shared";
const RESPITE_TEMPLATE: &str = "{\n  \"baseUrl\": \"http://localhost:8080\"\n}\n";
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
    rows: Vec<sidebar::Row>,
    /// Folders are open by default; this holds the collapsed ones.
    collapsed_dirs: HashSet<PathBuf>,
    sidebar: bool,
    /// A dirty tab whose close was requested once; the next close discards it.
    close_armed: Option<PathBuf>,
    /// What Cmd+Alt+L sends again: the file and its request lines.
    last_run: Option<(PathBuf, Vec<usize>)>,
    /// The running "run all / selected" sequence; dropping it stops the rest.
    batch: Option<Task<()>>,
    /// Re-renders the open search dialog (Cmd+P …) as its query changes.
    picker_sub: Option<Subscription>,
    /// Read-only editors (selectable, Cmd+F, folding): body with tree-sitter highlighting,
    /// headers/request with the `.http` highlighter.
    resp_body: Entity<EditorState>,
    resp_meta: Entity<EditorState>,
    tab: usize,
    /// (exchange pointer, tab) currently loaded into the response editors.
    shown: Option<(usize, usize)>,
    settings: respite_settings::Loaded,
    settings_window: Option<WindowHandle<Root>>,
    show_history: bool,
    /// Current project's history, newest first; reloaded when the page opens and after runs.
    history: Vec<http_exec::Entry>,
    /// Indices into `history` that match the filter (the list's rows).
    history_rows: Vec<usize>,
    history_pick: Option<(http_exec::Entry, Arc<Exchange>)>,
    history_filter: Entity<InputState>,
    /// Theme's own monospace font, used when `font_family` is empty.
    default_mono: SharedString,
    _subs: Vec<Subscription>,
}

impl Respite {
    fn new(root: PathBuf, open: Option<PathBuf>, env_name: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // `--env` wins; otherwise the environment last picked for this project.
        let env_name = env_name.or_else(|| workspace::load_env(Store::default_base().as_deref(), &root));
        let mut projects = workspace::projects(Store::default_base().as_deref(), &workspace::collections_dir(), &root);
        let current = projects.iter_mut().find(|p| p.root == root).expect("current project is listed");
        let files = current.files.get_or_insert_with(|| workspace::scan(&root));
        let first = files.iter().find(|f| !workspace::is_env_file(f)).cloned();
        let readonly = |http: bool, window: &mut Window, cx: &mut App| {
            cx.new(|cx| {
                // Folding only for bodies (JSON nodes); in headers/request it would fold the whole text.
                let mut s = EditorState::new(window, cx)
                    .line_number(false)
                    .soft_wrap(true)
                    .folding(!http)
                    .searchable(true) // Cmd+F; off by default in gpui-component
                    .replaceable(false);
                if http {
                    s.set_highlighter(SharedString::from("http"), cx);
                    s.set_highlighter_factory(Rc::new(|_: &str| Some(Box::new(highlight::HttpHighlighter::default()) as _)), cx);
                }
                s.set_readonly(true, cx);
                s
            })
        };
        let settings = respite_settings::Loaded::load(Some(&root));
        for e in &settings.errors {
            window.push_notification(format!("Settings: {e}"), cx);
        }
        let history_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter: method, URL, name, file, status"));
        // The close button: with unsaved files, ask (as Quit does) instead of closing.
        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            view.update(cx, |this, cx| {
                let dirty = this.docs.iter().any(|d| d.dirty);
                if dirty {
                    this.confirm_quit(window, cx);
                }
                !dirty
            })
            .unwrap_or(true)
        });
        let _subs = vec![
            cx.subscribe_in(&history_filter, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            // Remember the current file and cursor for the next launch of this project.
            cx.on_app_quit(|this, cx| {
                this.remember_position(cx);
                async {}
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.settings.settings.appearance == respite_settings::Appearance::System {
                    Theme::sync_system_appearance(Some(window), cx);
                    // Switching mode reloads the highlight theme; re-apply the option.
                    themes::current_line(this.settings.settings.editor.highlight_current_line, cx);
                    themes::sync_text_color(cx);
                }
            }),
        ];
        let mut this = Self {
            settings,
            settings_window: None,
            show_history: false,
            history: vec![],
            history_rows: vec![],
            history_pick: None,
            history_filter,
            default_mono: cx.theme().mono_font_family.clone(),
            _subs,
            env_name,
            executors: HashMap::new(),
            docs: vec![],
            current: None,
            projects,
            expanded: HashSet::from([root.clone()]),
            outlines: HashMap::new(),
            collapsed_dirs: HashSet::new(),
            rows: vec![],
            sidebar: true,
            close_armed: None,
            last_run: None,
            batch: None,
            picker_sub: None,
            resp_body: readonly(false, window, cx),
            resp_meta: readonly(true, window, cx),
            tab: 0,
            shown: None,
        };
        // An explicit file wins; else the file (and cursor) last used in this project — or in any
        // project, on a plain launch (no folder given); else the first one.
        let base = Store::default_base();
        let last = match root == workspace::collections_dir() {
            true => workspace::last_opened(base.as_deref()),
            false => workspace::last_file(base.as_deref(), &root),
        };
        match (open, last, first) {
            (Some(path), _, _) => {
                this.open(path, None, window, cx);
            }
            (None, Some((path, line, column)), _) => {
                if let Some(ix) = this.open(path, None, window, cx) {
                    this.docs[ix].editor.update(cx, |s, cx| s.set_cursor_position(Position::new(line, column), window, cx));
                }
            }
            (None, None, Some(path)) => {
                this.open(path, None, window, cx);
            }
            (None, None, None) => {
                this.open(root.join("untitled.http"), Some(SAMPLE.into()), window, cx);
            }
        }
        this.apply_settings(window, cx);
        this.watch_settings(window, cx);
        this
    }

    /// Re-read the settings files when they change on disk (edited by hand, another window).
    // ponytail: polls mtimes every second; switch to `notify` when file watching lands for .http/env files.
    fn watch_settings(&self, window: &mut Window, cx: &mut Context<Self>) {
        let stamp = |l: &respite_settings::Loaded| {
            [&l.global_path, &l.workspace_path].map(|p| p.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()))
        };
        let mut last = stamp(&self.settings);
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    let now = stamp(&this.settings);
                    if now == last {
                        return;
                    }
                    last = now;
                    let fresh = respite_settings::Loaded::load_from(this.settings.global_path.clone(), this.settings.workspace_path.clone());
                    // A half-written or broken file keeps the current settings.
                    if !fresh.errors.is_empty() {
                        for e in &fresh.errors {
                            window.push_notification(format!("Settings: {e}"), cx);
                        }
                        return;
                    }
                    this.settings = fresh;
                    this.apply_settings(window, cx);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn executor(&mut self, root: &Path) -> Arc<Executor> {
        let (env, s) = (self.env_name.clone(), self.settings.settings.clone());
        self.executors
            .entry(root.to_path_buf())
            .or_insert_with(|| {
                let store = Store::default_base().and_then(|base| {
                    let mut st = Store::open(&base, root).inspect_err(|e| eprintln!("respite: history disabled: {e}")).ok()?;
                    // ponytail: history limits apply to executors created after a change.
                    (st.per_request, st.total) = (s.history.per_request, s.history.total);
                    Some(st)
                });
                let exec = Executor::new(store, env.as_deref(), "gui");
                exec.set_options(respite_cli::exec_options(&s));
                Arc::new(exec)
            })
            .clone()
    }

    /// Write one setting to the global file and apply everything live.
    fn set(&mut self, key: &str, value: serde_json::Value, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.settings.set(key, value) {
            window.push_notification(format!("Settings not saved: {e}"), cx);
        }
        self.apply_settings(window, cx);
        cx.notify();
    }

    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use respite_settings::Appearance as A;
        let s = self.settings.settings.clone();
        themes::select(&s.theme, cx);
        match s.appearance {
            A::System => Theme::sync_system_appearance(Some(window), cx),
            A::Light => Theme::change(ThemeMode::Light, Some(window), cx),
            A::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
        }
        themes::current_line(s.editor.highlight_current_line, cx);
        themes::sync_text_color(cx);
        let t = Theme::global_mut(cx);
        t.font_size = px(s.ui_font_size);
        t.mono_font_size = px(s.font_size);
        t.mono_font_family = if s.font_family.is_empty() { self.default_mono.clone() } else { s.font_family.clone().into() };
        for d in &self.docs {
            d.editor.update(cx, |e, cx| {
                e.set_line_number(s.editor.line_numbers, window, cx);
                e.set_soft_wrap(s.editor.soft_wrap, window, cx);
            });
        }
        for exec in self.executors.values() {
            exec.set_options(respite_cli::exec_options(&s));
        }
        for view in [&self.resp_body, &self.resp_meta] {
            view.update(cx, |e, cx| e.set_soft_wrap(s.response.soft_wrap, window, cx));
        }
        cx.refresh_windows();
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
                let root = self.root_of(&path);
                // ponytail: env loaded when the file opens; reload on env-file change comes with `notify` watching.
                let env = self.env_for(&path, &root);
                let st = &self.settings.settings;
                let (line_numbers, soft_wrap) = (st.editor.line_numbers, st.editor.soft_wrap);
                let exec = self.executor(&root);
                // Env files are plain JSON (tree-sitter highlighting, no requests).
                let http = !workspace::is_env_file(&path);
                // Folded row → hidden lines, kept current by the doc; read by the fold placeholder.
                let fold_counts = doc::FoldCounts::default();
                let weak = cx.entity().downgrade();
                let editor = cx.new(|cx| {
                    let mut s = EditorState::new(window, cx)
                        .language(if http { "http" } else { "json" })
                        .searchable(true)
                        .line_number(line_numbers)
                        .soft_wrap(soft_wrap)
                        .default_value(text);
                    if http {
                        // Go to definition in another file (an env file): open it there. Same-file
                        // targets return `false`, so the editor selects them itself.
                        let (this, doc_path) = (weak.clone(), path.clone());
                        s.lsp_mut().show_document = Some(Rc::new(move |p, window, cx| {
                            let Some(target) = lang::uri_path(&p.uri).filter(|t| *t != doc_path) else { return false };
                            let line = p.selection.map_or(0, |r| r.start.line as usize) + 1;
                            this.update(cx, |this, cx| this.go_to(target, line, false, window, cx)).is_ok()
                        }));
                        s.set_highlighter_factory(Rc::new(|_: &str| Some(Box::new(highlight::HttpHighlighter::default()) as _)), cx);
                        s.set_gutter_decorations_width(px(3.), cx);
                        // After a folded line (e.g. a long body's preview); clicking it unfolds.
                        // The text colour, for contrast in every theme (`primary` / `muted` are grey in some).
                        // Read at paint time: the first editor is built before the theme is applied.
                        let counts = fold_counts.clone();
                        s.set_fold_placeholder_renderer(
                            Some(Rc::new(move |row| {
                                let accent = themes::text_color();
                                let label = match counts.borrow().get(&row) {
                                    Some(1) => "⋯ 1 more line".to_string(),
                                    Some(n) => format!("⋯ {n} more lines"),
                                    None => "⋯".to_string(),
                                };
                                div()
                                    .ml_2()
                                    .px_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(accent.opacity(0.45))
                                    .bg(accent.opacity(0.1))
                                    .hover(move |s| s.bg(accent.opacity(0.22)))
                                    .text_color(accent)
                                    .text_sm()
                                    .cursor_pointer()
                                    .child(label)
                                    .into_any_element()
                            })),
                            cx,
                        );
                    }
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
                self.docs.push(Doc::new(path.clone(), editor, env, exec, subs, fold_counts, cx));
                if let Some(e) = Environments::load(&path, &root, &[]).errors.first() {
                    window.push_notification(format!("Env file skipped: {}: {}", e.0.display(), e.1), cx);
                }
                self.docs.len() - 1
            }
        };
        self.current = Some(ix);
        self.expanded.insert(path);
        // Now, not only on quit: closing the window or killing the process skips the quit hook.
        self.remember_position(cx);
        self.docs[ix].editor.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
        Some(ix)
    }

    /// Root of `path`'s env chain, history and saved state; the same one the CLI uses.
    fn root_of(&self, path: &Path) -> PathBuf {
        http_env::workspace_root(path)
    }

    fn doc(&self) -> Option<&Doc> {
        self.current.map(|ix| &self.docs[ix])
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.docs.len() {
            self.current = Some(ix);
            self.remember_position(cx);
            self.docs[ix].editor.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.current {
            self.save_doc(ix, window, cx);
        }
    }

    /// Ask before quitting with unsaved files: save them all, discard them, or stay.
    /// Cmd+Q, the Quit menu item and the window's close button all come here.
    fn confirm_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty: Vec<String> = self.docs.iter().filter(|d| d.dirty).map(|d| d.name()).collect();
        if dirty.is_empty() {
            cx.quit();
            return;
        }
        let message = match dirty.len() {
            1 => format!("Save changes to {} before quitting?", dirty[0]),
            n => format!("Save changes to {n} files before quitting?"),
        };
        let detail = (dirty.len() > 1).then(|| dirty.join("\n"));
        let answer = window.prompt(PromptLevel::Warning, &message, detail.as_deref(), &["Save All", "Don't Save", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else { return };
            this.update_in(cx, |this, window, cx| match answer {
                0 => {
                    let all_saved = (0..this.docs.len()).filter(|&ix| this.docs[ix].dirty).collect::<Vec<_>>().into_iter().all(|ix| this.save_doc(ix, window, cx));
                    // A failed save (shown as a notification) keeps the app open.
                    if all_saved {
                        cx.quit();
                    }
                }
                1 => cx.quit(),
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// Write tab `ix` to disk; `false` (with a notification) when that fails.
    fn save_doc(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(d) = self.docs.get_mut(ix) else { return false };
        let text = d.editor.read(cx).value();
        let saved = std::fs::write(&*d.path, text.as_bytes());
        match saved {
            Ok(()) => {
                d.dirty = false;
                let path = (*d.path).clone();
                self.outlines.remove(&path);
                // A new file (e.g. the untitled sample) shows up in its project.
                self.rescan_projects_of(&path);
                if workspace::is_env_file(&path) {
                    // New variables / environments apply to every open request file right away.
                    self.select_env(self.env_name.clone(), cx);
                    let root = self.root_of(&path);
                    if let Some((p, e)) = Environments::load(&path, &root, &[]).errors.into_iter().find(|(p, _)| *p == path) {
                        window.push_notification(format!("{} is not valid JSON, so it is skipped: {e}", p.display()), cx);
                    }
                }
                cx.notify();
                true
            }
            Err(e) => {
                window.push_notification(format!("Save failed: {}: {e}", d.path.display()), cx);
                cx.notify();
                false
            }
        }
    }

    /// Current file + cursor, per project (`state.json` → `files`); reopened on next launch.
    fn remember_position(&self, cx: &App) {
        let Some(d) = self.doc() else { return };
        let pos = d.editor.read(cx).cursor_position();
        let root = self.root_of(&d.path);
        workspace::save_last_file(Store::default_base().as_deref(), &root, &d.path, pos.line, pos.character);
    }

    fn rescan_projects_of(&mut self, path: &Path) {
        for p in self.projects.iter_mut().filter(|p| p.files.is_some() && path.starts_with(&p.root)) {
            p.files = Some(workspace::scan(&p.root));
        }
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
        center_line(self.docs[ix].editor.clone(), line.saturating_sub(1), true, window, cx);
        if run {
            self.send(ix, vec![line], window, cx);
        }
    }

    /// Run one request now (questions were asked by `send`). `overrides`: `@prompt` values and
    /// literal undefined variables; `done` fires when it finishes (dropped if cancelled).
    fn start(
        &mut self,
        ix: usize,
        request_line: usize,
        overrides: HashMap<String, String>,
        done: Option<tokio::sync::oneshot::Sender<()>>,
        cx: &mut Context<Self>,
    ) {
        let d = &mut self.docs[ix];
        let Some(index) = d.file.requests.iter().position(|r| r.request_line == request_line) else { return };
        let (exec, file, path, env) = (d.exec.clone(), d.file.clone(), d.path.clone(), d.env.clone());
        let key = path.clone();
        let handle = RT.spawn(async move {
            let cx = http_exec::Context { path: &path, file: &file, env: &env, overrides: &overrides };
            exec.run(&cx, index).await
        });
        let abort = AbortOnDrop(handle.abort_handle());
        static RUN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = RUN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        d.status.insert(request_line, Status::Running);
        let task = cx.spawn(async move |this, cx| {
            let _abort = abort;
            let status = match handle.await {
                Ok(Ok(ex)) => Status::Done(Arc::new(ex)),
                Ok(Err(e)) => Status::Failed(e.to_string()),
                Err(e) => Status::Failed(e.to_string()),
            };
            this.update(cx, |this, cx| {
                // The block's line now: edits above may have moved it since the run started.
                if let Some(d) = this.docs.iter_mut().find(|d| d.path == key)
                    && let Some(line) = d.tasks.iter().find(|(_, (t, _))| *t == id).map(|(l, _)| *l)
                {
                    d.status.insert(line, status);
                }
                if this.show_history {
                    this.reload_history();
                }
                cx.notify();
            })
            .ok();
            if let Some(done) = done {
                done.send(()).ok();
            }
        });
        d.tasks.insert(request_line, (id, task));
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
        let in_editor = self.settings.settings.curl.auto_convert_on_paste
            && self.doc().is_some_and(|d| d.editor.focus_handle(cx).is_focused(window));
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
        let curl = &self.settings.settings.curl;
        let strip: Vec<&str> = curl.strip_headers.iter().map(String::as_str).collect();
        let opts = http_curl::Options { vars: &vars, keep_original: curl.keep_original_as_comment, strip_headers: &strip, ..Default::default() };
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

    /// Cmd+I: pick files in the system dialog and attach each to the request under the cursor
    /// (a multipart part, or a raw `< file` body — see `attach`).
    fn insert_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc().filter(|d| d.http) else { return };
        let (editor, dir) = (d.editor.clone(), d.path.parent().unwrap_or(Path::new(".")).to_path_buf());
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Insert".into()) });
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(files))) = picked.await else { return };
            editor
                .update_in(cx, |s, window, cx| {
                    if !attach::attach_files(s, &files, &dir, window, cx) {
                        window.push_notification("Put the cursor in a request to attach a file", cx);
                    }
                })
                .ok();
        })
        .detach();
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

    // ---- center ----

    /// Merged variables for the selected environment, plus every environment name the file can see.
    fn env_for(&self, path: &Path, root: &Path) -> doc::EnvView {
        let envs = Environments::load(path, root, &self.settings.settings.env_files(root));
        (envs.variables(self.env_name.as_deref()), envs.names(), envs.all_variables())
    }

    /// Switch every open file to `name` (`None` = no environment) and remember it for the
    /// current project. Executors are rebuilt: cookies are kept per environment.
    fn select_env(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        self.env_name = name;
        if let Some(d) = self.doc() {
            let root = self.root_of(&d.path);
            workspace::save_env(Store::default_base().as_deref(), &root, self.env_name.as_deref());
        }
        self.executors.clear();
        for ix in 0..self.docs.len() {
            let path = self.docs[ix].path.clone();
            let root = self.root_of(&path);
            let env = self.env_for(&path, &root);
            let exec = self.executor(&root);
            self.docs[ix].set_env(env, exec, cx);
        }
        cx.notify();
    }

    /// Cmd+Alt+E: Shared → first → … → last → Shared.
    fn next_env(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let names = self.doc().map(|d| d.env_names.clone()).unwrap_or_default();
        let next = match self.env_name.as_ref().and_then(|n| names.iter().position(|x| x == n)) {
            None => names.first().cloned(),
            Some(i) => names.get(i + 1).cloned(),
        };
        self.select_env(next, cx);
        let label = self.env_name.clone().unwrap_or_else(|| SHARED.into());
        window.push_notification(format!("Environment: {label}"), cx);
    }

    /// Top-right "Run with" picker; red when the environment looks like production.
    fn env_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let names = self.doc().map(|d| d.env_names.clone()).unwrap_or_default();
        let current = self.env_name.clone();
        let prod = current.as_deref().is_some_and(|n| self.settings.settings.env.is_prod(n));
        let label = current.clone().unwrap_or_else(|| SHARED.into());
        let view = cx.entity().downgrade();
        Button::new("env-picker")
            .icon(IconName::Globe)
            .label(label)
            .dropdown_caret(true)
            .small()
            .when(prod, |b| b.danger())
            .when(!prod, |b| b.ghost())
            .dropdown_menu(move |menu, _, cx| {
                let pick = |name: Option<String>| {
                    let view = view.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        view.update(cx, |this, cx| this.select_env(name.clone(), cx)).ok();
                    }
                };
                // Open an env file in a tab; `template` creates it first.
                let edit = |path: PathBuf, template: Option<&'static str>| {
                    let view = view.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        if let Some(t) = template
                            && let Err(e) = std::fs::write(&path, t)
                        {
                            window.push_notification(format!("{}: {e}", path.display()), cx);
                            return;
                        }
                        view.update(cx, |this, cx| {
                            if template.is_some() {
                                this.rescan_projects_of(&path);
                            }
                            this.open(path.clone(), None, window, cx);
                        })
                        .ok();
                    }
                };
                // Env files of the current file's chain (read only when the menu opens).
                let chain = view.upgrade().and_then(|v| {
                    let this = v.read(cx);
                    this.doc().map(|d| ((*d.path).clone(), this.root_of(&d.path)))
                });
                let files = chain.as_ref().map(|(path, root)| Environments::load(path, root, &[]).paths()).unwrap_or_default();
                // A pencil after each environment opens its file. It runs before the row's own
                // click (inner first), which then closes the menu without switching environments.
                let editing = Rc::new(std::cell::Cell::new(false));
                let row = |label: String, name: Option<String>| {
                    // Shared always gets a pencil: without `respite.json` it creates one at the root.
                    let file = env_file(&files, name.as_deref())
                        .or_else(|| chain.as_ref().filter(|_| name.is_none()).map(|(_, root)| root.join("respite.json")));
                    let (view, editing, on_row) = (view.clone(), editing.clone(), pick(name.clone()));
                    let editing_row = editing.clone();
                    PopupMenuItem::element(move |_, cx| {
                        let (view, editing, file) = (view.clone(), editing.clone(), file.clone());
                        h_flex().w_full().justify_between().gap_3().child(label.clone()).when_some(file, |el, file| {
                            el.child(
                                div()
                                    .id("edit-env")
                                    .px_1()
                                    .rounded_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .hover(|s| s.text_color(cx.theme().foreground))
                                    // Lucide's pencil, embedded: the bundled assets only hold `IconName` icons.
                                    .child(Icon::default().data(include_bytes!("../assets/pencil.svg")).size(cx.theme().font_size * 0.9))
                                    .on_click(move |_, window, cx| {
                                        editing.set(true);
                                        let created = !file.exists();
                                        if created && let Err(e) = std::fs::write(&file, RESPITE_TEMPLATE) {
                                            window.push_notification(format!("{}: {e}", file.display()), cx);
                                            return;
                                        }
                                        view.update(cx, |this, cx| {
                                            if created {
                                                this.rescan_projects_of(&file);
                                            }
                                            this.open(file.clone(), None, window, cx);
                                        })
                                        .ok();
                                    }),
                            )
                        })
                    })
                    .checked(current == name)
                    .on_click(move |e, window, cx| {
                        if !editing_row.replace(false) {
                            on_row(e, window, cx);
                        }
                    })
                };
                let mut menu = menu.item(row(SHARED.into(), None)).separator();
                if names.is_empty() {
                    menu = menu.label("No other environments");
                }
                menu = names.iter().fold(menu, |menu, n| menu.item(row(n.clone(), Some(n.clone()))));
                let Some((path, root)) = chain else { return menu };
                menu = menu.separator();
                for p in files {
                    let shown = p.strip_prefix(&root).unwrap_or(&p).display().to_string();
                    menu = menu.item(PopupMenuItem::new(format!("Edit {shown}")).on_click(edit(p, None)));
                }
                // New files go to the project root (applies to every subfolder) or next to the
                // file (only its folder and below); say which. Only a new environment's file:
                // `respite.json` comes from Shared's pencil. `dev` when on Shared.
                let names = [format!("respite-{}.json", current.as_deref().unwrap_or("dev"))];
                let root_name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
                let here = path.parent().unwrap_or(&root).to_path_buf();
                let dirs = if here == root { vec![root.clone()] } else { vec![root.clone(), here] };
                for name in &names {
                    for dir in &dirs {
                        let p = dir.join(name);
                        if !p.exists() {
                            // `join("")` (the root itself) leaves a trailing `/`.
                            let shown = Path::new(&root_name).join(dir.strip_prefix(&root).unwrap_or(dir)).display().to_string();
                            let shown = shown.trim_end_matches('/');
                            menu = menu.item(PopupMenuItem::new(format!("Create {name} in {shown}/")).on_click(edit(p, Some(RESPITE_TEMPLATE))));
                        }
                    }
                }
                menu
            })
            .into_any_element()
    }

    fn tabs(&self, cx: &mut Context<Self>) -> TabBar {
        let (muted, fg, hover_bg) = (cx.theme().muted_foreground, cx.theme().foreground, cx.theme().list_active);
        // Sized from the interface font, so the close button keeps up with the tab label.
        let fs = cx.theme().font_size;
        self.docs.iter().enumerate().fold(
            TabBar::new("docs")
                .selected_index(self.current.unwrap_or(0))
                .on_click(cx.listener(|this, ix: &usize, window, cx| this.select(*ix, window, cx)))
                .suffix(div().px_2().flex().items_center().h_full().child(self.env_picker(cx))),
            |bar, (i, d)| {
                // An icon, not a `×` glyph: the glyph sits small and low next to the label.
                let close = div()
                    .id(("close", i))
                    .size(fs * 1.5)
                    // Cancels the tab label's own 12 px right padding; the tab's 4 px gap stays.
                    .ml(px(-12.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .text_color(muted)
                    .hover(move |s| s.text_color(fg).bg(hover_bg))
                    .map(|el| match d.dirty {
                        true => el.child(div().size(fs * 0.5).rounded_full().bg(muted)),
                        false => el.child(Icon::new(IconName::Close).size(fs)),
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close(i, window, cx);
                    }));
                bar.child(Tab::new().label(d.name()).suffix(close))
            },
        )
    }

    fn center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.show_history {
            return self.history_page(cx);
        }
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
        let crumbs = self.breadcrumbs(cx);
        div().size_full().flex().flex_col().children(tabs).children(crumbs).child(body).into_any_element()
    }

    /// ▶ / ◼ and status hints for the visible request lines, from the editor's layout as it
    /// is *now* — call during prepaint, after the editor laid out this frame.
    fn marks(&self, cx: &App) -> Option<(usize, Vec<Mark>)> {
        // The editor is not on screen behind the history page.
        let (false, Some(ix), Some(d)) = (self.show_history, self.current, self.doc()) else { return None };
        if !d.http {
            return None;
        }
        let s = d.editor.read(cx);
        let (text, bounds) = (s.text(), s.input_bounds());
        let mut marks = vec![];
        for r in &d.file.requests {
            let line = r.request_line;
            let (start, end) = (text.line_start_offset(line - 1), text.line_end_offset(line - 1));
            let Some(lb) = s.range_to_bounds(&(start..start)) else { continue };
            if lb.top() < bounds.top() || lb.bottom() > bounds.bottom() {
                continue; // scrolled out or folded
            }
            let status = d.status.get(&line).cloned();
            let hint = match &status {
                _ if !self.settings.settings.editor.inlay_status => None,
                Some(Status::Done(ex)) => Some((summary(ex), self.status_color(ex.response.status, cx))),
                Some(Status::Failed(e)) => Some((format!("✕ {}", truncate(e, 80)), cx.theme().danger)),
                _ => None,
            };
            let hint = hint.zip(s.range_to_bounds(&(end..end))).map(|((t, c), eb)| (t, c, point(eb.right() + px(24.), eb.top())));
            marks.push(Mark {
                line,
                running: matches!(status, Some(Status::Running)),
                // At the editor's right edge (clear of the scrollbar), far from the fold chevrons in the
                // gutter, and fixed under horizontal scroll.
                at: point(bounds.right() - px(RUN_INSET), lb.top()),
                line_height: lb.size.height,
                hint,
            });
        }
        Some((ix, marks))
    }

    /// Run buttons and status hints painted over the editor. A canvas, not child elements:
    /// its prepaint runs after the editor's in the same frame, so positions never lag a
    /// frame behind while scrolling.
    fn overlay(&self, cx: &Context<Self>) -> impl IntoElement {
        let (read, write) = (cx.entity(), cx.entity());
        canvas(
            move |_, window, cx| {
                let Some((ix, marks)) = read.read(cx).marks(cx) else { return vec![] };
                marks
                    .into_iter()
                    .map(|m| (ix, window.insert_hitbox(Bounds::new(m.at, size(px(14.), m.line_height)), HitboxBehavior::Normal), m))
                    .collect::<Vec<_>>()
            },
            move |_, marks, window, cx| {
                let (run, stop, bg) = (cx.theme().success, cx.theme().warning, cx.theme().background);
                for (ix, hitbox, m) in marks {
                    if let Some((text, color, at)) = &m.hint {
                        // Clipped short of ▶, so it never spills over the response panel.
                        let mask = ContentMask { bounds: Bounds::from_corners(point(px(0.), px(0.)), point(m.at.x - px(8.), px(1e6))) };
                        window.with_content_mask(Some(mask), |window| paint_text(text, *at, *color, m.line_height, window, cx));
                    }
                    // Backdrop: a long line runs under the button.
                    window.paint_quad(fill(Bounds::new(point(m.at.x - px(4.), m.at.y), size(px(22.), m.line_height)), bg));
                    paint_text(if m.running { "◼" } else { "▶" }, m.at, if m.running { stop } else { run }, m.line_height, window, cx);
                    window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                    let (view, line, running) = (write.clone(), m.line, m.running);
                    window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left && hitbox.is_hovered(window) {
                            // Painted after the editor, so this runs first; the editor must not also handle it.
                            cx.stop_propagation();
                            view.update(cx, |this, cx| {
                                if running {
                                    this.cancel(ix, line, cx);
                                    return;
                                }
                                // The response panel follows the caret: put it on this request.
                                if let Some(d) = this.docs.get(ix) {
                                    d.editor.update(cx, |s, cx| s.set_cursor_position(Position::new(line as u32 - 1, 0), window, cx));
                                }
                                this.send(ix, vec![line], window, cx);
                            });
                        }
                    });
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    // ---- response ----

    /// Two compact rows: `METHOD name …… [200 OK] 564 ms · 277 B · HTTP/2`, then tabs + search.
    fn response_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let muted = t.muted_foreground;
        // (method, label) of what the panel shows: the picked history entry while History is
        // open, else the request under the cursor.
        let (head, status) = if self.show_history {
            match &self.history_pick {
                Some((e, ex)) => (Some((e.method.clone(), e.label.clone())), Some(Status::Done(ex.clone()))),
                None => (None, None),
            }
        } else {
            let d = self.doc();
            let req = d.and_then(|d| d.active.and_then(|l| d.file.requests.iter().find(|r| r.request_line == l)));
            (req.map(|r| (r.method.clone(), r.label())), req.and_then(|r| d?.status.get(&r.request_line)).cloned())
        };

        let has_head = head.is_some();
        let title = div().flex_1().min_w_0().flex().items_center().gap_1p5().text_sm();
        let title = match head {
            Some((method, label)) => {
                // Imported blocks are titled "METHOD /path" already.
                let label = label.strip_prefix(&format!("{method} ")).map(str::to_string).unwrap_or(label);
                title
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .font_weight(FontWeight::BOLD)
                            .font_family(t.mono_font_family.clone())
                            .text_color(sidebar::method_color(&method, cx))
                            .child(method),
                    )
                    .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(label))
            }
            None if self.show_history => title.text_color(muted).child("Pick an entry"),
            None => title.text_color(muted).child("No request under the cursor"),
        };
        let pill = |text: String, color: Hsla| {
            div().flex_none().px_1p5().rounded_sm().bg(color.opacity(0.15)).text_color(color).text_xs().font_weight(FontWeight::BOLD).child(text)
        };
        let right = match &status {
            None if self.show_history => div(),
            None => div().text_xs().text_color(muted).child("Cmd+Enter to send"),
            Some(Status::Running) => div().text_xs().text_color(t.warning).child("Running…"),
            Some(Status::Failed(_)) => pill("Error".into(), t.danger),
            Some(Status::Done(ex)) => {
                let r = &ex.response;
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(pill(format!("{} {}", r.status, r.reason), self.status_color(r.status, cx)))
                    .child(div().text_xs().text_color(muted).child(format!(
                        "{} ms · {} · {}",
                        r.total.as_millis(),
                        size_label(r.body.len()),
                        r.http_version
                    )))
            }
        };
        // Re-run / stop the request under the cursor; not on the History page (an entry is not a request).
        let running = matches!(status, Some(Status::Running));
        let run = (!self.show_history && has_head).then(|| {
            Button::new("response-run")
                .label(if running { "◼ Stop" } else { "▶ Run" })
                .primary()
                .small()
                .tooltip(if running { "Stop" } else { "Run again (Cmd+Enter)" })
                .on_click(cx.listener(move |this, _, window, cx| {
                    let Some((ix, line)) = this.current.zip(this.doc().and_then(|d| d.active)) else { return };
                    if running { this.cancel(ix, line, cx) } else { this.send(ix, vec![line], window, cx) }
                }))
        });
        // Title (+ run) on one row, status on its own row below, so neither squeezes the other.
        // A hairline under the status keeps it apart from the tabs below.
        let header = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .pb_2()
            .mb_1()
            .border_b_1()
            .border_color(t.border)
            .child(div().h(px(28.)).flex().items_center().gap_2().child(title).children(run))
            .child(div().h(px(20.)).flex().items_center().child(right));
        let panel = div().size_full().px_3().pt_2().flex().flex_col().gap_1().bg(t.background).child(header);
        match status {
            None | Some(Status::Running) => panel,
            Some(Status::Failed(e)) => panel.child(div().text_sm().text_color(t.danger).child(e)),
            Some(Status::Done(ex)) => {
                let deps = (!ex.dependencies.is_empty())
                    .then(|| div().text_xs().text_color(muted).child(format!("Ran first: {}", ex.dependencies.join(", "))));
                let view = if self.tab == 0 { &self.resp_body } else { &self.resp_meta };
                // Same panel as Cmd+F inside the response (find, next/prev, match case, regex).
                let target = view.clone();
                let search = Button::new("response-search")
                    .icon(IconName::Search)
                    .ghost()
                    .xsmall()
                    .tooltip("Search response (Cmd+F)")
                    .on_click(move |_, window, cx| {
                        target.update(cx, |s, cx| {
                            s.focus(window, cx);
                            s.open_search(false, cx);
                        })
                    });
                let wrap_on = self.settings.settings.response.soft_wrap;
                let wrap = Button::new("response-wrap")
                    .label("Wrap")
                    .ghost()
                    .xsmall()
                    .selected(wrap_on)
                    .tooltip(if wrap_on { "Long lines wrap — click to scroll them instead" } else { "Long lines scroll — click to wrap them" })
                    .on_click(cx.listener(move |this, _, window, cx| this.set("response.soft_wrap", (!wrap_on).into(), window, cx)));
                let tabs = TABS.iter().fold(
                    TabBar::new("response-tabs")
                        .underline()
                        .small()
                        .selected_index(self.tab)
                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                            this.tab = *ix;
                            cx.notify();
                        }))
                        .suffix(div().flex().items_center().gap_1().child(wrap).child(search)),
                    |bar, t| bar.child(Tab::new().label(*t)),
                );
                panel
                    .children(deps)
                    .child(tabs)
                    .child(div().flex_1().min_h_0().child(Editor::new(view).size_full().bordered(false)))
            }
        }
    }

    /// Load the active block's exchange into the response editors (only when it or the tab changed:
    /// `set_value` resets selection and scroll).
    fn sync_response(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ex = if self.show_history {
            self.history_pick.as_ref().map(|(_, ex)| ex.clone())
        } else {
            self.doc().and_then(|d| match d.status.get(&d.active?) {
                Some(Status::Done(ex)) => Some(ex.clone()),
                _ => None,
            })
        };
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

/// One request line's ▶ / ◼ and its status hint, in window coordinates.
/// ▶ distance from the editor's right edge (leaves room for the scrollbar).
const RUN_INSET: f32 = 30.;

struct Mark {
    line: usize,
    running: bool,
    at: Point<Pixels>,
    line_height: Pixels,
    hint: Option<(String, Hsla, Point<Pixels>)>,
}

/// Paint one line of UI text at `origin` (top-left of an editor line `line_height` tall).
fn paint_text(text: &str, origin: Point<Pixels>, color: Hsla, line_height: Pixels, window: &mut Window, cx: &mut App) {
    let font_size = px(13.);
    let run = TextRun { len: text.len(), font: window.text_style().font(), color, background_color: None, underline: None, strikethrough: None };
    let shaped = window.text_system().shape_line(SharedString::from(text.to_string()), font_size, &[run], None);
    let _ = shaped.paint(origin, line_height, TextAlign::Left, None, window, cx);
}

/// Scroll so 0-based `line` sits mid-viewport, unless it's already well inside it (the editor
/// alone only scrolls it into view, onto the bottom row). A just-opened editor has no layout
/// yet: `retry` waits one frame for it.
fn center_line(editor: Entity<EditorState>, line: usize, retry: bool, window: &mut Window, cx: &mut App) {
    let done = editor.update(cx, |s, cx| {
        let (Some(lh), Some(bounds)) = (s.line_height(), s.text_bounds()) else { return false };
        let row = s.display_map().buffer_line_to_display_row(line);
        if s.visible_row_range().is_some_and(|r| r.start + 2 < row && row + 3 < r.end) {
            return true;
        }
        let y = (bounds.size.height - lh) / 2. - lh * row as f32;
        s.set_scroll_offset(point(px(0.), y.min(px(0.))), cx);
        true
    });
    if !done && retry {
        window.on_next_frame(move |window, cx| center_line(editor, line, false, window, cx));
    }
}

/// `200 OK · 142 ms · 3.4 KB`
fn summary(ex: &Exchange) -> String {
    let r = &ex.response;
    format!("{} {} · {} ms · {}", r.status, r.reason, r.total.as_millis(), size_label(r.body.len()))
}

fn size_label(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.),
        n => format!("{:.1} MB", n as f64 / 1024. / 1024.),
    }
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
            .on_action(cx.listener(|this, _: &RunRequest, window, cx| {
                if let (Some(ix), Some(line)) = (this.current, this.doc().and_then(|d| d.cursor_request(cx)).map(|r| r.request_line)) {
                    this.send(ix, vec![line], window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RunAll, window, cx| {
                if let Some(ix) = this.current {
                    let lines = this.docs[ix].file.requests.iter().map(|r| r.request_line).collect();
                    this.send(ix, lines, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RunSelected, window, cx| {
                if let Some(ix) = this.current {
                    let lines = this.selected_requests(ix, cx);
                    this.send(ix, lines, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &RepeatLast, window, cx| {
                let Some((path, lines)) = this.last_run.clone() else {
                    window.push_notification("Nothing sent yet", cx);
                    return;
                };
                if let Some(ix) = this.open(path, None, window, cx) {
                    this.send(ix, lines, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CancelRequests, _, cx| this.cancel_all(cx)))
            .on_action(cx.listener(|this, _: &GoToFile, window, cx| this.pick_file(window, cx)))
            .on_action(cx.listener(|this, _: &GoToRequest, window, cx| this.pick_request(window, cx)))
            .on_action(cx.listener(|this, _: &CommandPalette, window, cx| this.pick_command(commands(), window, cx)))
            .on_action(cx.listener(|this, _: &PrevRequest, window, cx| this.step_request(false, window, cx)))
            .on_action(cx.listener(|this, _: &NextRequest, window, cx| this.step_request(true, window, cx)))
            .on_action(cx.listener(|this, _: &SmartPaste, window, cx| this.smart_paste(window, cx)))
            .on_action(cx.listener(|this, _: &PasteCurl, window, cx| this.paste_curl(window, cx)))
            .on_action(cx.listener(|this, _: &InsertFile, window, cx| this.insert_file(window, cx)))
            .on_action(cx.listener(|this, _: &CopyAsCurl, window, cx| this.copy_as_curl(window, cx)))
            .on_action(cx.listener(|this, _: &SaveFile, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if let Some(ix) = this.current {
                    this.close(ix, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &AddProject, _, cx| this.add_project(cx)))
            .on_action(cx.listener(|this, _: &Import, window, cx| this.import(window, cx)))
            .on_action(cx.listener(|this, _: &NextEnvironment, window, cx| this.next_env(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleHistory, _, cx| this.toggle_history(cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)))
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
            .child(self.overlay(cx))
    }
}

/// macOS menu bar (Linux has none); shortcuts shown come from the keymap.
fn set_menus(cx: &mut App) {
    use gpui_component::input::{Copy, Cut, Paste, Redo, Search, SelectAll, Undo};
    cx.set_menus([
        Menu::new("Respite").items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit Respite", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Add Project…", AddProject),
            MenuItem::action("Import…", Import),
            MenuItem::separator(),
            MenuItem::action("Save", SaveFile),
            MenuItem::action("Close Tab", CloseTab),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", SmartPaste, OsAction::Paste),
            MenuItem::action("Paste as Plain Text", Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find", Search),
        ]),
        Menu::new("Request").items([
            MenuItem::action("Run Request", RunRequest),
            MenuItem::action("Run All in File", RunAll),
            MenuItem::action("Run Selected", RunSelected),
            MenuItem::action("Repeat Last", RepeatLast),
            MenuItem::action("Cancel Running", CancelRequests),
            MenuItem::separator(),
            MenuItem::action("Paste cURL", PasteCurl),
            MenuItem::action("Copy as cURL", CopyAsCurl),
            MenuItem::action("Insert File…", InsertFile),
            MenuItem::separator(),
            MenuItem::action("Next Environment", NextEnvironment),
        ]),
        Menu::new("View").items([
            MenuItem::action("History", ToggleHistory),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Next Tab", NextTab),
        ]),
        Menu::new("Go").items([
            MenuItem::action("Go to File…", GoToFile),
            MenuItem::action("Go to Request…", GoToRequest),
            MenuItem::action("Previous Request", PrevRequest),
            MenuItem::action("Next Request", NextRequest),
            MenuItem::separator(),
            MenuItem::action("Command Palette…", CommandPalette),
        ]),
    ]);
}

/// What Cmd+Shift+P offers: the menu commands, by their menu names.
fn commands() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("Run Request", Box::new(RunRequest)),
        ("Run All in File", Box::new(RunAll)),
        ("Run Selected", Box::new(RunSelected)),
        ("Repeat Last", Box::new(RepeatLast)),
        ("Cancel Running", Box::new(CancelRequests)),
        ("Paste cURL", Box::new(PasteCurl)),
        ("Copy as cURL", Box::new(CopyAsCurl)),
        ("Insert File…", Box::new(InsertFile)),
        ("Next Environment", Box::new(NextEnvironment)),
        ("Go to File…", Box::new(GoToFile)),
        ("Go to Request…", Box::new(GoToRequest)),
        ("Previous Request", Box::new(PrevRequest)),
        ("Next Request", Box::new(NextRequest)),
        ("History", Box::new(ToggleHistory)),
        ("Toggle Sidebar", Box::new(ToggleSidebar)),
        ("Next Tab", Box::new(NextTab)),
        ("Save", Box::new(SaveFile)),
        ("Close Tab", Box::new(CloseTab)),
        ("Add Project…", Box::new(AddProject)),
        ("Import…", Box::new(Import)),
        ("Settings…", Box::new(OpenSettings)),
        ("Quit Respite", Box::new(Quit)),
    ]
}

/// `respite <command> …` runs the CLI without starting the GUI;
/// `respite [open] [--env NAME] [file.http | dir]` opens the GUI.
fn main() -> std::process::ExitCode {
    if let Some(code) = respite_cli::main_if_cli() {
        return code;
    }
    // ponytail: hand-rolled GUI args; fold into clap when `open` grows options.
    // `-psn_…` is a process serial number some macOS launches still pass to app bundles.
    let mut args = std::env::args().skip(1).filter(|a| a != "open" && !a.starts_with("-psn_"));
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
        // No target: the collections folder, not the launch dir (cwd is `/` from Finder, `target/…` from cargo).
        None => {
            let dir = workspace::collections_dir();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                eprintln!("respite: {}: {e}", dir.display());
            }
            (dir, None)
        }
    };
    gpui_platform::application().with_assets(gpui_kit_assets::Assets).run(move |cx: &mut App| {
        gpui_component::init(cx); // theme and fonts come from settings (Respite::apply_settings)
        themes::register(cx);
        // `secondary-` is not supported by this GPUI version.
        let m = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };
        let key = |k: &str| -> &'static str { format!("{m}-{k}").leak() };
        // Editor keys go in its own "Input" context (later bindings win over its cmd-enter / cmd-v).
        cx.bind_keys([
            KeyBinding::new(key("enter"), RunRequest, Some("Input")),
            KeyBinding::new(key("shift-enter"), RunAll, Some("Input")),
            KeyBinding::new(key("alt-enter"), RunSelected, Some("Input")),
            KeyBinding::new(key("alt-l"), RepeatLast, Some("Input")),
            // Over the editor's code actions (Cmd+.), which have no provider here.
            KeyBinding::new(key("."), CancelRequests, Some("Input")),
            KeyBinding::new("alt-up", PrevRequest, Some("Input")),
            KeyBinding::new("alt-down", NextRequest, Some("Input")),
            KeyBinding::new(key("v"), SmartPaste, Some("Input")),
            KeyBinding::new(key("shift-v"), gpui_component::input::Paste, Some("Input")),
            KeyBinding::new(key("alt-v"), PasteCurl, Some("Input")),
            KeyBinding::new(key("shift-c"), CopyAsCurl, Some("Input")),
            KeyBinding::new(key("i"), InsertFile, Some("Input")),
            // The editor has these actions but no default keys.
            KeyBinding::new("f12", gpui_component::input::GoToDefinition, Some("Input")),
            KeyBinding::new("ctrl-space", gpui_component::input::ShowCompletions, Some("Input")),
        ]);
        for ctx in ["Respite", "Input"] {
            cx.bind_keys([
                KeyBinding::new(key("s"), SaveFile, Some(ctx)),
                KeyBinding::new(key("w"), CloseTab, Some(ctx)),
                KeyBinding::new(key("b"), ToggleSidebar, Some(ctx)),
                KeyBinding::new("ctrl-tab", NextTab, Some(ctx)),
                KeyBinding::new(key(","), OpenSettings, Some(ctx)),
                KeyBinding::new(key("alt-e"), NextEnvironment, Some(ctx)),
                KeyBinding::new(key("y"), ToggleHistory, Some(ctx)),
                KeyBinding::new(key("p"), GoToFile, Some(ctx)),
                KeyBinding::new(key("shift-o"), GoToRequest, Some(ctx)),
                KeyBinding::new(key("shift-p"), CommandPalette, Some(ctx)),
            ]);
        }
        cx.bind_keys([KeyBinding::new(key("q"), Quit, None)]);
        set_menus(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(880.)), cx)),
            titlebar: Some(TitlebarOptions { title: Some("Respite".into()), ..Default::default() }),
            ..Default::default()
        };
        let mut view = None;
        let handle = cx
            .open_window(options, |window, cx| {
                let v = cx.new(|cx| Respite::new(root, open, env_name, window, cx));
                view = Some(v.downgrade());
                cx.new(|cx| Root::new(v, window, cx))
            })
            .unwrap();
        let main = handle.window_id();
        // Quit (Cmd+Q, menu) asks about unsaved files in the main window, from any window.
        let view = view.expect("main view");
        cx.on_action(move |_: &Quit, cx| {
            let view = view.clone();
            let asked = handle.update(cx, |_, window, cx| view.update(cx, |this, cx| this.confirm_quit(window, cx)));
            if !matches!(asked, Ok(Ok(()))) {
                cx.quit();
            }
        });
        // Closing the settings window keeps the app running.
        cx.on_window_closed(move |cx, id| {
            if id == main {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
    std::process::ExitCode::SUCCESS
}
