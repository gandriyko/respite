//! Response panel: header (status, runs ‹ i/N ›, Copy / Save / Diff / Pin), tabs (Body, Headers,
//! Cookies, Request, Timeline, Tests), image / binary / too-big bodies, the JSONPath filter,
//! the JSON path under the cursor, and the diff against the previous run.

use std::{fmt::Write as _, path::Path, sync::Arc};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, Size, WindowExt as _,
    button::{Button, ButtonGroup, ButtonVariants as _},
    diff::{Diff, DiffFile, DiffMode, DiffState},
    input::{Editor, Input},
    progress::ProgressCircle,
    tab::{Tab, TabBar},
    v_flex,
};
use http_exec::{Exchange, Response};

use crate::{BodyOther, Older, Respite, Shown, Status, sidebar, size_label};

pub const TABS: [&str; 6] = ["Body", "Headers", "Cookies", "Request", "Timeline", "Tests"];

pub fn header<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
    r.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// How the Body tab shows a response.
pub enum BodyView {
    /// Highlighter language and text.
    Text(&'static str, String),
    Image(ImageFormat),
    /// Not text: size and type only (and Save).
    Binary,
    /// Over `response.preview_limit_mb`: only Save.
    TooBig,
}

pub struct Limits {
    pub format: usize,
    pub preview: usize,
}

/// `raw` skips pretty-printing; `filter` is a JSONPath applied to a JSON body.
pub fn body_view(r: &Response, raw: bool, filter: &str, limits: &Limits) -> BodyView {
    let ct = header(r, "content-type").unwrap_or("").to_ascii_lowercase();
    let mime = ct.split(';').next().unwrap_or("").trim();
    if r.body.len() > limits.preview {
        return BodyView::TooBig;
    }
    if mime.starts_with("image/")
        && let Some(f) = ImageFormat::from_mime_type(mime)
    {
        return BodyView::Image(f);
    }
    let Ok(text) = std::str::from_utf8(&r.body) else {
        return BodyView::Binary;
    };
    let json = ct.contains("json") || serde_json::from_str::<serde_json::Value>(text).is_ok();
    let lang = if json {
        "json"
    } else if ct.contains("html") || ct.contains("xml") {
        "html"
    } else {
        "text"
    };
    if json && !filter.trim().is_empty() {
        return BodyView::Text("json", jsonpath(text, filter.trim()).unwrap_or_else(|e| format!("// {e}")));
    }
    if raw || r.body.len() > limits.format {
        return BodyView::Text(lang, cap(text.to_string()));
    }
    let pretty = json.then(|| serde_json::from_str::<serde_json::Value>(text).ok().and_then(|v| serde_json::to_string_pretty(&v).ok())).flatten();
    BodyView::Text(lang, cap(pretty.unwrap_or_else(|| text.to_string())))
}

/// Soft wrap measures every line on the UI thread when the text is set (measured: ~3 s for
/// 23 MB of pretty JSON, ~0.15 s unwrapped); bigger bodies don't wrap.
pub const WRAP_MAX: usize = 1024 * 1024;

/// The editor lays out only the visible lines, but each of them whole: a megabytes-long line
/// (minified JSON shown raw) freezes it. Any number of short lines is fine; a body with a
/// too-long line is cut, and Save has all of it.
fn cap(text: String) -> String {
    const CAP: usize = 256 * 1024;
    if text.len() <= CAP || text.lines().all(|l| l.len() <= CAP) {
        return text;
    }
    let end = (0..=CAP).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    format!("{}\n… (truncated, {} B; Save shows all of it)", &text[..end], text.len())
}

/// Matches of `expr` as a pretty JSON array.
pub fn jsonpath(text: &str, expr: &str) -> Result<String, String> {
    let path = serde_json_path::JsonPath::parse(expr).map_err(|e| format!("JSONPath: {e}"))?;
    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let found: Vec<_> = path.query(&json).all().into_iter().cloned().collect();
    serde_json::to_string_pretty(&found).map_err(|e| e.to_string())
}

/// JSONPath (`$.items[2].id`) of the JSON node at byte `offset` of `text`.
pub fn path_at(text: &str, offset: usize) -> Option<String> {
    #[derive(Debug)]
    struct Frame {
        array: bool,
        index: usize,
        key: Option<String>,
        /// In an object: the next string is a key.
        want_key: bool,
    }
    let render = |stack: &[Frame]| {
        let mut s = String::from("$");
        for f in stack {
            if f.array {
                let _ = write!(s, "[{}]", f.index);
            } else if let Some(k) = &f.key {
                if !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit()) {
                    let _ = write!(s, ".{k}");
                } else {
                    let _ = write!(s, "['{}']", k.replace('\'', "\\'"));
                }
            }
        }
        s
    };
    let b = text.as_bytes();
    let mut stack: Vec<Frame> = vec![];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                let end = (i + 1).min(b.len());
                if let Some(f) = stack.last_mut().filter(|f| !f.array && f.want_key) {
                    f.key = serde_json::from_str(&text[start..end]).ok();
                    f.want_key = false;
                }
                if offset < end {
                    return (!stack.is_empty() || offset >= start).then(|| render(&stack));
                }
                i = end;
                continue;
            }
            _ if i >= offset && !b[i].is_ascii_whitespace() => return Some(render(&stack)),
            b'{' | b'[' => stack.push(Frame { array: b[i] == b'[', index: 0, key: None, want_key: b[i] == b'{' }),
            b'}' | b']' => {
                stack.pop();
            }
            b',' => {
                if let Some(f) = stack.last_mut() {
                    if f.array {
                        f.index += 1;
                    } else {
                        (f.key, f.want_key) = (None, true);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    Some(render(&stack))
}

/// Cookies tab: what the server set, and what the jar sent.
pub fn cookies_text(r: &Response) -> String {
    let set: Vec<&str> = r.headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("set-cookie")).map(|(_, v)| v.as_str()).collect();
    let mut s = String::from("# Received (Set-Cookie)\n");
    if set.is_empty() {
        s += "# none\n";
    }
    for c in set {
        let mut parts = c.split(';').map(str::trim);
        let first = parts.next().unwrap_or("");
        let (name, value) = first.split_once('=').unwrap_or((first, ""));
        let _ = writeln!(s, "{name}: {value}");
        for attr in parts.filter(|a| !a.is_empty()) {
            let _ = writeln!(s, "#   {attr}");
        }
    }
    s += "\n# Sent from the cookie jar\n";
    if r.cookies_sent.is_empty() {
        s += "# none\n";
    }
    for (n, v) in &r.cookies_sent {
        let _ = writeln!(s, "{n}: {v}");
    }
    s
}

/// Timeline tab: redirects, then timings.
pub fn timeline_text(ex: &Exchange) -> String {
    let r = &ex.response;
    let mut s = String::new();
    if !r.redirects.is_empty() {
        s += "# Redirects\n";
        for h in &r.redirects {
            let _ = writeln!(s, "{} {}\n#   → {}", h.status, h.from, h.to);
        }
        s += "\n";
    }
    let url = if r.url.is_empty() { &ex.request.url } else { &r.url };
    let _ = writeln!(s, "# Response\n{} {} {}\n#   from {url}\n", r.status, r.reason, r.http_version);
    let (ttfb, total) = (r.ttfb.as_millis(), r.total.as_millis());
    let _ = writeln!(s, "# Timings\nWaiting (TTFB): {ttfb} ms\nDownload: {} ms\nTotal: {total} ms", total.saturating_sub(ttfb));
    s += "# DNS, TCP and TLS are not measured separately yet\n";
    s
}

/// Tests tab: `# @assert` results; scripts are recognised but not run yet.
pub fn tests_text(ex: &Exchange, has_scripts: bool) -> String {
    let mut s = String::new();
    if ex.tests.is_empty() {
        s += "# No tests. Add checks above the request line:\n#   # @assert status == 200\n#   # @assert body $.id exists\n";
    } else {
        let failed = ex.tests.iter().filter(|t| !t.passed).count();
        let _ = writeln!(s, "# {} passed, {failed} failed", ex.tests.len() - failed);
        for t in &ex.tests {
            let _ = writeln!(s, "{} {}  (line {})", if t.passed { "✓" } else { "✗" }, t.text, t.line);
            if !t.passed {
                let _ = writeln!(s, "    {}", t.detail);
            }
        }
    }
    if has_scripts {
        s += "# Response handler scripts (> {% … %}) are not executed yet.\n";
    }
    s
}

/// Unified diff of two bodies (pretty JSON when both are JSON), for the diff view.
pub fn diff(prev: &[u8], cur: &[u8]) -> String {
    let show = |b: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(b)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| String::from_utf8_lossy(b).into_owned())
            + "\n"
    };
    let (a, b) = (show(prev), show(cur));
    // Whole bodies as context: the view folds unchanged lines itself and can unfold them all.
    similar::TextDiff::from_lines(&a, &b).unified_diff().context_radius(usize::MAX / 2).header("previous", "current").to_string()
}

