//! Settings window (Cmd+,): edits the global settings file, applied live in the main window.

use gpui::*;
use gpui_component::{
    ActiveTheme as _, Root,
    button::Button,
    menu::{DropdownMenu as _, PopupMenuItem},
    input::{Input, InputEvent, InputState},
    switch::Switch,
    tab::{Tab, TabBar},
};
use respite_settings::{Appearance, Keymap, Loaded, Split};
use serde_json::json;

use crate::{CloseTab, Respite, themes::FAMILIES};

const APPEARANCES: [(&str, Appearance); 3] = [("System", Appearance::System), ("Light", Appearance::Light), ("Dark", Appearance::Dark)];
const SPLITS: [(&str, Split); 2] = [("Side by side", Split::Horizontal), ("Stacked", Split::Vertical)];
const KEYMAPS: [(&str, Keymap); 3] = [("Respite", Keymap::Default), ("VS Code", Keymap::Vscode), ("JetBrains", Keymap::Jetbrains)];

impl Respite {
    /// Bring the settings window forward, opening it if it is not open.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(h) = self.settings_window
            && h.update(cx, |_, w, _| w.activate_window()).is_ok()
        {
            return;
        }
        let (app, main) = (cx.entity().downgrade(), window.window_handle());
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(680.), px(780.)), cx)),
            titlebar: Some(TitlebarOptions { title: Some("Settings".into()), ..Default::default() }),
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        };
        // Deferred: the new window reads `Respite` while building and drawing its first frame,
        // which panics while `Respite` is still being updated (we are inside its listener).
        cx.defer(move |cx| {
            let weak = app.clone();
            let h = cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| SettingsWindow::new(weak, main, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            });
            match h {
                Ok(h) => _ = app.update(cx, |this, _| this.settings_window = Some(h)),
                Err(e) => eprintln!("respite: settings window: {e}"),
            }
        });
    }
}

