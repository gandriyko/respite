//! History (Cmd+Y): every execution of the current project, newest first; headers sort.
//! Click shows the recorded response in the response panel; ▶ runs the request again.

use std::{cmp::Ordering, ops::Range, sync::Arc};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    input::Input,
};
use http_exec::{Entry, Store};

use crate::{Respite, sidebar};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortBy {
    Time,
    Status,
    Method,
    Request,
    File,
    Duration,
}

/// Column widths: header and rows share them.
const TIME_W: f32 = 104.;
const STATUS_W: f32 = 56.;
const METHOD_W: f32 = 48.;
const FILE_W: f32 = 120.;
const DURATION_W: f32 = 64.;
const REPLAY_W: f32 = 24.;

impl Respite {
    pub(crate) fn toggle_history(&mut self, cx: &mut Context<Self>) {
        self.show_history = !self.show_history;
        if self.show_history {
            self.reload_history();
        }
        cx.notify();
    }

    /// History of the current file's project.
    fn history_store(&self) -> Option<&Store> {
        self.doc()?.exec.store()
    }

    /// Re-read entries (newest first); keeps the picked one if it is still there.
    pub(crate) fn reload_history(&mut self) {
        self.history = self.history_store().map(|s| s.list()).unwrap_or_default();
        if let Some((e, _)) = &self.history_pick
            && !self.history.iter().any(|x| x.id == e.id)
        {
            self.history_pick = None;
        }
    }

    /// Indices of entries matching every word of the filter (method, URL, name, file, status),
    /// in the chosen column order.
    fn history_matches(&self, cx: &App) -> Vec<usize> {
        let query = self.history_filter.read(cx).value().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        let mut rows: Vec<usize> = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                let hay = format!("{} {} {} {} {}", e.method, e.url, e.label, e.file, e.status).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .map(|(i, _)| i)
            .collect();
        let (by, desc) = self.history_sort;
        // Stable: ties keep the newest-first order.
        rows.sort_by(|&a, &b| {
            let o = compare(by, &self.history[a], &self.history[b]);
            if desc { o.reverse() } else { o }
        });
        rows
    }