/// History time (RFC 3339, UTC) in local time: `15:17:01` today, else `Nov 11 15:17`.
fn run_time(time: &str) -> String {
    let Ok(t) = time.parse::<jiff::Timestamp>() else { return String::new() };
    let tz = jiff::tz::TimeZone::system();
    let (z, now) = (t.to_zoned(tz.clone()), jiff::Timestamp::now().to_zoned(tz));
    if z.date() == now.date() { z.strftime("%H:%M:%S").to_string() } else { z.strftime("%b %-d %H:%M").to_string() }
}

/// A file name for Save: the URL's last segment when it has an extension, else by type.
fn suggested_name(ex: &Exchange) -> String {
    let url = if ex.response.url.is_empty() { &ex.request.url } else { &ex.response.url };
    let last = url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
    if last.contains('.') && !last.ends_with('.') {
        return last.to_string();
    }
    let ct = header(&ex.response, "content-type").unwrap_or("").to_ascii_lowercase();
    let ext = [("json", "json"), ("xml", "xml"), ("html", "html"), ("png", "png"), ("jpeg", "jpg"), ("gif", "gif"), ("webp", "webp"), ("svg", "svg"), ("pdf", "pdf"), ("text/", "txt")]
        .iter()
        .find(|(k, _)| ct.contains(k))
        .map_or("bin", |(_, e)| e);
    format!("response.{ext}")
}