pub(crate) struct SettingsWindow {
    app: WeakEntity<Respite>,
    /// The main window: settings are applied there (editors, executors, theme).
    main: AnyWindowHandle,
    focus: FocusHandle,
    font_input: Entity<InputState>,
    timeout_input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl SettingsWindow {
    fn new(app: WeakEntity<Respite>, main: AnyWindowHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = app.upgrade().map(|a| a.read(cx).settings.settings.clone()).unwrap_or_default();
        let font_input = cx.new(|cx| InputState::new(window, cx).placeholder("Default monospace font").default_value(s.font_family.clone()));
        let timeout_input = cx.new(|cx| InputState::new(window, cx).placeholder("0 = no limit").default_value(s.request.timeout_ms.to_string()));
        // Only real edits are written: syncing an input from the file must not write it back
        // (a project override would land in the global file).
        let mut _subs = vec![
            cx.subscribe(&font_input, |this, input, ev: &InputEvent, cx| {
                let v = input.read(cx).value().trim().to_string();
                if matches!(ev, InputEvent::Change) && Some(&v) != this.current(cx).map(|s| s.font_family).as_ref() {
                    this.set("font_family", v.into(), cx);
                }
            }),
            cx.subscribe(&timeout_input, |this, input, ev: &InputEvent, cx| {
                // Invalid numbers are ignored until they parse.
                if let (InputEvent::Change, Ok(ms)) = (ev, input.read(cx).value().trim().parse::<u64>())
                    && Some(ms) != this.current(cx).map(|s| s.request.timeout_ms)
                {
                    this.set("request.timeout_ms", ms.into(), cx);
                }
            }),
        ];
        // Settings changed elsewhere (the file on disk): refresh the text inputs not being edited.
        if let Some(entity) = app.upgrade() {
            _subs.push(cx.observe_in(&entity, window, |this, app, window, cx| {
                let s = app.read(cx).settings.settings.clone();
                for (input, value) in [(&this.font_input, s.font_family), (&this.timeout_input, s.request.timeout_ms.to_string())] {
                    if !input.focus_handle(cx).is_focused(window) && input.read(cx).value() != value {
                        input.update(cx, |i, cx| i.set_value(value, window, cx));
                    }
                }
                cx.notify();
            }));
        }
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self { app, main, focus, font_input, timeout_input, _subs }
    }

    fn current(&self, cx: &App) -> Option<respite_settings::Settings> {
        self.app.upgrade().map(|a| a.read(cx).settings.settings.clone())
    }

    fn set(&mut self, key: &str, value: serde_json::Value, cx: &mut Context<Self>) {
        let app = self.app.clone();
        self.main.update(cx, |_, window, cx| app.update(cx, |this, cx| this.set(key, value, window, cx))).ok();
        cx.notify();
    }

    /// Label, control, and a note when the project file overrides the key.
    fn row(l: &Loaded, label: &str, key: &str, control: AnyElement, cx: &App) -> Div {
        let note = l.overridden(key).then(|| div().text_xs().text_color(cx.theme().warning).child("overridden by .respite/settings.json"));
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .py_1p5()
            .child(div().flex().flex_col().child(label.to_string()).children(note))
            .child(control)
    }

    fn switch(key: &'static str, checked: bool, cx: &mut Context<Self>) -> AnyElement {
        Switch::new(key).checked(checked).on_click(cx.listener(move |this, v: &bool, _, cx| this.set(key, json!(*v), cx))).into_any_element()
    }

    /// − value + ; integers stay integers in the file.
    fn stepper(key: &'static str, value: f64, step: f64, min: f64, max: f64, cx: &mut Context<Self>) -> AnyElement {
        let button = |label: &'static str, next: f64| {
            Button::new((key, label.len())).label(label).on_click(cx.listener(move |this, _, _, cx| {
                this.set(key, json!(next.clamp(min, max) as i64), cx);
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

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(l) = self.app.upgrade().map(|a| a.read(cx).settings.clone()) else { return div().into_any_element() };
        let s = l.settings.clone();
        let muted = cx.theme().muted_foreground;
        let section = |title: &str| div().mt_6().mb_1().text_xs().font_weight(FontWeight::BOLD).text_color(muted).child(title.to_uppercase());
        let switch = |key: &'static str, on: bool, cx: &mut Context<Self>| Self::switch(key, on, cx);

        let mode_ix = APPEARANCES.iter().position(|(_, a)| *a == s.appearance).unwrap_or(0);
        let mode = APPEARANCES.iter().fold(
            TabBar::new("appearance").segmented().selected_index(mode_ix).on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.set("appearance", json!(APPEARANCES[*ix].1), cx);
            })),
            |bar, (label, _)| bar.child(Tab::new().label(*label)),
        );
        let split_ix = SPLITS.iter().position(|(_, k)| *k == s.split).unwrap_or(0);
        let split = SPLITS.iter().fold(
            TabBar::new("split").segmented().selected_index(split_ix).on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.set("split", json!(SPLITS[*ix].1), cx);
            })),
            |bar, (label, _)| bar.child(Tab::new().label(*label)),
        );
        let keymap_ix = KEYMAPS.iter().position(|(_, k)| *k == s.keymap).unwrap_or(0);
        let keymap = KEYMAPS.iter().fold(
            TabBar::new("keymap").segmented().selected_index(keymap_ix).on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.set("keymap", json!(KEYMAPS[*ix].1), cx);
            })),
            |bar, (label, _)| bar.child(Tab::new().label(*label)),
        );
        let label = |name: &str, light: &Option<&str>, dark: &Option<&str>| match (light, dark) {
            (None, Some(_)) => format!("{name} (dark only)"),
            (Some(_), None) => format!("{name} (light only)"),
            _ => name.to_string(),
        };
        let current = FAMILIES.iter().find(|f| f.0 == s.theme).map_or(s.theme.clone(), |(n, l, d)| label(n, l, d));
        let view = cx.entity().downgrade();
        let selected = s.theme.clone();
        let families = Button::new("theme").label(current).dropdown_caret(true).outline().w(px(260.)).dropdown_menu(move |menu, _, _| {
            FAMILIES.iter().fold(menu.scrollable(true).max_h(px(420.)), |menu, (name, light, dark)| {
                let view = view.clone();
                menu.item(PopupMenuItem::new(label(name, light, dark)).checked(selected == *name).on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| this.set("theme", json!(name), cx)).ok();
                }))
            })
        });

        let path = l.global_path.as_ref().map_or("—".into(), |p| p.display().to_string());
        let body = div()
            .max_w(px(620.))
            .flex()
            .flex_col()
            .child(section("Appearance"))
            .child(Self::row(&l, "Theme", "theme", families.into_any_element(), cx))
            .child(Self::row(&l, "Appearance", "appearance", mode.into_any_element(), cx))
            .child(Self::row(&l, "Interface font size", "ui_font_size", Self::stepper("ui_font_size", s.ui_font_size as f64, 1., 10., 24., cx), cx))
            .child(Self::row(&l, "Editor font", "font_family", Input::new(&self.font_input).w(px(260.)).into_any_element(), cx))
            .child(Self::row(&l, "Editor font size", "font_size", Self::stepper("font_size", s.font_size as f64, 1., 9., 32., cx), cx))
            .child(section("Editor"))
            .child(Self::row(&l, "Line numbers", "editor.line_numbers", switch("editor.line_numbers", s.editor.line_numbers, cx), cx))
            .child(Self::row(&l, "Soft wrap", "editor.soft_wrap", switch("editor.soft_wrap", s.editor.soft_wrap, cx), cx))
            .child(Self::row(
                &l,
                "Highlight the current line",
                "editor.highlight_current_line",
                switch("editor.highlight_current_line", s.editor.highlight_current_line, cx),
                cx,
            ))
            .child(section("Response"))
            .child(Self::row(&l, "Layout", "split", split.into_any_element(), cx))
            .child(Self::row(&l, "Pretty-print bodies up to, MB", "response.format_limit_mb", Self::stepper("response.format_limit_mb", s.response.format_limit_mb, 5., 1., 1000., cx), cx))
            .child(Self::row(&l, "Show bodies up to, MB", "response.preview_limit_mb", Self::stepper("response.preview_limit_mb", s.response.preview_limit_mb, 10., 1., 2000., cx), cx))
            .child(section("Keyboard"))
            .child(Self::row(&l, "Extra shortcuts", "keymap", keymap.into_any_element(), cx))
            .child(div().text_xs().text_color(muted).child("Your own: keymap.json next to the settings file (Zed's format)."))
            .child(section("Requests"))
            .child(Self::row(&l, "Default timeout, ms", "request.timeout_ms", Input::new(&self.timeout_input).w(px(160.)).into_any_element(), cx))
            .child(Self::row(&l, "Follow redirects", "request.follow_redirects", switch("request.follow_redirects", s.request.follow_redirects, cx), cx))
            .child(Self::row(&l, "Max redirects", "request.max_redirects", Self::stepper("request.max_redirects", s.request.max_redirects as f64, 1., 0., 50., cx), cx))
            .child(section("History"))
            .child(Self::row(&l, "Entries per request", "history.per_request", Self::stepper("history.per_request", s.history.per_request as f64, 10., 0., 1000., cx), cx))
            .child(Self::row(&l, "Entries in total", "history.total", Self::stepper("history.total", s.history.total as f64, 100., 0., 10000., cx), cx))
            .child(section("cURL"))
            .child(Self::row(&l, "Convert cURL on paste", "curl.auto_convert_on_paste", switch("curl.auto_convert_on_paste", s.curl.auto_convert_on_paste, cx), cx))
            .child(Self::row(
                &l,
                "Keep the original command as a comment",
                "curl.keep_original_as_comment",
                switch("curl.keep_original_as_comment", s.curl.keep_original_as_comment, cx),
                cx,
            ))
            .child(div().mt_8().text_xs().text_color(muted).child(format!("Saved to {path}. A project's .respite/settings.json overrides these values.")));

        div()
            .id("settings")
            .key_context("Respite")
            .track_focus(&self.focus)
            .on_action(|_: &CloseTab, window, _| window.remove_window())
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .px_8()
            .pb_6()
            .child(body)
            .into_any_element()
    }
}
