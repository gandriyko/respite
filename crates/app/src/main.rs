//! Respite GUI: sidebar (projects → .http files → requests), tabs of open files,
//! editor with ▶ / status overlays, response panel.
//! ▶ and the status inlay are overlays positioned from the editor's last layout
//! (gpui-component has no custom gutter elements or inlay hints).

mod doc;
mod highlight;
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
    ActiveTheme as _, Root, Theme, ThemeMode, WindowExt as _, h_resizable,
    input::{Editor, EditorState, InputEvent, InputState, Position, RopeExt as _},
    IconName, Sizable as _,
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
    [RunRequest, SmartPaste, PasteCurl, CopyAsCurl, SaveFile, CloseTab, ToggleSidebar, NextTab, OpenSettings, AddProject, Quit, NextEnvironment]
);

const SAMPLE: &str = include_str!("../sample.http");

/// Starting points for "Create …" in the environment menu (JetBrains format).
const ENV_TEMPLATES: [(&str, &str); 2] = [
    ("http-client.env.json", "{\n  \"$shared\": {},\n  \"dev\": {\n    \"baseUrl\": \"http://localhost:8080\"\n  }\n}\n"),
    // Secrets go here; keep this file out of git.
    ("http-client.private.env.json", "{\n  \"dev\": {\n    \"token\": \"\"\n  }\n}\n"),
];
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
    /// Read-only editors (selectable, Cmd+F, folding): body with tree-sitter highlighting,
    /// headers/request with the `.http` highlighter.
    resp_body: Entity<EditorState>,
    resp_meta: Entity<EditorState>,
    tab: usize,
    /// (exchange pointer, tab) currently loaded into the response editors.
    shown: Option<(usize, usize)>,
    settings: respite_settings::Loaded,
    show_settings: bool,
    font_input: Entity<InputState>,
    timeout_input: Entity<InputState>,
    /// Theme's own monospace font, used when `font_family` is empty.
    default_mono: SharedString,
    _subs: Vec<Subscription>,
}