    /// Same column again flips the order; a new one starts slowest / newest first for
    /// duration and time, A→Z otherwise.
    fn sort_history(&mut self, by: SortBy, cx: &mut Context<Self>) {
        self.history_sort = match self.history_sort {
            (cur, desc) if cur == by => (by, !desc),
            _ => (by, matches!(by, SortBy::Time | SortBy::Duration)),
        };
        self.history_scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn pick_history(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self.history[i].clone();
        match self.history_store().and_then(|s| s.exchange(entry.clone())) {
            Some(ex) => self.history_pick = Some((entry, Arc::new(ex))),
            None => window.push_notification("This entry's response body is gone from disk", cx),
        }
        cx.notify();
    }

    /// Open the entry's file at its request; `run` sends the request as it is in the file now
    /// (secrets are masked in history, so the recorded request can't be resent verbatim).
    fn open_history_request(&mut self, i: usize, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self.history[i].clone();
        let Some(path) = self.history_store().map(|s| s.root().join(&entry.file)) else { return };
        let Some(ix) = self.open(path.clone(), None, window, cx) else { return };
        let requests = &self.docs[ix].file.requests;
        let by_name = entry.name.as_ref().and_then(|n| requests.iter().find(|r| r.name.as_ref() == Some(n)));
        let line = by_name.or_else(|| requests.iter().find(|r| r.request_line == entry.line)).map(|r| r.request_line);
        match line {
            Some(line) => {
                self.show_history = false;
                self.go_to(path, line, run, window, cx);
            }
            None => window.push_notification(format!("{} is no longer in {}", entry.label, entry.file), cx),
        }
    }

    pub(crate) fn history_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.history_rows = self.history_matches(cx);
        let t = cx.theme();
        let muted = t.muted_foreground;
        let count = match (self.history_rows.len(), self.history.len()) {
            (shown, all) if shown == all => format!("{all} runs"),
            (shown, all) => format!("{shown} of {all}"),
        };
        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(t.border)
            .child(div().text_lg().font_weight(FontWeight::BOLD).child("History"))
            .child(div().text_xs().text_color(muted).child(count))
            .child(self.history_summary(cx))
            .child(div().flex_1())
            .child(Input::new(&self.history_filter).small().w(px(280.)))
            .child(
                Button::new("history-refresh")
                    .icon(IconName::RefreshCw)
                    .ghost()
                    .small()
                    .tooltip("Reload")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload_history();
                        cx.notify();
                    })),
            )
            .child(Button::new("history-close").label("Close").small().on_click(cx.listener(|this, _, _, cx| this.toggle_history(cx))));
        let body = if self.history_rows.is_empty() {
            let text = if self.history.is_empty() { "No runs in this project yet. Cmd+Enter a request to start." } else { "Nothing matches the filter." };
            div().flex_1().flex().items_center().justify_center().text_sm().text_color(muted).child(text).into_any_element()
        } else {
            let list = uniform_list("history", self.history_rows.len(), cx.processor(|this, range: Range<usize>, _, cx| {
                range.map(|ix| this.history_row(ix, cx)).collect::<Vec<_>>()
            }))
            .track_scroll(&self.history_scroll);
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .pt_2()
                .child(self.table_head(cx))
                .child(div().flex_1().min_h_0().child(list.size_full()))
                .into_any_element()
        };
        div().size_full().flex().flex_col().child(header).child(body).into_any_element()
    }

    /// "3 OK · 1 failed · avg 414 ms" as coloured chips, for the shown rows.
    fn history_summary(&self, cx: &App) -> AnyElement {
        let t = cx.theme();
        let rows: Vec<&Entry> = self.history_rows.iter().map(|&i| &self.history[i]).collect();
        let ok = rows.iter().filter(|e| (200..400).contains(&e.status)).count();
        let failed = rows.len() - ok;
        let avg = if rows.is_empty() { 0 } else { rows.iter().map(|e| e.total_ms).sum::<u64>() / rows.len() as u64 };
        let chip = |text: String, color: Hsla| div().px_2().py_0p5().rounded_full().bg(color.opacity(0.15)).text_color(color).text_xs().font_weight(FontWeight::SEMIBOLD).child(text);
        div()
            .flex()
            .gap_1()
            .when(ok > 0, |d| d.child(chip(format!("{ok} OK"), t.success)))
            .when(failed > 0, |d| d.child(chip(format!("{failed} failed"), t.danger)))
            .when(!rows.is_empty(), |d| d.child(chip(format!("avg {avg} ms"), t.muted_foreground)))
            .into_any_element()
    }

    fn table_head(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (fg, muted, hover) = (t.foreground, t.muted_foreground, t.list_hover);
        let (sorted, desc) = self.history_sort;
        let fs = cx.theme().font_size * 0.75;
        let cell = |id: &'static str, label: &'static str, by: SortBy, cx: &mut Context<Self>| {
            let active = sorted == by;
            let arrow = if desc { IconName::ChevronDown } else { IconName::ChevronUp };
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_0p5()
                .rounded_sm()
                .cursor_pointer()
                .whitespace_nowrap()
                .hover(move |s| s.bg(hover))
                .text_color(if active { fg } else { muted })
                .child(label)
                .when(active, |d| d.child(Icon::new(arrow).size(fs)))
                .on_click(cx.listener(move |this, _, _, cx| this.sort_history(by, cx)))
        };
        div()
            .flex_none()
            .mx_2()
            .px_3()
            .pb_2()
            .mb_1()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(t.border)
            .text_xs()
            .font_weight(FontWeight::BOLD)
            .child(div().w(px(TIME_W)).flex_none().child(cell("sort-time", "TIME", SortBy::Time, cx)))
            .child(div().w(px(STATUS_W)).flex_none().child(cell("sort-status", "STATUS", SortBy::Status, cx)))
            .child(div().w(px(METHOD_W)).flex_none().child(cell("sort-method", "METHOD", SortBy::Method, cx)))
            .child(div().flex_1().min_w_0().child(cell("sort-request", "REQUEST", SortBy::Request, cx)))
            .child(div().w(px(FILE_W)).flex_none().child(cell("sort-file", "FILE", SortBy::File, cx)))
            .child(div().w(px(DURATION_W)).flex_none().flex().justify_end().child(cell("sort-duration", "MS", SortBy::Duration, cx)))
            .child(div().w(px(REPLAY_W)).flex_none())
            .into_any_element()
    }

    fn history_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let i = self.history_rows[ix];
        let e = &self.history[i];
        let t = cx.theme();
        let (muted, hover, active, zebra) = (t.muted_foreground, t.list_hover, t.list_active, t.secondary.opacity(0.5));
        let mono = t.mono_font_family.clone();
        let selected = self.history_pick.as_ref().is_some_and(|(p, _)| p.id == e.id);
        let color = self.status_color(e.status, cx);
        let slow = if e.total_ms >= 1000 { t.warning } else { muted };
        let replay = Button::new(("history-replay", ix))
            .icon(IconName::Play)
            .ghost()
            .xsmall()
            .tooltip("Run again")
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.open_history_request(i, true, window, cx);
            }));
        let row = div()
            .id(("history-row", ix))
            .px_3()
            .h(px(36.))
            .flex()
            .items_center()
            .gap_3()
            .rounded_md()
            .cursor_pointer()
            .when(ix % 2 == 1, |r| r.bg(zebra))
            .when(selected, |r| r.bg(active))
            .hover(move |s| s.bg(hover))
            .child(div().w(px(TIME_W)).flex_none().text_xs().font_family(mono.clone()).text_color(muted).child(time_label(&e.time)))
            .child(
                div()
                    .w(px(STATUS_W))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(div().size(px(8.)).rounded_full().bg(color))
                    .child(div().text_sm().font_weight(FontWeight::BOLD).text_color(color).child(e.status.to_string())),
            )
            .child(div().w(px(METHOD_W)).flex_none().text_xs().font_weight(FontWeight::BOLD).font_family(mono).text_color(sidebar::method_color(&e.method, cx)).child(e.method.clone()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    // The name keeps most of the room; the URL shrinks first.
                    .child(div().flex_shrink_0().max_w(relative(0.75)).truncate().font_weight(FontWeight::SEMIBOLD).child(short_label(e)))
                    .child(div().truncate().text_xs().text_color(muted).child(e.url.clone())),
            )
            .child(div().w(px(FILE_W)).flex_none().truncate().text_xs().text_color(muted).child(format!("{}:{}", e.file, e.line)))
            .child(div().w(px(DURATION_W)).flex_none().text_right().text_xs().text_color(slow).child(e.total_ms.to_string()))
            .child(div().w(px(REPLAY_W)).flex_none().child(replay))
            // Click shows the response; double click opens the request in its file.
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                if ev.click_count() >= 2 {
                    this.open_history_request(i, false, window, cx);
                } else {
                    this.pick_history(i, window, cx);
                }
            }));
        div().w_full().px_2().child(row).into_any_element()
    }
}

