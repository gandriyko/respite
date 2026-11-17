//! Sidebar: the central folder (folders → `.http` files → requests), then the files added
//! from elsewhere.
//!
//! Styled after the VS Code / Zed project panels: disclosure chevrons, indent guides,
//! compact single-child folders (`api/v2`), inset rounded selection, a fixed
//! right-aligned method column (as in Bruno / Insomnia) and muted meta on the right.

use std::{
    collections::BTreeMap,
    ops::Range,
    path::{Path, PathBuf},
};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
};
use http_exec::Store;

use crate::{Respite, doc::Status, workspace};

const ROW_H: f32 = 24.;
const INDENT: f32 = 14.;
/// Left padding before the first chevron.
const PAD: f32 = 6.;
/// Chevron column width; leaves get a spacer of the same width.
const CHEVRON: f32 = 14.;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    /// `~/.config/respite/collections`
    Collections,
    /// Added from elsewhere.
    Files,
}

pub(crate) enum Row {
    Section(Section),
    Dir { path: PathBuf, name: String, depth: usize },
    /// `added`: in the Files section (× drops it from the list).
    File { path: PathBuf, name: String, depth: usize, requests: usize, added: bool },
    Request { path: PathBuf, line: usize, method: String, label: String, depth: usize },
    Note { text: &'static str, depth: usize },
}

impl Row {
    fn depth(&self) -> usize {
        match self {
            Row::Section(_) => 0,
            Row::Dir { depth, .. } | Row::File { depth, .. } | Row::Request { depth, .. } | Row::Note { depth, .. } => *depth,
        }
    }
}

/// Folder tree of the central folder.
#[derive(Default)]
struct Tree {
    dirs: BTreeMap<String, Tree>,
    files: Vec<PathBuf>,
}

impl Tree {
    /// `entries`: folders and files under `root` (folders make empty nodes).
    fn build(root: &Path, entries: &[PathBuf]) -> Self {
        let mut tree = Tree::default();
        for e in entries {
            let rel = e.strip_prefix(root).unwrap_or(e);
            let is_dir = !workspace::is_request_file(e);
            let parent = if is_dir { Some(rel) } else { rel.parent() };
            let dirs = parent.into_iter().flat_map(|p| p.components()).map(|c| c.as_os_str().to_string_lossy().into_owned());
            let node = dirs.fold(&mut tree, |node, d| node.dirs.entry(d).or_default());
            if !is_dir {
                node.files.push(e.clone());
            }
        }
        tree
    }
}

/// `+`, new folder, `×` … on a row or a section header: shown on hover, or always; `tip` on hover.
fn row_action(id: impl Into<ElementId>, child: impl IntoElement, tip: &'static str, always: bool, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tip).build(window, cx))
        .flex_none()
        .p_0p5()
        .rounded_sm()
        .text_color(cx.theme().muted_foreground)
        .when(!always, |d| d.invisible().group_hover("row", |s| s.visible()))
        .hover(|s| s.bg(cx.theme().list_active))
        .child(child)
}

impl Respite {
    /// `+` in the Files section: `.http` / `.rest` files from anywhere, several at once.
    pub(crate) fn add_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Add".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            this.update_in(cx, |this, window, cx| this.add_paths(paths, window, cx)).ok();
        })
        .detach();
    }

    /// Lists and opens the `.http` / `.rest` files among `paths` (the `+` dialog, or files dropped
    /// on the window); the rest are skipped with a notice.
    pub(crate) fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        // ponytail: the system dialog has no extension filter here; other files are skipped after.
        let (ok, skipped): (Vec<PathBuf>, Vec<PathBuf>) = paths.into_iter().partition(|p| workspace::is_request_file(p));
        if !skipped.is_empty() {
            let names: Vec<_> = skipped.iter().map(|p| p.file_name().unwrap_or(p.as_os_str()).to_string_lossy()).collect();
            let title = if skipped.len() == 1 { "Not a request file" } else { "Not request files" };
            let n = Notification::warning(format!("{}\nOnly .http and .rest files can be added.", names.join(", "))).title(title).placement(Anchor::TopLeft);
            window.push_notification(n, cx);
        }
        let first = ok.first().cloned();
        for p in ok {
            self.add_file(p);
        }
        if let Some(p) = first {
            self.open(p, None, window, cx);
        }
        cx.notify();
    }

    /// List `path` in the Files section, unless it's there already or under the central folder.
    pub(crate) fn add_file(&mut self, path: PathBuf) {
        if path.starts_with(workspace::collections_dir()) || self.files.contains(&path) {
            return;
        }
        self.files.push(path);
        workspace::save_files(Store::default_base().as_deref(), &self.files);
    }

    /// Drops the file from the list only; the file and its history stay.
    fn remove_file(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.files.retain(|p| p != path);
        workspace::save_files(Store::default_base().as_deref(), &self.files);
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        window.push_notification(format!("Removed {name} from the list. The file and its history are kept."), cx);
        cx.notify();
    }

    /// File > Import…: into the central folder.
    pub(crate) fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.import_into(workspace::collections_dir(), window, cx);
    }

    /// Several picks at once into `dir`: `.http` / `.rest` files are copied; Postman / Insomnia /
    /// OpenAPI / Bruno / HAR each become a new folder (subfolders kept), environments go to
    /// the env folder.
    pub(crate) fn import_into(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            // Bruno collections are folders.
            directories: true,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            this.update_in(cx, |this, window, cx| {
                let mut first = None;
                for src in paths {
                    let name = src.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    if workspace::is_request_file(&src) {
                        let dest = dir.join(&name);
                        let copied = if dest.exists() { Err("a file with this name is already there".to_string()) } else { std::fs::copy(&src, &dest).map_err(|e| e.to_string()) };
                        match copied {
                            Ok(_) => _ = first.get_or_insert(dest),
                            Err(e) => window.push_notification(Notification::warning(format!("{name}: {e}")).title("Not imported"), cx),
                        }
                        continue;
                    }
                    let result = http_import::import(&src)
                        .and_then(|c| http_import::write(&c, &dir, &workspace::env_dir()).map(|f| (c, f)).map_err(|e| e.to_string()));
                    let (c, files) = match result {
                        Ok(r) => r,
                        Err(e) => {
                            window.push_notification(Notification::warning(format!("{name}: {e}")).title("Not imported"), cx);
                            continue;
                        }
                    };
                    if let Some(f) = files.iter().find(|f| workspace::is_request_file(f)).or(files.first()) {
                        first.get_or_insert(f.clone());
                    }
                    let mut msg = format!("Imported {} '{}': {} file(s)", c.format, c.name, files.len());
                    if !c.warnings.is_empty() {
                        msg += &format!(". {} warning(s): {}", c.warnings.len(), c.warnings.join("; "));
                    }
                    window.push_notification(msg, cx);
                }
                this.central = None;
                this.collapsed_dirs.remove(&dir);
                this.reload_envs(cx);
                if let Some(f) = first {
                    this.open(f, None, window, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A new `.http` file in `dir`, named in an in-app dialog.
    fn new_file(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let dir = dir.to_path_buf();
        self.ask_name("New File", "requests.http", "Create", window, cx, move |this, name, window, cx| {
            let mut path = dir.join(name);
            if !workspace::is_request_file(&path) {
                path.set_extension("http");
            }
            // An existing file is just opened, never truncated.
            if !path.exists()
                && let Err(e) = std::fs::write(&path, "### New request\nGET https://\n")
            {
                window.push_notification(format!("{}: {e}", path.display()), cx);
                return;
            }
            this.central = None;
            this.open(path, None, window, cx);
            cx.notify();
        });
    }

    /// A new folder in `dir`, named in an in-app dialog.
    fn new_folder(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let dir = dir.to_path_buf();
        self.ask_name("New Folder", "New Folder", "Create", window, cx, move |this, name, window, cx| {
            let path = dir.join(name);
            if let Err(e) = std::fs::create_dir_all(&path) {
                window.push_notification(format!("{}: {e}", path.display()), cx);
                return;
            }
            this.collapsed_dirs.remove(&path);
            this.central = None;
            cx.notify();
        });
    }

    /// Not the system save dialog: macOS won't open it inside the hidden `~/.config`, so it
    /// started wherever it was last. Enter or the button applies; a name with `/` is refused.
    fn ask_name(
        &mut self,
        title: &'static str,
        default: &str,
        ok: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl Fn(&mut Self, &str, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(default.to_string()));
        let this = cx.entity().downgrade();
        let apply = std::rc::Rc::new(apply);
        let apply = std::rc::Rc::new({
            let input = input.clone();
            move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_string();
                if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) {
                    window.push_notification(format!("Invalid name: {name}"), cx);
                    return;
                }
                window.close_dialog(cx);
                // Enter arrives through a subscription of `Respite`, which is mid-update then.
                let (this, apply) = (this.clone(), apply.clone());
                window.defer(cx, move |window, cx| {
                    this.update(cx, |this, cx| apply(this, &name, window, cx)).ok();
                });
            }
        });
        self.picker_sub = Some(cx.subscribe_in(&input, window, {
            let apply = apply.clone();
            move |_, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    apply(window, cx);
                }
            }
        }));
        let field = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let apply = apply.clone();
            dialog.title(title).w(px(420.)).child(Input::new(&field)).footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("name-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("name-ok").label(ok).primary().on_click(move |_, window, cx| apply(window, cx))),
            )
        });
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
    }

    fn toggle(&mut self, key: &Path) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_path_buf());
        }
    }

    fn toggle_dir(&mut self, path: &Path) {
        if !self.collapsed_dirs.remove(path) {
            self.collapsed_dirs.insert(path.to_path_buf());
        }
    }

    /// Folders and request files of the central folder, scanned once until something changes.
    pub(crate) fn central_files(&mut self) -> Vec<PathBuf> {
        self.central.get_or_insert_with(|| workspace::scan(&workspace::collections_dir())).clone()
    }

    /// Every request file the sidebar lists: the central folder's, then the added ones.
    pub(crate) fn all_files(&mut self) -> Vec<PathBuf> {
        let mut all: Vec<PathBuf> = self.central_files().into_iter().filter(|p| workspace::is_request_file(p)).collect();
        all.extend(self.files.iter().cloned());
        all
    }

    /// `(request_line, method, label)` of a file: live parse when open, else read once.
    fn outline_of(&mut self, path: &Path) -> Vec<(usize, String, String)> {
        match self.docs.iter().find(|d| *d.path == *path) {
            Some(d) => outline(&d.file),
            None => self
                .outlines
                .entry(path.to_path_buf())
                .or_insert_with(|| outline(&http_syntax::parse(&std::fs::read_to_string(path).unwrap_or_default())))
                .clone(),
        }
    }

    fn build_rows(&mut self, query: &str) -> Vec<Row> {
        let root = workspace::collections_dir();
        let central = self.central_files();
        let mut rows = vec![Row::Section(Section::Collections)];
        let mut found = vec![];
        self.emit(&Tree::build(&root, &central), &root, 0, query, &mut found);
        if found.is_empty() && query.is_empty() {
            rows.push(Row::Note { text: "No files yet: + creates one", depth: 0 });
        }
        rows.extend(found);
        rows.push(Row::Section(Section::Files));
        let mut found = vec![];
        for f in self.files.clone() {
            self.emit_file(&f, 0, query, true, &mut found);
        }
        if found.is_empty() && query.is_empty() {
            rows.push(Row::Note { text: "+ adds .http / .rest files from anywhere", depth: 0 });
        }
        rows.extend(found);
        rows
    }

    fn emit(&mut self, tree: &Tree, base: &Path, depth: usize, query: &str, rows: &mut Vec<Row>) {
        let searching = !query.is_empty();
        for (name, node) in &tree.dirs {
            // Compact folders: a chain of single-child folders shows as `a/b/c`.
            let (mut node, mut path, mut label) = (node, base.join(name), name.clone());
            while node.files.is_empty() && node.dirs.len() == 1 {
                let (child, next) = node.dirs.iter().next().unwrap();
                (node, path, label) = (next, path.join(child), format!("{label}/{child}"));
            }
            // `Demo/Demo.http` alone in its folder (an imported collection) shows as just the file.
            if node.dirs.is_empty() && node.files.len() == 1 && node.files[0].file_stem() == path.file_name() {
                self.emit(node, &path, depth, query, rows);
                continue;
            }
            if searching {
                let mut found = vec![];
                self.emit(node, &path, depth + 1, query, &mut found);
                if !found.is_empty() {
                    rows.push(Row::Dir { path: path.clone(), name: label, depth });
                    rows.extend(found);
                }
                continue;
            }
            rows.push(Row::Dir { path: path.clone(), name: label, depth });
            if !self.collapsed_dirs.contains(&path) {
                self.emit(node, &path, depth + 1, query, rows);
            }
        }
        for f in &tree.files {
            self.emit_file(f, depth, query, false, rows);
        }
    }

    fn emit_file(&mut self, f: &Path, depth: usize, query: &str, added: bool, rows: &mut Vec<Row>) {
        let outline = self.outline_of(f);
        let name = f.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let path = f.to_path_buf();
        if !query.is_empty() {
            // A file-name match keeps all its requests; otherwise only the matching ones.
            let file_hit = name.to_lowercase().contains(query);
            let hits: Vec<_> =
                outline.into_iter().filter(|(_, m, l)| file_hit || l.to_lowercase().contains(query) || m.to_lowercase().contains(query)).collect();
            if hits.is_empty() && !file_hit {
                return;
            }
            rows.push(Row::File { path: path.clone(), name, depth, requests: hits.len(), added });
            rows.extend(hits.into_iter().map(|(line, method, label)| Row::Request { path: path.clone(), line, method, label, depth: depth + 1 }));
            return;
        }
        rows.push(Row::File { path: path.clone(), name, depth, requests: outline.len(), added });
        if self.expanded.contains(&path) {
            rows.extend(outline.into_iter().map(|(line, method, label)| Row::Request { path: path.clone(), line, method, label, depth: depth + 1 }));
        }
    }

    /// New file / new folder buttons for `dir` (a folder row or the Collections header).
    fn folder_actions(&self, ix: usize, dir: &Path, always: bool, cx: &mut Context<Self>) -> [Stateful<Div>; 3] {
        let size = cx.theme().font_size * 0.95;
        let (d1, d2, d3) = (dir.to_path_buf(), dir.to_path_buf(), dir.to_path_buf());
        [
            row_action(("import", ix), Icon::default().data(include_bytes!("../assets/import.svg")).size(size), "Import Files…", always, cx).on_click(
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.import_into(d3.clone(), window, cx);
                }),
            ),
            row_action(("new-file", ix), Icon::default().data(include_bytes!("../assets/file-plus.svg")).size(size), "New File", always, cx).on_click(
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.new_file(&d1, window, cx);
                }),
            ),
            row_action(("new-folder", ix), Icon::default().data(include_bytes!("../assets/folder-plus.svg")).size(size), "New Folder", always, cx).on_click(
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.new_folder(&d2, window, cx);
                }),
            ),
        ]
    }

    /// Trash button on a file or folder of the central folder.
    fn trash_action(&self, ix: usize, path: &Path, cx: &mut Context<Self>) -> Stateful<Div> {
        let size = cx.theme().font_size * 0.95;
        let p = path.to_path_buf();
        row_action(("trash", ix), Icon::default().data(include_bytes!("../assets/trash.svg")).size(size), "Move to Trash", false, cx).on_click(cx.listener(
            move |this, _, window, cx| {
                cx.stop_propagation();
                this.trash(p.clone(), window, cx);
            },
        ))
    }

    /// A file goes to the Trash at once; a folder asks first.
    fn trash(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !path.is_dir() {
            self.move_to_trash(&path, window, cx);
            return;
        }
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let message = format!("Move the folder “{name}” to the Trash?");
        let answer = window.prompt(PromptLevel::Warning, &message, Some("Everything in it goes too."), &["Move to Trash", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| this.move_to_trash(&path, window, cx)).ok();
        })
        .detach();
    }

    fn move_to_trash(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        // Unsaved edits aren't in the Trash copy: the user saves or closes the tab first.
        if let Some(d) = self.docs.iter().find(|d| d.path.starts_with(path) && d.dirty) {
            window.push_notification(format!("Unsaved changes in {}. Save or close it first.", d.name()), cx);
            return;
        }
        if let Err(e) = trash::delete(path) {
            window.push_notification(format!("Couldn't move {} to the Trash: {e}", path.display()), cx);
            return;
        }
        while let Some(ix) = self.docs.iter().position(|d| d.path.starts_with(path)) {
            self.close(ix, window, cx);
        }
        self.central = None;
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        window.push_notification(format!("Moved {name} to the Trash"), cx);
        cx.notify();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, hover, active, guide) = (t.muted_foreground, t.list_hover, t.list_active, t.sidebar_border);
        let row = &self.rows[ix];
        let depth = row.depth();
        // Indent guides: one hairline per ancestor level, centred under its chevron.
        let guides = (0..depth).map(move |i| {
            div().absolute().top_0().bottom_0().left(px(PAD + i as f32 * INDENT + CHEVRON / 2.)).w(px(1.)).bg(guide)
        });
        let base = div()
            .id(("row", ix))
            .relative()
            .w_full()
            .h(px(ROW_H))
            .pl(px(PAD + depth as f32 * INDENT))
            .pr_2()
            .rounded_md()
            .flex()
            .items_center()
            .gap_1p5()
            .text_sm()
            .cursor_pointer()
            .group("row")
            .hover(move |s| s.bg(hover))
            .children(guides);
        let spacer = || div().w(px(CHEVRON)).flex_none();
        let icon = |name: IconName, color: Hsla| Icon::new(name).size_3p5().flex_none().text_color(color);

        // Inset by 4 px on both sides, so selection and hover read as rounded pills.
        let row = match row {
            Row::Section(section) => {
                let title = match section {
                    Section::Collections => "COLLECTIONS",
                    Section::Files => "FILES",
                };
                let head = base
                    .cursor_default()
                    .when(ix > 0, |r| r.mt_2())
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(div().flex_1().child(title));
                match section {
                    Section::Collections => head.children(self.folder_actions(ix, &workspace::collections_dir(), true, cx)),
                    Section::Files => head.child(
                        row_action("add-files", Icon::new(IconName::Plus).size_3p5(), "Add .http / .rest Files…", true, cx)
                            .on_click(cx.listener(|this, _, window, cx| this.add_files(window, cx))),
                    ),
                }
                .into_any_element()
            }
            Row::Dir { path, name, .. } => {
                let open = !self.collapsed_dirs.contains(path);
                let p = path.clone();
                base.child(chevron(open, muted))
                    .child(icon(if open { IconName::FolderOpen } else { IconName::Folder }, muted))
                    .child(div().flex_1().truncate().child(name.clone()))
                    .children(self.folder_actions(ix, path, false, cx))
                    .child(self.trash_action(ix, path, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_dir(&p);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Row::File { path, name, requests, added, .. } => {
                let doc = self.docs.iter().position(|d| *d.path == *path);
                let current = doc.is_some() && doc == self.current;
                let dirty = doc.is_some_and(|i| self.docs[i].dirty);
                let open = self.expanded.contains(path);
                let (p, q, r) = (path.clone(), path.clone(), path.clone());
                let toggle = div()
                    .id(("file-chevron", ix))
                    .flex_none()
                    .child(chevron(open, muted))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle(&q);
                        cx.notify();
                    }));
                let meta = if dirty { "●".to_string() } else { requests.to_string() };
                let remove = if *added {
                    row_action(("remove-file", ix), Icon::new(IconName::Close).size_3(), "Remove from the List (the file stays)", false, cx).on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.remove_file(&r, window, cx);
                    }))
                } else {
                    self.trash_action(ix, path, cx)
                };
                base.when(current, |r| r.bg(active))
                    .child(toggle)
                    .child(icon(IconName::FileText, if current { cx.theme().foreground } else { muted }))
                    .child(div().flex_1().truncate().child(name.clone()))
                    .child(remove)
                    .child(div().flex_none().text_xs().text_color(muted).child(meta))
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
            Row::Request { path, line, method, label, .. } => {
                let doc = self.docs.iter().position(|d| *d.path == *path);
                let selected = doc.is_some() && doc == self.current && doc.is_some_and(|i| self.docs[i].active == Some(*line));
                let status = doc.and_then(|i| self.docs[i].status.get(line).cloned()).map(|s| match s {
                    Status::Done(ex) => (ex.response.status.to_string(), self.status_color(ex.response.status, cx)),
                    Status::Failed(_) => ("ERR".into(), t.danger),
                    Status::Running => ("…".into(), t.warning),
                });
                let (p, line) = (path.clone(), *line);
                // Fixed right-aligned column, so labels line up whatever the method length.
                let badge = div()
                    .w(px(44.)) // fits DELETE / OPTIONS
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_size(px(9.5))
                    .font_weight(FontWeight::BOLD)
                    .font_family(t.mono_font_family.clone())
                    .text_color(method_color(method, cx))
                    .child(method.clone());
                base.when(selected, |r| r.bg(active))
                    .child(badge)
                    .child(div().flex_1().truncate().child(label.clone()))
                    .children(status.map(|(s, c)| div().flex_none().text_xs().font_family(t.mono_font_family.clone()).text_color(c).child(s)))
                    // Click jumps to the request; double click also runs it.
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| this.go_to(p.clone(), line, ev.click_count() >= 2, window, cx)))
                    .into_any_element()
            }
            Row::Note { text, .. } => base.cursor_default().child(spacer()).text_xs().text_color(muted).child(*text).into_any_element(),
        };
        div().w_full().px_1().child(row).into_any_element()
    }

    /// ↑/↓ in the search field: select the previous / next request row, keeping focus in the field.
    fn step_search_hit(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let reqs: Vec<(PathBuf, usize)> =
            self.rows.iter().filter_map(|r| if let Row::Request { path, line, .. } = r { Some((path.clone(), *line)) } else { None }).collect();
        if reqs.is_empty() {
            return;
        }
        let cur = self.doc().and_then(|d| d.active.map(|l| (d.path.to_path_buf(), l)));
        let next = match cur.and_then(|c| reqs.iter().position(|r| *r == c)) {
            Some(i) => (i as isize + step).clamp(0, reqs.len() as isize - 1) as usize,
            None if step > 0 => 0,
            None => reqs.len() - 1,
        };
        let (path, line) = reqs[next].clone();
        self.go_to(path, line, false, window, cx);
        self.sidebar_filter.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.sidebar_filter.read(cx).value().trim().to_lowercase();
        self.rows = self.build_rows(&query);
        let list = uniform_list("sidebar", self.rows.len(), cx.processor(|this, range: Range<usize>, _, cx| {
            range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
        }));
        let t = cx.theme();
        let (muted, hover) = (t.muted_foreground, t.list_hover);
        let search = div().flex_none().px_2().pt_2().pb_1p5().capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
            let step = match ev.keystroke.key.as_str() {
                "up" => -1,
                "down" => 1,
                _ => return,
            };
            cx.stop_propagation();
            this.step_search_hit(step, window, cx);
        })).child(Input::new(&self.sidebar_filter).small().cleanable(true));
        let item = |id: &'static str, icon: IconName, label: &'static str| {
            div()
                .id(id)
                .flex_1()
                .h(px(30.))
                .px_2()
                .rounded_md()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(muted)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(Icon::new(icon).size_3p5())
                .child(label)
        };
        let footer = div()
            .flex_none()
            .mx_1()
            .mb_1()
            .flex()
            .gap_1()
            .child(item("open-settings", IconName::Settings, "Settings").on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))))
            .child(item("open-history", IconName::RotateCw, "History").on_click(cx.listener(|this, _, _, cx| this.toggle_history(cx))));
        div()
            .size_full()
            .flex()
            .flex_col()
            // Background and border come from the card it sits in (main.rs `card`).
            .text_color(t.sidebar_foreground)
            .child(search)
            .child(div().flex_1().min_h_0().child(list.size_full()))
            .child(div().flex_none().h(px(1.)).mx_2().bg(t.sidebar_border))
            .child(footer)
            .into_any_element()
    }
}

