//! Sending from the editor: what a send asks first (`@prompt` values, then one confirmation
//! for production, `@note` and undefined variables), batches (all / selected), repeat, cancel.

use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

use gpui::*;
use gpui_component::{
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use http_syntax::Request;

use crate::Respite;

/// `@prompt name description` of `reqs`, each name once.
fn prompts(reqs: &[&Request]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for d in reqs.iter().flat_map(|r| &r.directives).filter(|d| d.name == "prompt") {
        let value = d.value.as_deref().unwrap_or("").trim();
        let (var, desc) = value.split_once(char::is_whitespace).map_or((value, ""), |(v, d)| (v, d.trim()));
        if !var.is_empty() && !out.iter().any(|(v, _)| v == var) {
            out.push((var.into(), desc.into()));
        }
    }
    out
}

/// Plain `{{name}}`s in `reqs` that nothing defines (file variable, environment, `@prompt`).
/// Dynamic `{{$…}}` and `{{req.response…}}` references are left to the executor.
fn undefined(reqs: &[&Request], file: &http_syntax::HttpFile, env: &HashMap<String, http_env::Variable>, asked: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for r in reqs {
        let texts = std::iter::once(r.url.as_str())
            .chain(r.headers.iter().flat_map(|h| [h.name.as_str(), h.value.as_str()]))
            .chain(r.body.as_ref().map(|b| b.text.as_str()));
        for text in texts {
            let mut rest = text;
            while let Some(open) = rest.find("{{") {
                let Some(close) = rest[open..].find("}}").map(|c| c + open) else { break };
                let name = rest[open + 2..close].trim();
                let known = name.starts_with('$')
                    || name.contains('.')
                    || file.variables.iter().any(|v| v.name == name)
                    || env.contains_key(name)
                    || asked.iter().any(|(v, _)| v == name);
                if !known && !out.iter().any(|n| n == name) {
                    out.push(name.to_string());
                }
                rest = &rest[close + 2..];
            }
        }
    }
    out
}

/// A secret-looking `@prompt` name gets a masked field (as in the CLI).
fn is_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["pass", "secret", "token", "key"].iter().any(|s| lower.contains(s))
}

impl Respite {
    /// Send `lines` (request lines) of tab `ix`, in order, after asking what they need.
    pub(crate) fn send(&mut self, ix: usize, lines: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.docs.get(ix) else { return };
        let reqs: Vec<&Request> = lines.iter().filter_map(|l| d.file.requests.iter().find(|r| r.request_line == *l)).collect();
        if reqs.is_empty() {
            return;
        }
        let path = (*d.path).clone();
        self.last_run = Some((path.clone(), lines.clone()));

        let asked = prompts(&reqs);
        let missing = undefined(&reqs, &d.file, &d.env, &asked);
        let mut reasons = vec![];
        if let Some(env) = self.env_name.as_deref().filter(|n| self.settings.settings.env.is_prod(n)) {
            reasons.push(format!("Environment \"{env}\" is production."));
        }
        for r in &reqs {
            if let Some(note) = r.directive("note") {
                let why = note.value.as_deref().map(|v| format!(": {v}")).unwrap_or_default();
                reasons.push(format!("{} is marked @note{why}", r.label()));
            }
        }
        if !missing.is_empty() {
            let list = missing.iter().map(|n| format!("{{{{{n}}}}}")).collect::<Vec<_>>().join(", ");
            reasons.push(format!("Undefined: {list}. Sent as written."));
        }

        // Undefined variables go out literally: an override is used as is, never resolved again.
        let literal: HashMap<String, String> = missing.iter().map(|n| (n.clone(), format!("{{{{{n}}}}}"))).collect();
        let then = move |this: &mut Self, mut overrides: HashMap<String, String>, window: &mut Window, cx: &mut Context<Self>| {
            overrides.extend(literal);
            this.confirm_and_run(path, lines, overrides, reasons, window, cx);
        };
        if asked.is_empty() {
            then(self, HashMap::new(), window, cx);
        } else {
            self.ask_prompts(asked, then, window, cx);
        }
    }