impl Respite {
    fn new(root: PathBuf, open: Option<PathBuf>, env_name: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // `--env` wins; otherwise the environment last picked for this project.
        let env_name = env_name.or_else(|| workspace::load_env(Store::default_base().as_deref(), &root));
        let mut projects = workspace::projects(Store::default_base().as_deref(), &root);
        let files = workspace::scan(&root);
        let first = files.first().cloned();
        projects[0].files = Some(files);
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
        let s = &settings.settings;
        let font_input = cx.new(|cx| InputState::new(window, cx).placeholder("Default monospace font").default_value(s.font_family.clone()));
        let timeout_input = cx.new(|cx| InputState::new(window, cx).placeholder("0 = no limit").default_value(s.request.timeout_ms.to_string()));
        let _subs = vec![
            cx.subscribe_in(&font_input, window, |this, input, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::Change) {
                    let v = input.read(cx).value().to_string();
                    this.set("font_family", v.trim().into(), window, cx);
                }
            }),
            cx.subscribe_in(&timeout_input, window, |this, input, ev: &InputEvent, window, cx| {
                // Invalid numbers are ignored until they parse.
                if let (InputEvent::Change, Ok(ms)) = (ev, input.read(cx).value().trim().parse::<u64>()) {
                    this.set("request.timeout_ms", ms.into(), window, cx);
                }
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.settings.settings.appearance == respite_settings::Appearance::System {
                    Theme::sync_system_appearance(Some(window), cx);
                }
            }),
        ];
        let mut this = Self {
            settings,
            show_settings: false,
            font_input,
            timeout_input,
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
            resp_body: readonly(false, window, cx),
            resp_meta: readonly(true, window, cx),
            tab: 0,
            shown: None,
        };
        match open.or(first) {
            Some(path) => this.open(path, None, window, cx),
            None => this.open(root.join("untitled.http"), Some(SAMPLE.into()), window, cx),
        };
        this.apply_settings(window, cx);
        this
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
                let root = http_env::workspace_root(&path);
                // ponytail: env loaded when the file opens; reload on env-file change comes with `notify` watching.
                let env = self.env_for(&path, &root);
                let st = &self.settings.settings;
                let (line_numbers, soft_wrap) = (st.editor.line_numbers, st.editor.soft_wrap);
                let exec = self.executor(&root);
                // Env files are plain JSON (tree-sitter highlighting, no requests).
                let http = !workspace::is_env_file(&path);
                let editor = cx.new(|cx| {
                    let mut s = EditorState::new(window, cx)
                        .language(if http { "http" } else { "json" })
                        .searchable(true)
                        .line_number(line_numbers)
                        .soft_wrap(soft_wrap)
                        .default_value(text);
                    if http {
                        s.set_highlighter_factory(Rc::new(|_: &str| Some(Box::new(highlight::HttpHighlighter::default()) as _)), cx);
                        s.set_gutter_decorations_width(px(3.), cx);
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
                self.docs.push(Doc::new(path.clone(), editor, env, exec, subs, cx));
                if let Some(e) = Environments::load(&path, &root, &[]).errors.first() {
                    window.push_notification(format!("Env file skipped: {}: {}", e.0.display(), e.1), cx);
                }
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
                self.rescan_projects_of(&path);
                if workspace::is_env_file(&path) {
                    // New variables / environments apply to every open request file right away.
                    self.select_env(self.env_name.clone(), cx);
                    let root = http_env::workspace_root(&path);
                    if let Some((p, e)) = Environments::load(&path, &root, &[]).errors.into_iter().find(|(p, _)| *p == path) {
                        window.push_notification(format!("{} is not valid JSON, so it is skipped: {e}", p.display()), cx);
                    }
                }
            }
            Err(e) => window.push_notification(format!("Save failed: {}: {e}", d.path.display()), cx),
        }
        cx.notify();
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
    fn env_for(&self, path: &Path, root: &Path) -> (HashMap<String, http_env::Variable>, Vec<String>) {
        let envs = Environments::load(path, root, &self.settings.settings.env_files(root));
        (envs.variables(self.env_name.as_deref()), envs.names())
    }

    /// Switch every open file to `name` (`None` = no environment) and remember it for the
    /// current project. Executors are rebuilt: cookies are kept per environment.
    fn select_env(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        self.env_name = name;
        if let Some(d) = self.doc() {
            let root = http_env::workspace_root(&d.path);
            workspace::save_env(Store::default_base().as_deref(), &root, self.env_name.as_deref());
        }
        self.executors.clear();
        for ix in 0..self.docs.len() {
            let path = self.docs[ix].path.clone();
            let root = http_env::workspace_root(&path);
            let (env, names) = self.env_for(&path, &root);
            let exec = self.executor(&root);
            let d = &mut self.docs[ix];
            (d.env, d.env_names, d.exec) = (Arc::new(env), names, exec);
        }
        cx.notify();
    }

    /// Cmd+Alt+E: No environment → first → … → last → No environment.
    fn next_env(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let names = self.doc().map(|d| d.env_names.clone()).unwrap_or_default();
        let next = match self.env_name.as_ref().and_then(|n| names.iter().position(|x| x == n)) {
            None => names.first().cloned(),
            Some(i) => names.get(i + 1).cloned(),
        };
        self.select_env(next, cx);
        let label = self.env_name.clone().unwrap_or_else(|| "No environment".into());
        window.push_notification(format!("Environment: {label}"), cx);
    }

    /// Top-right "Run with" picker; red when the environment looks like production.
    fn env_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let names = self.doc().map(|d| d.env_names.clone()).unwrap_or_default();
        let current = self.env_name.clone();
        let prod = current.as_deref().is_some_and(|n| self.settings.settings.env.is_prod(n));
        let label = current.clone().unwrap_or_else(|| "No environment".into());
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
                            this.show_settings = false;
                            if template.is_some() {
                                this.rescan_projects_of(&path);
                            }
                            this.open(path.clone(), None, window, cx);
                        })
                        .ok();
                    }
                };
                let none = PopupMenuItem::new("No environment").checked(current.is_none()).on_click(pick(None));
                let mut menu = menu.item(none).separator();
                if names.is_empty() {
                    menu = menu.label("No environments defined");
                }
                menu = names.iter().fold(menu, |menu, n| {
                    menu.item(PopupMenuItem::new(n.clone()).checked(current.as_deref() == Some(n)).on_click(pick(Some(n.clone()))))
                });
                // Edit / create env files of the current file's chain (read only when the menu opens).
                let Some(path) = view.upgrade().and_then(|v| v.read(cx).doc().map(|d| (*d.path).clone())) else { return menu };
                let root = http_env::workspace_root(&path);
                menu = menu.separator();
                for p in Environments::load(&path, &root, &[]).paths() {
                    let shown = p.strip_prefix(&root).unwrap_or(&p).display().to_string();
                    menu = menu.item(PopupMenuItem::new(format!("Edit {shown}")).on_click(edit(p, None)));
                }
                for (name, template) in ENV_TEMPLATES {
                    let p = root.join(name);
                    if !p.exists() {
                        menu = menu.item(PopupMenuItem::new(format!("Create {name}")).on_click(edit(p, Some(template))));
                    }
                }
                // Respite format: one flat file per environment.
                let name = format!("respite.{}.json", current.as_deref().unwrap_or("dev"));
                let p = root.join(&name);
                if !p.exists() {
                    menu = menu.item(PopupMenuItem::new(format!("Create {name}")).on_click(edit(p, Some(RESPITE_TEMPLATE))));
                }
                menu
            })
            .into_any_element()
    }

    fn tabs(&self, cx: &mut Context<Self>) -> TabBar {
        let muted = cx.theme().muted_foreground;
        self.docs.iter().enumerate().fold(
            TabBar::new("docs")
                .selected_index(self.current.unwrap_or(0))
                .on_click(cx.listener(|this, ix: &usize, window, cx| this.select(*ix, window, cx)))
                .suffix(div().px_2().flex().items_center().h_full().child(self.env_picker(cx))),
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
        if self.show_settings {
            return self.settings_page(cx);
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
        div().size_full().flex().flex_col().children(tabs).child(body).into_any_element()
    }

    fn overlays(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        // The editor is not on screen behind the settings page; its last layout is stale.
        let (false, Some(ix), Some(d)) = (self.show_settings, self.current, self.doc()) else { return vec![] };
        if !d.http {
            return vec![];
        }
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
                _ if !self.settings.settings.editor.inlay_status => None,
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

    /// Two compact rows: `METHOD name …… [200 OK] 564 ms · 277 B · HTTP/2`, then tabs + search.
    fn response_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let muted = t.muted_foreground;
        let d = self.doc();
        let req = d.and_then(|d| d.active.and_then(|l| d.file.requests.iter().find(|r| r.request_line == l)));
        let status = req.and_then(|r| d?.status.get(&r.request_line)).cloned();

        let title = div().flex_1().min_w_0().flex().items_center().gap_1p5().text_sm();
        let title = match req {
            Some(r) => {
                let label = r.label();
                // Imported blocks are titled "METHOD /path" already.
                let label = label.strip_prefix(&format!("{} ", r.method)).map(str::to_string).unwrap_or(label);
                title
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .font_weight(FontWeight::BOLD)
                            .font_family(t.mono_font_family.clone())
                            .text_color(sidebar::method_color(&r.method, cx))
                            .child(r.method.clone()),
                    )
                    .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(label))
            }
            None => title.text_color(muted).child("No request under the cursor"),
        };
        let pill = |text: String, color: Hsla| {
            div().flex_none().px_1p5().rounded_sm().bg(color.opacity(0.15)).text_color(color).text_xs().font_weight(FontWeight::BOLD).child(text)
        };
        let right = match &status {
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
        let header = div().flex_none().h(px(30.)).flex().items_center().gap_2().child(title).child(right);
        let panel = div().size_full().px_3().pt_1().flex().flex_col().gap_1().bg(t.background).child(header);
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
                let tabs = TABS.iter().fold(
                    TabBar::new("response-tabs")
                        .underline()
                        .small()
                        .selected_index(self.tab)
                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                            this.tab = *ix;
                            cx.notify();
                        }))
                        .suffix(search),
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
            .on_action(cx.listener(|this, _: &AddProject, _, cx| this.add_project(cx)))
            .on_action(cx.listener(|this, _: &NextEnvironment, window, cx| this.next_env(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.show_settings = !this.show_settings;
                cx.notify();
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
            MenuItem::separator(),
            MenuItem::action("Paste cURL", PasteCurl),
            MenuItem::action("Copy as cURL", CopyAsCurl),
            MenuItem::separator(),
            MenuItem::action("Next Environment", NextEnvironment),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Next Tab", NextTab),
        ]),
    ]);
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
        gpui_component::init(cx); // theme and fonts come from settings (Respite::apply_settings)
        themes::register(cx);
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
                KeyBinding::new(key(","), OpenSettings, Some(ctx)),
                KeyBinding::new(key("alt-e"), NextEnvironment, Some(ctx)),
            ]);
        }
        cx.bind_keys([KeyBinding::new(key("q"), Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        set_menus(cx);
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
