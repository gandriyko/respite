//! Settings page (Cmd+,): edits the global settings file, applied live.

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _,
    button::Button,
    input::Input,
    switch::Switch,
    tab::{Tab, TabBar},
};
use respite_settings::Appearance;
use serde_json::json;

use crate::{Respite, themes::FAMILIES};

const APPEARANCES: [(&str, Appearance); 3] = [("System", Appearance::System), ("Light", Appearance::Light), ("Dark", Appearance::Dark)];

impl Respite {
    pub(crate) fn settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.settings.settings.clone();
        let muted = cx.theme().muted_foreground;
        let section = |title: &str| div().mt_6().mb_1().text_xs().font_weight(FontWeight::BOLD).text_color(muted).child(title.to_uppercase());

        let mode_ix = APPEARANCES.iter().position(|(_, a)| *a == s.appearance).unwrap_or(0);
        let mode = APPEARANCES.iter().fold(
            TabBar::new("appearance").segmented().selected_index(mode_ix).on_click(cx.listener(|this, ix: &usize, window, cx| {
                this.set("appearance", json!(APPEARANCES[*ix].1), window, cx);
            })),
            |bar, (label, _)| bar.child(Tab::new().label(*label)),
        );
        let (border, accent, hover) = (cx.theme().border, cx.theme().primary, cx.theme().accent);
        let families = div().flex().flex_wrap().justify_end().gap_2().max_w(px(380.)).children(FAMILIES.iter().map(|(name, light, _)| {
            let selected = s.theme == *name;
            div()
                .id(*name)
                .px_3()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if selected { accent } else { border })
                .when(selected, |d| d.bg(hover))
                .cursor_pointer()
                .hover(move |d| d.bg(hover))
                .text_sm()
                .child(if light.is_none() && *name != "Default" { format!("{name} (dark only)") } else { name.to_string() })
                .on_click(cx.listener(move |this, _, window, cx| this.set("theme", json!(name), window, cx)))
        }));

        let path = self.settings.global_path.as_ref().map_or("—".into(), |p| p.display().to_string());
        div()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .px_8()
            .py_6()
            .child(
                div()
                    .max_w(px(620.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_xl().font_weight(FontWeight::BOLD).child("Settings"))
                            .child(Button::new("close-settings").label("Close").on_click(cx.listener(|this, _, _, cx| {
                                this.show_settings = false;
                                cx.notify();
                            }))),
                    )
                    .child(section("Appearance"))
                    .child(self.row("Theme", "theme", families.into_any_element(), cx))
                    .child(self.row("Appearance", "appearance", mode.into_any_element(), cx))
                    .child(self.row("Interface font size", "ui_font_size", self.stepper("ui_font_size", s.ui_font_size as f64, 1., 10., 24., cx), cx))
                    .child(self.row("Editor font", "font_family", Input::new(&self.font_input).w(px(260.)).into_any_element(), cx))
                    .child(self.row("Editor font size", "font_size", self.stepper("font_size", s.font_size as f64, 1., 9., 32., cx), cx))
                    .child(section("Editor"))
                    .child(self.row("Line numbers", "editor.line_numbers", self.switch("editor.line_numbers", s.editor.line_numbers, cx), cx))
                    .child(self.row("Soft wrap", "editor.soft_wrap", self.switch("editor.soft_wrap", s.editor.soft_wrap, cx), cx))
                    .child(self.row("Status after the request line", "editor.inlay_status", self.switch("editor.inlay_status", s.editor.inlay_status, cx), cx))
                    .child(self.row(
                        "Highlight the current line",
                        "editor.highlight_current_line",
                        self.switch("editor.highlight_current_line", s.editor.highlight_current_line, cx),
                        cx,
                    ))
                    .child(section("Requests"))
                    .child(self.row("Default timeout, ms", "request.timeout_ms", Input::new(&self.timeout_input).w(px(160.)).into_any_element(), cx))
                    .child(self.row("Follow redirects", "request.follow_redirects", self.switch("request.follow_redirects", s.request.follow_redirects, cx), cx))
                    .child(self.row("Max redirects", "request.max_redirects", self.stepper("request.max_redirects", s.request.max_redirects as f64, 1., 0., 50., cx), cx))
                    .child(section("History"))
                    .child(self.row("Entries per request", "history.per_request", self.stepper("history.per_request", s.history.per_request as f64, 10., 0., 1000., cx), cx))
                    .child(self.row("Entries in total", "history.total", self.stepper("history.total", s.history.total as f64, 100., 0., 10000., cx), cx))
                    .child(section("cURL"))
                    .child(self.row("Convert cURL on paste", "curl.auto_convert_on_paste", self.switch("curl.auto_convert_on_paste", s.curl.auto_convert_on_paste, cx), cx))
                    .child(self.row("Keep the original command as a comment", "curl.keep_original_as_comment", self.switch("curl.keep_original_as_comment", s.curl.keep_original_as_comment, cx), cx))
                    .child(
                        div()
                            .mt_8()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("Saved to {path}. A project's .respite/settings.json overrides these values.")),
                    ),
            )
            .into_any_element()
    }

    /// Label, control, and a note when the project file overrides the key.
    fn row(&self, label: &str, key: &str, control: AnyElement, cx: &Context<Self>) -> Div {
        let note = self
            .settings
            .overridden(key)
            .then(|| div().text_xs().text_color(cx.theme().warning).child("overridden by .respite/settings.json"));
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .py_1p5()
            .child(div().flex().flex_col().child(label.to_string()).children(note))
            .child(control)
    }

    fn switch(&self, key: &'static str, checked: bool, cx: &mut Context<Self>) -> AnyElement {
        Switch::new(key)
            .checked(checked)
            .on_click(cx.listener(move |this, v: &bool, window, cx| this.set(key, json!(*v), window, cx)))
            .into_any_element()
    }

    /// − value + ; integers stay integers in the file.
    fn stepper(&self, key: &'static str, value: f64, step: f64, min: f64, max: f64, cx: &mut Context<Self>) -> AnyElement {
        let button = |label: &'static str, next: f64| {
            Button::new((key, label.len())).label(label).on_click(cx.listener(move |this, _, window, cx| {
                this.set(key, json!(next.clamp(min, max) as i64), window, cx);
            }))
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(button("−", value - step))
            .child(div().w(px(48.)).flex().justify_center().child(format!("{}", value as i64)))
            .child(button("+", value + step))
            .into_any_element()
    }
}