    /// One dialog with a field per `@prompt` variable; Send hands the values to `then`.
    fn ask_prompts(
        &mut self,
        asked: Vec<(String, String)>,
        then: impl FnOnce(&mut Self, HashMap<String, String>, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fields: Vec<(String, String, Entity<InputState>)> = asked
            .into_iter()
            .map(|(var, desc)| {
                let secret = is_secret(&var);
                let input = cx.new(|cx| InputState::new(window, cx).placeholder(desc.clone()).masked(secret));
                (var, desc, input)
            })
            .collect();
        let first = fields[0].2.clone();
        // `open_dialog` rebuilds on every render (`Fn`); the continuation runs once.
        let then = Rc::new(RefCell::new(Some(then)));
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let submit = {
                let (then, view, fields) = (then.clone(), view.clone(), fields.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let values: HashMap<String, String> = fields.iter().map(|(v, _, i)| (v.clone(), i.read(cx).value().to_string())).collect();
                    window.close_dialog(cx);
                    if let Some(then) = then.borrow_mut().take() {
                        view.update(cx, |this, cx| then(this, values, window, cx)).ok();
                    }
                }
            };
            let rows = fields.iter().map(|(var, desc, input)| {
                let label = if desc.is_empty() { var.clone() } else { format!("{var} — {desc}") };
                v_flex().gap_1().child(div().text_sm().child(label)).child(Input::new(input))
            });
            dialog
                .title("Values for this request")
                .w(px(480.))
                .child(v_flex().gap_3().children(rows))
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("prompt-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("prompt-send").label("Send").primary().on_click(submit)),
                )
        });
        first.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Ask once for everything that needs a yes (production, `@note`, undefined), then run.
    fn confirm_and_run(
        &mut self,
        path: PathBuf,
        lines: Vec<usize>,
        overrides: HashMap<String, String>,
        reasons: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if reasons.is_empty() {
            self.run_batch(path, lines, overrides, cx);
            return;
        }
        let title = if lines.len() == 1 { "Send this request?".to_string() } else { format!("Send {} requests?", lines.len()) };
        let detail = reasons.join("\n");
        let answer = window.prompt(PromptLevel::Warning, &title, Some(&detail), &["Send", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |this, cx| this.run_batch(path, lines, overrides, cx)).ok();
            }
        })
        .detach();
    }

    /// One after another (a later request may use an earlier one's response). Cancelling one
    /// (Cmd+. / ◼) stops the rest.
    fn run_batch(&mut self, path: PathBuf, lines: Vec<usize>, overrides: HashMap<String, String>, cx: &mut Context<Self>) {
        self.batch = Some(cx.spawn(async move |this, cx| {
            for line in lines {
                let Ok(Some(done)) = this.update(cx, |this, cx| {
                    let ix = this.docs.iter().position(|d| *d.path == path)?;
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    this.start(ix, line, overrides.clone(), Some(tx), cx);
                    Some(rx)
                }) else {
                    return;
                };
                if done.await.is_err() {
                    return; // cancelled
                }
            }
        }));
    }

    /// Cmd+.: stop every running request of the current tab, and the batch it belongs to.
    pub(crate) fn cancel_all(&mut self, cx: &mut Context<Self>) {
        self.batch = None;
        if let Some(ix) = self.current {
            let running: Vec<usize> = self.docs[ix].tasks.keys().copied().collect();
            for line in running {
                self.cancel(ix, line, cx);
            }
        }
    }

    /// Request lines of tab `ix` the selection touches (the one under the cursor without one).
    pub(crate) fn selected_requests(&self, ix: usize, cx: &App) -> Vec<usize> {
        use gpui_component::input::RopeExt as _;
        let d = &self.docs[ix];
        let s = d.editor.read(cx);
        let sel = s.selected_range();
        let (a, b) = (s.text().offset_to_position(sel.start).line as usize + 1, s.text().offset_to_position(sel.end).line as usize + 1);
        d.file.requests.iter().filter(|r| *r.lines.start() <= b && a <= *r.lines.end()).map(|r| r.request_line).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{prompts, undefined};

    #[test]
    fn what_a_send_asks() {
        let file = http_syntax::parse(
            "@id = 1\n# @prompt otp one-time code\n# @prompt otp again\nGET /{{id}}/{{otp}}?q={{nope}}&t={{$uuid}}\nX-Ref: {{login.response.body.x}}\nAuth: {{token}}\n",
        );
        let reqs: Vec<_> = file.requests.iter().collect();
        let asked = prompts(&reqs);
        assert_eq!(asked, [("otp".to_string(), "one-time code".to_string())]);
        let env = std::collections::HashMap::from([(
            "token".to_string(),
            http_env::Variable { value: "t".into(), source: "/e.json".into(), secret: true },
        )]);
        assert_eq!(undefined(&reqs, &file, &env, &asked), ["nope"]);
    }
}