impl Respite {
    /// What the panel shows: the picked entry on the History page, else the pinned response,
    /// else the request under the cursor (an older run of it when one was picked with ‹ ›).
    pub(crate) fn shown_now(&self) -> Option<Shown> {
        if self.show_history {
            let (e, ex) = self.history_pick.as_ref()?;
            return Some(Shown { head: (e.method.clone(), e.label.clone()), status: Some(Status::Done(ex.clone())), has_scripts: false });
        }
        if let Some(p) = &self.pinned {
            return Some(p.clone());
        }
        let d = self.doc()?;
        let r = d.file.requests.iter().find(|r| Some(r.request_line) == d.active)?;
        let mut status = d.status.get(&r.request_line).cloned();
        if let (Some(Status::Done(latest)), Some(o)) = (&status, &self.older)
            && Arc::ptr_eq(latest, &o.latest)
        {
            status = Some(Status::Done(o.ex.clone()));
        }
        Some(Shown { head: (r.method.clone(), r.label()), status, has_scripts: r.has_scripts })
    }

    /// The history of the request under the cursor, reloaded when its newest run changes.
    // ponytail: `entries_of` reads every entry file (≤ history.total); index it if that shows up.
    fn sync_runs(&mut self) {
        let follow = !self.show_history && self.pinned.is_none();
        let found = self.doc().filter(|_| follow).and_then(|d| {
            let r = d.file.requests.iter().find(|r| Some(r.request_line) == d.active)?;
            let Some(Status::Done(latest)) = d.status.get(&r.request_line) else { return None };
            Some((latest.clone(), r.label(), d.path.clone(), d.exec.clone()))
        });
        let Some((latest, label, path, exec)) = found else {
            self.runs = None;
            return;
        };
        if self.runs.as_ref().is_some_and(|(l, _)| Arc::ptr_eq(l, &latest)) {
            return;
        }
        let entries = exec.store().map(|s| s.entries_of(&path).into_iter().filter(|e| e.label == label).collect()).unwrap_or_default();
        self.runs = Some((latest, entries));
    }

    /// (index of `ex` among the runs, newest first; how many there are).
    fn run_position(&self, ex: &Exchange) -> Option<(usize, usize)> {
        let (_, entries) = self.runs.as_ref()?;
        let id = ex.history_id.as_ref()?;
        Some((entries.iter().position(|e| e.id == *id)?, entries.len()))
    }