/// An unnamed request's label is `METHOD url`; the METHOD column already says the method.
fn short_label(e: &Entry) -> String {
    e.label.strip_prefix(&e.method).and_then(|rest| rest.strip_prefix(' ')).unwrap_or(&e.label).to_string()
}

fn compare(by: SortBy, a: &Entry, b: &Entry) -> Ordering {
    match by {
        // RFC 3339 UTC: text order is time order.
        SortBy::Time => a.time.cmp(&b.time),
        SortBy::Status => a.status.cmp(&b.status),
        SortBy::Method => a.method.cmp(&b.method),
        SortBy::Request => a.label.to_lowercase().cmp(&b.label.to_lowercase()),
        SortBy::File => (&a.file, a.line).cmp(&(&b.file, b.line)),
        SortBy::Duration => a.total_ms.cmp(&b.total_ms),
    }
}

/// `14:03:22` today, `Nov 11 18:40` otherwise (local time).
fn time_label(rfc3339: &str) -> String {
    let Ok(ts) = rfc3339.parse::<jiff::Timestamp>() else { return rfc3339.to_string() };
    let tz = jiff::tz::TimeZone::system();
    let (at, today) = (ts.to_zoned(tz.clone()), jiff::Timestamp::now().to_zoned(tz));
    if at.date() == today.date() { at.strftime("%H:%M:%S").to_string() } else { at.strftime("%b %d %H:%M").to_string() }
}
