//! History (Cmd+Y): every execution of the current project, newest first.
//! Click shows the recorded response in the response panel; ▶ runs the request again.

use std::{ops::Range, sync::Arc};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    input::Input,
};
use http_exec::Store;

use crate::{Respite, sidebar};

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

    /// Indices of entries matching every word of the filter (method, URL, name, file, status).
    fn history_matches(&self, cx: &App) -> Vec<usize> {
        let query = self.history_filter.read(cx).value().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        self.history
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                let hay = format!("{} {} {} {} {}", e.method, e.url, e.label, e.file, e.status).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .map(|(i, _)| i)
            .collect()
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
            }));
            div().flex_1().min_h_0().py_1().child(list.size_full()).into_any_element()
        };
        div().size_full().flex().flex_col().child(header).child(body).into_any_element()
    }

    fn history_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let i = self.history_rows[ix];
        let e = &self.history[i];
        let t = cx.theme();
        let (muted, hover, active) = (t.muted_foreground, t.list_hover, t.list_active);
        let selected = self.history_pick.as_ref().is_some_and(|(p, _)| p.id == e.id);
        let color = self.status_color(e.status, cx);
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
            .h(px(40.))
            .px_3()
            .rounded_md()
            .flex()
            .items_center()
            .gap_3()
            .text_sm()
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .when(selected, |r| r.bg(active))
            .child(div().w(px(104.)).flex_none().text_xs().font_family(t.mono_font_family.clone()).text_color(muted).child(time_label(&e.time)))
            .child(
                div()
                    .w(px(36.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .rounded_sm()
                    .bg(color.opacity(0.15))
                    .text_color(color)
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .child(e.status.to_string()),
            )
            .child(
                div()
                    .w(px(44.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_size(px(9.5))
                    .font_weight(FontWeight::BOLD)
                    .font_family(t.mono_font_family.clone())
                    .text_color(sidebar::method_color(&e.method, cx))
                    .child(e.method.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(e.label.clone()))
                    .child(div().truncate().text_xs().text_color(muted).child(e.url.clone())),
            )
            .child(div().flex_none().text_xs().text_color(muted).child(format!("{}:{}", e.file, e.line)))
            .child(div().w(px(64.)).flex_none().flex().justify_end().text_xs().text_color(muted).child(format!("{} ms", e.total_ms)))
            .child(replay)
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

/// `14:03:22` today, `Nov 11 18:40` otherwise (local time).
fn time_label(rfc3339: &str) -> String {
    let Ok(ts) = rfc3339.parse::<jiff::Timestamp>() else { return rfc3339.to_string() };
    let tz = jiff::tz::TimeZone::system();
    let (at, today) = (ts.to_zoned(tz.clone()), jiff::Timestamp::now().to_zoned(tz));
    if at.date() == today.date() { at.strftime("%H:%M:%S").to_string() } else { at.strftime("%b %d %H:%M").to_string() }
}