fn chevron(open: bool, color: Hsla) -> impl IntoElement {
    div()
        .w(px(CHEVRON))
        .flex_none()
        .flex()
        .justify_center()
        .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).size_3().text_color(color))
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

pub(crate) fn method_color(method: &str, cx: &App) -> Hsla {
    match method {
        "GET" | "HEAD" | "OPTIONS" => cx.theme().success,
        "POST" => cx.theme().warning,
        "DELETE" => cx.theme().danger,
        _ => cx.theme().primary,
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's glob import brings its own `test` macro.
    use super::Tree;
    use std::path::{Path, PathBuf};

    #[test]
    fn tree_groups_by_folder() {
        let root = Path::new("/w");
        let entries = ["/w/a.http", "/w/api", "/w/api/v1", "/w/api/v1/z.http", "/w/crates/app/x.http", "/w/crates/app/y.rest", "/w/empty"]
            .map(PathBuf::from);
        let t = Tree::build(root, &entries);
        assert_eq!(t.files, [PathBuf::from("/w/a.http")]);
        assert_eq!(t.dirs.keys().collect::<Vec<_>>(), ["api", "crates", "empty"]);
        assert_eq!(t.dirs["crates"].dirs["app"].files.len(), 2);
        assert_eq!(t.dirs["api"].dirs["v1"].files, [PathBuf::from("/w/api/v1/z.http")]);
        assert!(t.dirs["empty"].files.is_empty() && t.dirs["empty"].dirs.is_empty()); // a new folder shows
    }
}
