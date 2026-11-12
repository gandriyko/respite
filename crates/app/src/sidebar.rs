//! Sidebar: projects → folders → `.http` files → requests.
//!
//! Styled after the VS Code / Zed project panels: disclosure chevrons, indent guides,
//! compact single-child folders (`crates/app`), inset rounded selection, a fixed
//! right-aligned method column (as in Bruno / Insomnia) and muted meta on the right.

use std::{
    collections::BTreeMap,
    ops::Range,
    path::{Path, PathBuf},
};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{ActiveTheme as _, Icon, IconName, WindowExt as _};
use http_exec::Store;

use crate::{
    Respite,
    doc::Status,
    workspace::{self, Project},
};

const ROW_H: f32 = 24.;
const INDENT: f32 = 14.;
/// Left padding before the first chevron.
const PAD: f32 = 6.;
/// Chevron column width; leaves get a spacer of the same width.
const CHEVRON: f32 = 14.;

pub(crate) enum Row {
    Project(usize),
    Dir { path: PathBuf, name: String, depth: usize },
    File { path: PathBuf, name: String, depth: usize, requests: usize },
    Request { path: PathBuf, line: usize, method: String, label: String, depth: usize },
    Note { text: &'static str, depth: usize },
}

impl Row {
    fn depth(&self) -> usize {
        match self {
            Row::Project(_) => 0,
            Row::Dir { depth, .. } | Row::File { depth, .. } | Row::Request { depth, .. } | Row::Note { depth, .. } => *depth,
        }
    }
}

/// Folder tree of one project's files.
#[derive(Default)]
struct Tree {
    dirs: BTreeMap<String, Tree>,
    files: Vec<PathBuf>,
}

impl Tree {
    fn build(root: &Path, files: &[PathBuf]) -> Self {
        let mut tree = Tree::default();
        for f in files {
            let rel = f.strip_prefix(root).unwrap_or(f);
            let dirs = rel.parent().into_iter().flat_map(|p| p.components()).map(|c| c.as_os_str().to_string_lossy().into_owned());
            dirs.fold(&mut tree, |node, d| node.dirs.entry(d).or_default()).files.push(f.clone());
        }
        tree
    }
}

impl Respite {
    pub(crate) fn add_project(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add Project".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(root) = paths.into_iter().next() else { return };
            this.update(cx, |this, cx| {
                if !this.projects.iter().any(|p| p.root == root) {
                    this.projects.push(Project::new(root.clone()));
                    this.save_projects();
                }
                if let Some(p) = this.projects.iter_mut().find(|p| p.root == root) {
                    p.files = Some(workspace::scan(&root));
                }
                this.expanded.insert(root);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Postman / Insomnia / OpenAPI / Bruno / HAR → a new folder in the collections dir.
    pub(crate) fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let collections = workspace::collections_dir();
        let current_root = self.doc().map(|d| self.root_of(&d.path));
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            // Bruno collections are folders.
            directories: true,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(src) = paths.into_iter().next() else { return };
            this.update_in(cx, |this, window, cx| {
                let result = http_import::import(&src).and_then(|c| {
                    // A collection becomes a new folder in the collections dir. Environments alone
                    // go to the open file's project, else to a collection of their own: never to
                    // the collections dir itself, where they'd apply to nothing visible.
                    let dest = match (http_import::render(&c).is_empty(), current_root) {
                        (false, _) => collections.clone(),
                        (true, Some(root)) => root,
                        (true, None) => collections.join(http_import::sanitize(&c.name)),
                    };
                    http_import::write(&c, &dest).map(|f| (c, f, dest)).map_err(|e| e.to_string())
                });
                let (c, files, dest) = match result {
                    Ok(r) => r,
                    Err(e) => {
                        window.push_notification(format!("Import failed: {e}"), cx);
                        return;
                    }
                };
                this.rescan_projects_of(&dest);
                this.expanded.insert(dest.clone());
                if let Some(first) = files.iter().find(|f| f.extension().is_some_and(|e| e == "http")).or(files.first()) {
                    this.open(first.clone(), None, window, cx);
                }
                let mut msg = format!("Imported {} '{}': {} file(s)", c.format, c.name, files.len());
                if !c.warnings.is_empty() {
                    msg += &format!(". {} warning(s): {}", c.warnings.len(), c.warnings.join("; "));
                }
                window.push_notification(msg, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Hover `+`: a new `.http` file in `dir`, named in the system save dialog.
    fn add_button(&self, ix: usize, dir: PathBuf, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(("add-file", ix))
            .flex_none()
            .px_1()
            .rounded_sm()
            .text_color(cx.theme().muted_foreground)
            .invisible()
            .group_hover("row", |s| s.visible())
            .hover(|s| s.bg(cx.theme().list_active))
            .child("+")
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.new_file(&dir, window, cx);
            }))
    }

    fn new_file(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_new_path(dir, Some("requests.http"));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut path))) = picked.await else { return };
            if path.extension().is_none_or(|e| e != "http" && e != "rest") {
                path.set_extension("http");
            }
            this.update_in(cx, |this, window, cx| {
                // The dialog already confirmed overwriting; an existing file is just opened, never truncated.
                if !path.exists()
                    && let Err(e) = std::fs::write(&path, "### New request\nGET https://\n")
                {
                    window.push_notification(format!("{}: {e}", path.display()), cx);
                    return;
                }
                this.rescan_projects_of(&path);
                this.open(path, None, window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Drops the project from the sidebar list only; files and history stay.
    fn remove_project(&mut self, pi: usize, window: &mut Window, cx: &mut Context<Self>) {
        if pi >= self.projects.len() {
            return;
        }
        let p = self.projects.remove(pi);
        self.expanded.remove(&p.root);
        self.save_projects();
        window.push_notification(format!("Removed {} from the list. Files and history are kept.", p.name), cx);
        cx.notify();
    }

    fn save_projects(&self) {
        let roots: Vec<PathBuf> = self.projects.iter().filter(|p| !p.flat).map(|p| p.root.clone()).collect();
        workspace::save_list(Store::default_base().as_deref(), &roots);
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

    fn toggle_project(&mut self, pi: usize, cx: &mut Context<Self>) {
        let p = &mut self.projects[pi];
        if p.files.is_none() || !self.expanded.contains(&p.root) {
            p.files = Some(workspace::scan(&p.root)); // rescan on expand: picks up new files
        }
        let root = p.root.clone();
        self.toggle(&root);
        cx.notify();
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

    fn build_rows(&mut self) -> Vec<Row> {
        let mut rows = vec![];
        for pi in 0..self.projects.len() {
            let p = &self.projects[pi];
            let (root, files, flat) = (p.root.clone(), p.files.clone().unwrap_or_default(), p.flat);
            if flat {
                self.emit(&Tree::build(&root, &files), &root, 0, &mut rows);
                continue;
            }
            rows.push(Row::Project(pi));
            if !self.expanded.contains(&root) {
                continue;
            }
            if files.is_empty() {
                rows.push(Row::Note { text: "No .http or .rest files", depth: 1 });
            }
            self.emit(&Tree::build(&root, &files), &root, 1, &mut rows);
        }
        rows
    }

    fn emit(&mut self, tree: &Tree, base: &Path, depth: usize, rows: &mut Vec<Row>) {
        for (name, node) in &tree.dirs {
            // Compact folders: a chain of single-child folders shows as `a/b/c`.
            let (mut node, mut path, mut label) = (node, base.join(name), name.clone());
            while node.files.is_empty() && node.dirs.len() == 1 {
                let (child, next) = node.dirs.iter().next().unwrap();
                (node, path, label) = (next, path.join(child), format!("{label}/{child}"));
            }
            // `Demo/Demo.http` alone in its folder (an imported collection) shows as just the file.
            if node.dirs.is_empty() && node.files.len() == 1 && node.files[0].file_stem() == path.file_name() {
                self.emit(node, &path, depth, rows);
                continue;
            }
            rows.push(Row::Dir { path: path.clone(), name: label, depth });
            if !self.collapsed_dirs.contains(&path) {
                self.emit(node, &path, depth + 1, rows);
            }
        }
        for f in &tree.files {
            let outline = if workspace::is_env_file(f) { vec![] } else { self.outline_of(f) };
            let name = f.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            rows.push(Row::File { path: f.clone(), name, depth, requests: outline.len() });
            if self.expanded.contains(f) {
                rows.extend(outline.into_iter().map(|(line, method, label)| Row::Request { path: f.clone(), line, method, label, depth: depth + 1 }));
            }
        }
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
            Row::Project(pi) => {
                let (p, pi) = (&self.projects[*pi], *pi);
                let open = self.expanded.contains(&p.root);
                let remove = div()
                    .id(("remove-project", pi))
                    .flex_none()
                    .px_1()
                    .rounded_sm()
                    .text_color(muted)
                    .invisible()
                    .group_hover("row", |s| s.visible())
                    .hover(move |s| s.bg(active))
                    .child("×")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.remove_project(pi, window, cx);
                    }));
                base.when(pi > 0, |r| r.mt_1())
                    .child(chevron(open, muted))
                    .child(icon(if open { IconName::FolderOpen } else { IconName::Folder }, t.primary))
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).truncate().child(p.name.clone()))
                    .child(self.add_button(ix, p.root.clone(), cx))
                    .child(remove)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_project(pi, cx)))
                    .into_any_element()
            }
            Row::Dir { path, name, .. } => {
                let open = !self.collapsed_dirs.contains(path);
                let p = path.clone();
                base.child(chevron(open, muted))
                    .child(icon(if open { IconName::FolderOpen } else { IconName::Folder }, muted))
                    .child(div().flex_1().truncate().child(name.clone()))
                    .child(self.add_button(ix, path.clone(), cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_dir(&p);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Row::File { path, name, requests, .. } => {
                let doc = self.docs.iter().position(|d| *d.path == *path);
                let current = doc.is_some() && doc == self.current;
                let dirty = doc.is_some_and(|i| self.docs[i].dirty);
                let open = self.expanded.contains(path);
                let (p, q) = (path.clone(), path.clone());
                let toggle = div()
                    .id(("file-chevron", ix))
                    .flex_none()
                    .child(chevron(open, muted))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle(&q);
                        cx.notify();
                    }));
                let env = workspace::is_env_file(path);
                let meta = match (dirty, env) {
                    (true, _) => "●".to_string(),
                    (false, true) => String::new(),
                    (false, false) => requests.to_string(),
                };
                base.when(current, |r| r.bg(active))
                    .map(|r| if env { r.child(spacer()) } else { r.child(toggle) })
                    .child(icon(if env { IconName::Globe } else { IconName::FileText }, if current { t.foreground } else { muted }))
                    .child(div().flex_1().truncate().child(name.clone()))
                    // Next to the file: also how a collapsed `Demo/Demo.http` gets a sibling.
                    .children(path.parent().map(|d| self.add_button(ix, d.to_path_buf(), cx)))
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

    pub(crate) fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.rows = self.build_rows();
        let list = uniform_list("sidebar", self.rows.len(), cx.processor(|this, range: Range<usize>, _, cx| {
            range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
        }));
        let t = cx.theme();
        let (muted, hover) = (t.muted_foreground, t.list_hover);
        let icon_button = |id: &'static str, name: IconName| {
            div().id(id).p_1().rounded_md().cursor_pointer().text_color(muted).hover(move |s| s.bg(hover)).child(Icon::new(name).size_3p5())
        };
        let header = div()
            .flex_none()
            .h(px(32.))
            .pl_3()
            .pr_1p5()
            .flex()
            .items_center()
            .justify_between()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(muted)
            .child("PROJECTS")
            .child(icon_button("add-project", IconName::Plus).on_click(cx.listener(|this, _, _, cx| this.add_project(cx))));
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
            .bg(t.sidebar)
            .text_color(t.sidebar_foreground)
            .child(header)
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
        let files = ["/w/a.http", "/w/crates/app/x.http", "/w/crates/app/y.rest", "/w/api/v1/z.http"].map(PathBuf::from);
        let t = Tree::build(root, &files);
        assert_eq!(t.files, [PathBuf::from("/w/a.http")]);
        assert_eq!(t.dirs.keys().collect::<Vec<_>>(), ["api", "crates"]);
        assert_eq!(t.dirs["crates"].dirs["app"].files.len(), 2);
        assert_eq!(t.dirs["api"].dirs["v1"].files, [PathBuf::from("/w/api/v1/z.http")]);
    }
}