    /// ‹ (older) / › (newer) run of the shown request.
    fn step_run(&mut self, older: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Shown { status: Some(Status::Done(ex)), .. }) = self.shown_now() else { return };
        let Some((i, n)) = self.run_position(&ex) else { return };
        let j = if older { i + 1 } else { i.wrapping_sub(1) };
        let (Some((latest, entries)), Some(store)) = (&self.runs, self.doc().and_then(|d| d.exec.store())) else { return };
        let Some(entry) = entries.get(j).filter(|_| j < n) else { return };
        if latest.history_id.as_ref() == Some(&entry.id) {
            self.older = None;
        } else {
            match store.exchange(entry.clone()) {
                Some(old) => self.older = Some(Older { latest: latest.clone(), ex: Arc::new(old) }),
                None => window.push_notification("That run's body is gone from history", cx),
            }
        }
        cx.notify();
    }

    fn copy_body(&self, ex: &Exchange, window: &mut Window, cx: &mut App) {
        cx.write_to_clipboard(ClipboardItem::new_string(String::from_utf8_lossy(&ex.response.body).into_owned()));
        window.push_notification(format!("Copied the body ({})", size_label(ex.response.body.len())), cx);
    }

    fn copy_curl(&self, ex: &Exchange, window: &mut Window, cx: &mut App) {
        cx.write_to_clipboard(ClipboardItem::new_string(http_curl::export_sent(&ex.request)));
        window.push_notification("Copied as cURL", cx);
    }

    fn save_body(&self, ex: &Exchange, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.doc().and_then(|d| d.path.parent().map(Path::to_path_buf)).or_else(std::env::home_dir).unwrap_or_default();
        let picked = cx.prompt_for_new_path(&dir, Some(&suggested_name(ex)));
        let body = ex.response.body.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = picked.await else { return };
            let msg = match std::fs::write(&path, &body) {
                Ok(()) => format!("Saved {}", path.display()),
                Err(e) => format!("{}: {e}", path.display()),
            };
            cx.update(|window, cx| window.push_notification(msg, cx)).ok();
        })
        .detach();
    }

    /// Diff of this run's body against the run before it.
    fn compare(&mut self, ex: &Exchange, window: &mut Window, cx: &mut Context<Self>) {
        let Some((i, _)) = self.run_position(ex) else { return };
        let prev = self.runs.as_ref().and_then(|(_, e)| e.get(i + 1)).cloned();
        let Some((prev, store)) = prev.zip(self.doc().and_then(|d| d.exec.store())) else { return };
        let Some(old) = store.body(&prev.id) else {
            window.push_notification("The previous run's body is gone from history", cx);
            return;
        };
        let patch = diff(&old, &ex.response.body);
        if !patch.contains("@@") {
            window.push_notification("Same body as the previous run", cx);
            return;
        }
        let files = match DiffFile::parse(&patch) {
            Ok(f) => f,
            Err(e) => {
                window.push_notification(format!("Diff: {e}"), cx);
                return;
            }
        };
        // Highlighted like the Body tab: the patch's file names say nothing about the type.
        let lang = match body_view(&ex.response, false, "", &Limits { format: usize::MAX, preview: usize::MAX }) {
            BodyView::Text(lang, _) if lang != "text" => Some(lang),
            _ => None,
        };
        let files: Vec<DiffFile> = files.into_iter().map(|f| if let Some(l) = lang { f.with_language(l) } else { f }).collect();
        // Unchanged lines fold to 3 around each change; the toggle unfolds the whole body.
        let state = cx.new(|cx| DiffState::new(files, cx).with_mode(DiffMode::Split).with_context_lines(Some(3)));
        let whole = std::rc::Rc::new(std::cell::Cell::new(false));
        let title = format!("Body: {} → this run", run_time(&prev.time));
        window.open_dialog(cx, move |dialog, window, _| {
            let h = window.viewport_size().height * 0.75;
            let (state_t, whole_t) = (state.clone(), whole.clone());
            let toggle = Button::new("diff-whole")
                .label(if whole.get() { "Changes only" } else { "Whole body" })
                .ghost()
                .xsmall()
                .on_click(move |_, window, cx| {
                    whole_t.set(!whole_t.get());
                    state_t.update(cx, |s, cx| if whole_t.get() { s.expand_unchanged(cx) } else { s.collapse_unchanged(cx) });
                    window.refresh();
                });
            dialog
                .title(title.clone())
                .w(px(1100.))
                .child(v_flex().gap_2().child(div().flex().justify_end().child(toggle)).child(div().h(h).child(Diff::new(&state))))
        });
    }

    /// Right-click → Copy JSON Path: the path of the node under the caret in the body.
    pub(crate) fn copy_json_path(&self, window: &mut Window, cx: &mut App) {
        let (text, at) = self.resp_body.read_with(cx, |s, _| (s.value().to_string(), s.cursor()));
        let Some(path) = path_at(&text, at) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
        window.push_notification(format!("Copied {path}"), cx);
    }

    /// Load what the current tab shows into its editor (or image / binary view) when it changed.
    pub(crate) fn sync_response(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_runs();
        let Some(Shown { status: Some(Status::Done(ex)), has_scripts, .. }) = self.shown_now() else {
            (self.shown, self.shown_meta, self.body_other) = (None, None, None);
            return;
        };
        let ptr = Arc::as_ptr(&ex) as usize;
        if self.tab == 0 {
            let filter = self.body_filter.read(cx).value().to_string();
            let key = (ptr, self.raw, filter.clone());
            if self.shown.as_ref() == Some(&key) {
                return;
            }
            let s = &self.settings.settings.response;
            let mb = |m: f64| (m * 1024. * 1024.) as usize;
            let limits = Limits { format: mb(s.format_limit_mb), preview: mb(s.preview_limit_mb) };
            (self.body_json, self.body_other) = (false, None);
            let wrap = s.soft_wrap;
            match body_view(&ex.response, self.raw, &filter, &limits) {
                BodyView::Text(lang, text) => {
                    self.body_json = lang == "json";
                    let wrap = wrap && text.len() <= WRAP_MAX;
                    self.resp_body.update(cx, |s, cx| {
                        s.set_highlighter(SharedString::from(lang), cx);
                        // Wrapping is the slow part: off before a big text, on only after a small one.
                        if !wrap {
                            s.set_soft_wrap(false, window, cx);
                        }
                        s.set_value(text, window, cx);
                        if wrap {
                            s.set_soft_wrap(true, window, cx);
                        }
                    });
                }
                BodyView::Image(f) => self.body_other = Some(BodyOther::Image(Arc::new(gpui::Image::from_bytes(f, ex.response.body.clone())))),
                BodyView::Binary => self.body_other = Some(BodyOther::Binary),
                BodyView::TooBig => self.body_other = Some(BodyOther::TooBig),
            }
            self.shown = Some(key);
        } else if self.shown_meta != Some((ptr, self.tab)) {
            let text = match self.tab {
                1 => {
                    let r = &ex.response;
                    let headers: String = r.headers.iter().map(|(n, v)| format!("{n}: {v}\n")).collect();
                    format!("{} {} {}\n{headers}", r.http_version, r.status, r.reason)
                }
                2 => cookies_text(&ex.response),
                3 => {
                    let q = &ex.request;
                    let headers: String = q.headers.iter().map(|(n, v)| format!("{n}: {v}\n")).collect();
                    let body = String::from_utf8_lossy(&q.body);
                    let body = if body.is_empty() { String::new() } else { format!("\n{body}\n") };
                    format!("{} {}\n{headers}{body}", q.method, q.url)
                }
                4 => timeline_text(&ex),
                _ => tests_text(&ex, has_scripts),
            };
            self.resp_meta.update(cx, |s, cx| s.set_value(text, window, cx));
            self.shown_meta = Some((ptr, self.tab));
        }
    }

    /// Header (title + Run; status + runs + actions), tabs, then the tab's content.
    pub(crate) fn response_panel(&self, cx: &Context<Self>) -> AnyElement {
        let t = cx.theme();
        let muted = t.muted_foreground;
        let shown = self.shown_now();
        let status = shown.as_ref().and_then(|s| s.status.clone());
        let title = div().flex_1().min_w_0().flex().items_center().gap_1p5().text_sm();
        let title = match shown.as_ref().map(|s| s.head.clone()) {
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
        let left = match &status {
            None if self.show_history || shown.is_none() => div(),
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
                    .child(div().text_xs().text_color(muted).child(format!("{} ms · {} · {}", r.total.as_millis(), size_label(r.body.len()), r.http_version)))
            }
        };
        let icon = |svg: &'static [u8]| Icon::default().data(svg);
        // Pin: keep this response on screen; next to Run because both are about the panel itself.
        let pin = (matches!(status, Some(Status::Done(_))) && !self.show_history).then(|| {
            let (pinned, current) = (self.pinned.is_some(), shown.clone());
            Button::new("pin")
                .icon(icon(include_bytes!("../assets/pin.svg")))
                .ghost()
                .small()
                .selected(pinned)
                .tooltip(if pinned { "Pinned — click to follow the cursor again" } else { "Pin: keep this response while the cursor moves" })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pinned = if this.pinned.is_some() { None } else { current.clone() };
                    cx.notify();
                }))
        });
        // Re-run / stop the request under the cursor; not on the History page or while pinned.
        let running = matches!(status, Some(Status::Running));
        let run = (!self.show_history && self.pinned.is_none() && shown.is_some()).then(|| {
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
        // This request's earlier runs: ‹ Run 3 of 5 · 15:17:01 › and Compare with the previous one.
        let history = match &status {
            Some(Status::Done(ex)) if !self.show_history => self.run_position(ex).filter(|(_, n)| *n > 1).map(|(i, n)| {
                let when = self.runs.as_ref().and_then(|(_, e)| e.get(i)).map(|e| run_time(&e.time)).unwrap_or_default();
                let step = |id: &'static str, name: IconName, tip: &'static str, enabled: bool, older: bool| {
                    Button::new(id).icon(name).ghost().xsmall().tooltip(tip).disabled(!enabled).on_click(cx.listener(move |this, _, window, cx| this.step_run(older, window, cx)))
                };
                let c = ex.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .child(step("run-older", IconName::ChevronLeft, "Older run", i + 1 < n, true))
                    .child(format!("Run {} of {n} · {when}", n - i))
                    .child(step("run-newer", IconName::ChevronRight, "Newer run", i > 0, false))
                    .when(i + 1 < n, |el| {
                        el.child(
                            Button::new("diff-body")
                                .icon(icon(include_bytes!("../assets/git-compare.svg")))
                                .label("Compare with previous")
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(move |this, _, window, cx| this.compare(&c, window, cx))),
                        )
                    })
            }),
            _ => None,
        };
        let body_tab = self.tab == 0;
        let view = if body_tab { &self.resp_body } else { &self.resp_meta };
        // What can be done with the response, next to its status: view options, then
        // copy / save / search.
        let tools = match &status {
            Some(Status::Done(ex)) => {
                // Same panel as Cmd+F inside the response (find, next/prev, match case, regex).
                let target = view.clone();
                let search = Button::new("response-search").icon(IconName::Search).ghost().xsmall().tooltip("Search response (Cmd+F)").on_click(move |_, window, cx| {
                    target.update(cx, |s, cx| {
                        s.focus(window, cx);
                        s.open_search(false, cx);
                    })
                });
                let wrap_on = self.settings.settings.response.soft_wrap;
                let too_big = body_tab && view.read(cx).text().len() > WRAP_MAX;
                let wrap = Button::new("response-wrap")
                    .icon(icon(include_bytes!("../assets/text-wrap.svg")))
                    .selected(wrap_on && !too_big)
                    .disabled(too_big)
                    .tooltip(match (too_big, wrap_on) {
                        (true, _) => "Bodies over 1 MB don't wrap: wrapping them freezes the app",
                        (_, true) => "Long lines wrap — click to scroll them instead",
                        _ => "Long lines scroll — click to wrap them",
                    })
                    .on_click(cx.listener(move |this, _, window, cx| this.set("response.soft_wrap", (!wrap_on).into(), window, cx)));
                let raw = (body_tab && self.body_other.is_none()).then(|| {
                    Button::new("response-raw")
                        .icon(icon(include_bytes!("../assets/braces.svg")))
                        .selected(self.raw)
                        .tooltip(if self.raw { "As received — click to pretty-print" } else { "Pretty-printed — click to show as received" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.raw = !this.raw;
                            cx.notify();
                        }))
                });
                let (a, b, c_) = (ex.clone(), ex.clone(), ex.clone());
                let copy = Button::new("copy-body")
                    .icon(IconName::Copy)
                    .tooltip("Copy the body")
                    .on_click(cx.listener(move |this, _, window, cx| this.copy_body(&a, window, cx)));
                let curl = Button::new("copy-curl")
                    .icon(icon(include_bytes!("../assets/terminal.svg")))
                    .tooltip("Copy the request as a cURL command")
                    .on_click(cx.listener(move |this, _, window, cx| this.copy_curl(&c_, window, cx)));
                let save = Button::new("save-body")
                    .icon(icon(include_bytes!("../assets/download.svg")))
                    .tooltip("Save the body to a file")
                    .on_click(cx.listener(move |this, _, window, cx| this.save_body(&b, window, cx)));
                // Outlined segments: view toggles, then actions; search stands alone.
                let view_opts = ButtonGroup::new("response-view").outline().xsmall().children(raw).child(wrap);
                let actions = ButtonGroup::new("response-actions").outline().xsmall().child(copy).child(curl).child(save);
                Some(div().flex().items_center().gap_1().child(view_opts).child(div().w_2()).child(actions).child(search))
            }
            _ => None,
        };
        // Three groups, apart by spacing only: what was sent, what came back, its earlier runs.
        let head = v_flex()
            .flex_none()
            .gap_1()
            .pb_3()
            .child(div().h(px(28.)).flex().items_center().gap_1().child(title).children(pin).children(run))
            .child(div().h(px(24.)).flex().items_center().justify_between().gap_2().child(left).children(tools))
            .children(history);
        let panel = div().size_full().px_3().pt_2().flex().flex_col().gap_1().bg(t.background).child(head);
        let ex = match status {
            Some(Status::Done(ex)) => ex,
            Some(Status::Failed(e)) => return panel.child(div().text_sm().text_color(t.danger).child(e)).into_any_element(),
            // ponytail: covers the wait for the server; showing the body (≤ ~0.2 s, measured) runs on
            // the UI thread, where nothing animates.
            Some(Status::Running) => {
                let waiting = v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(ProgressCircle::new("waiting").loading(true).color(t.primary).with_size(Size::Size(t.font_size * 4.)))
                    .child(div().text_sm().text_color(muted).child("Waiting for the response…"))
                    .child(div().text_xs().text_color(muted).child("Esc to stop"));
                return panel.child(waiting).into_any_element();
            }
            _ => return panel.into_any_element(),
        };

        let deps = (!ex.dependencies.is_empty()).then(|| div().text_xs().text_color(muted).child(format!("Ran first: {}", ex.dependencies.join(", "))));
        // Pill tabs: no full-width rule under them; the row is theirs alone.
        let tabs = TABS.iter().fold(
            TabBar::new("response-tabs").pill().small().selected_index(self.tab).on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.tab = *ix;
                cx.notify();
            })),
            |bar, name| {
                let tab = Tab::new().label(*name);
                // Failed `# @assert`: a dot sized from the UI font, ringed so the hover fill doesn't swallow it.
                bar.child(match *name == "Tests" && ex.tests.iter().any(|r| !r.passed) {
                    true => tab.suffix(div().size(t.font_size * 0.75).rounded_full().bg(t.danger).border_2().border_color(t.background).mr(t.font_size * 0.4)),
                    false => tab,
                })
            },
        );

        let content = match (&self.body_other, body_tab) {
            (Some(BodyOther::Image(image)), true) => div()
                .flex_1()
                .min_h_0()
                .p_2()
                .flex()
                .justify_center()
                .child(img(ImageSource::Image(image.clone())).max_w_full().max_h_full().object_fit(ObjectFit::ScaleDown))
                .into_any_element(),
            (Some(other @ (BodyOther::Binary | BodyOther::TooBig)), true) => {
                let size = size_label(ex.response.body.len());
                let kind = header(&ex.response, "content-type").unwrap_or("unknown type").to_string();
                let text = match other {
                    BodyOther::TooBig => format!("{size} — over the preview limit (response.preview_limit_mb)"),
                    _ => format!("Binary response · {size} · {kind}"),
                };
                let save = ex.clone();
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(div().text_sm().text_color(muted).child(text))
                    .child(Button::new("save-binary").label("Save to File…").on_click(cx.listener(move |this, _, window, cx| this.save_body(&save, window, cx))))
                    .into_any_element()
            }
            _ => {
                let mut editor = Editor::new(view).size_full().bordered(false);
                if body_tab && self.body_json {
                    editor = editor.context_menu(|menu, _, _| {
                        use gpui_component::input::{Copy, SelectAll};
                        menu.menu("Copy", Box::new(Copy)).menu("Copy JSON Path", Box::new(crate::CopyJsonPath)).separator().menu("Select All", Box::new(SelectAll))
                    });
                }
                let filter = (body_tab && self.body_json).then(|| div().flex_none().py_1().child(Input::new(&self.body_filter).small().cleanable(true)));
                v_flex().flex_1().min_h_0().child(div().flex_1().min_h_0().child(editor)).children(filter).into_any_element()
            }
        };
        panel.children(deps).child(tabs).child(content).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{BodyView, Limits, body_view, cap, cookies_text, diff, path_at};
    use gpui::ImageFormat;
    use http_exec::Response;

    fn resp(ct: &str, body: &[u8]) -> Response {
        Response { headers: vec![("Content-Type".into(), ct.into())], body: body.to_vec(), ..Default::default() }
    }

    #[test]
    fn json_path_under_cursor() {
        let text = "{\n  \"items\": [\n    {\"id\": 1, \"tags\": [\"a\", \"b\"]},\n    {\"id\": 2, \"odd key\": null}\n  ]\n}";
        let at = |needle: &str| path_at(text, text.find(needle).unwrap()).unwrap();
        assert_eq!(at("\"items\""), "$.items");
        assert_eq!(at("1,"), "$.items[0].id");
        assert_eq!(at("\"b\""), "$.items[0].tags[1]");
        assert_eq!(at("2,"), "$.items[1].id");
        assert_eq!(at("null"), "$.items[1]['odd key']");
        assert_eq!(path_at(text, 0).unwrap(), "$");
    }

    #[test]
    fn body_views() {
        let limits = Limits { format: 1 << 20, preview: 1 << 20 };
        let text = |v: BodyView| match v {
            BodyView::Text(l, t) => (l, t),
            _ => panic!("not text"),
        };
        assert_eq!(text(body_view(&resp("application/json", b"{\"a\":[1,2]}"), false, "", &limits)), ("json", "{\n  \"a\": [\n    1,\n    2\n  ]\n}".into()));
        assert_eq!(text(body_view(&resp("application/json", b"{\"a\":[1,2]}"), true, "", &limits)).1, "{\"a\":[1,2]}");
        assert_eq!(text(body_view(&resp("application/json", b"{\"a\":[1,2]}"), false, "$.a[1]", &limits)).1, "[\n  2\n]");
        assert!(text(body_view(&resp("application/json", b"{}"), false, "$[", &limits)).1.starts_with("// JSONPath"));
        assert!(matches!(body_view(&resp("image/png", b"\x89PNG"), false, "", &limits), BodyView::Image(ImageFormat::Png)));
        assert!(matches!(body_view(&resp("application/octet-stream", b"\xff\xfe\x00"), false, "", &limits), BodyView::Binary));
        let small = Limits { format: 2, preview: 4 };
        assert!(matches!(body_view(&resp("text/plain", b"hello"), false, "", &small), BodyView::TooBig));
        assert_eq!(text(body_view(&resp("application/json", b"{\"a\":1}"), false, "", &Limits { format: 2, preview: 100 })).1, "{\"a\":1}");
    }

    #[test]
    fn caps_only_long_lines() {
        let short_lines = "{\"id\": 1},\n".repeat(100_000); // ~1 MB of short lines: kept whole
        assert_eq!(cap(short_lines.clone()), short_lines);
        let one_line = "x".repeat(1 << 20); // 1 MB on one line: cut
        let cut = cap(one_line);
        assert!(cut.len() < 300 * 1024 && cut.ends_with("Save shows all of it)"));
    }

    #[test]
    fn tab_texts() {
        let mut r = resp("text/plain", b"");
        r.headers.push(("set-cookie".into(), "sid=abc; Path=/; HttpOnly".into()));
        r.cookies_sent = vec![("theme".into(), "dark".into())];
        let c = cookies_text(&r);
        assert!(c.contains("sid: abc\n#   Path=/\n#   HttpOnly\n") && c.contains("theme: dark"));
        let d = diff(b"{\"a\":1}", b"{\"a\":2}");
        assert!(d.contains("-  \"a\": 1\n+  \"a\": 2"));
        // The diff view takes it as is.
        assert_eq!(gpui_component::diff::DiffFile::parse(&d).map(|f| f.len()), Ok(1));
    }
}
