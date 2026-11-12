//! Shortcuts: Respite's own, a VS Code / JetBrains preset on top (`keymap` setting), and the
//! user's `keymap.json` (next to `settings.json`) over both. Re-applied live on change.
//!
//! `keymap.json` follows Zed's layout:
//! `[{"context": "Input", "bindings": {"cmd-r": "respite::RunRequest", "cmd-.": null}}]`.
//! Without `context` a binding works in the editor and outside it; `null` removes a shortcut.

use std::{path::Path, rc::Rc};

use gpui::*;
use respite_settings::Keymap;
use serde_json::Value;

use crate::*;

/// gpui-component's own bindings (editor, lists, dialogs), kept to rebuild the keymap.
struct Base(Vec<KeyBinding>);
impl Global for Base {}

/// Call once, right after `gpui_component::init`.
pub fn init(cx: &mut App) {
    let base = cx.key_bindings().borrow().bindings().cloned().collect();
    cx.set_global(Base(base));
}

/// `cmd-` on macOS, `ctrl-` elsewhere (`secondary-` is not supported by this GPUI version).
fn key(k: &str) -> String {
    let m = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };
    format!("{m}-{k}")
}

/// Where a binding works: the editor (`Input`), outside it (`Respite`), or everywhere.
#[derive(Clone, Copy)]
enum Ctx {
    Editor,
    Both,
    Global,
}

type Spec = (String, Box<dyn Action>, Ctx);

fn spec(keys: impl Into<String>, action: impl Action, ctx: Ctx) -> Spec {
    (keys.into(), Box::new(action), ctx)
}

fn defaults() -> Vec<Spec> {
    use gpui_component::input::{GoToDefinition, Paste, ShowCompletions};
    use Ctx::*;
    vec![
        // Editor keys go in its own "Input" context (later bindings win over its cmd-enter / cmd-v).
        spec(key("enter"), RunRequest, Editor),
        spec(key("shift-enter"), RunAll, Editor),
        spec(key("alt-enter"), RunSelected, Editor),
        spec(key("alt-l"), RepeatLast, Editor),
        // Cancels running requests, else opens the quick fixes (the editor's own Cmd+.).
        spec(key("."), QuickFix, Editor),
        spec("alt-up", PrevRequest, Editor),
        spec("alt-down", NextRequest, Editor),
        spec(key("v"), SmartPaste, Editor),
        spec(key("shift-v"), Paste, Editor),
        spec(key("alt-v"), PasteCurl, Editor),
        spec(key("shift-c"), CopyAsCurl, Editor),
        spec(key("i"), InsertFile, Editor),
        spec("f2", RenameSymbol, Editor),
        // Live templates (gtr, ptr, …) and their fields; otherwise Tab indents as before.
        spec("tab", ExpandTemplate, Editor),
        // The editor has these actions but no default keys.
        spec("f12", GoToDefinition, Editor),
        spec("ctrl-space", ShowCompletions, Editor),
        spec(key("s"), SaveFile, Both),
        spec(key("w"), CloseTab, Both),
        spec(key("b"), ToggleSidebar, Both),
        spec("ctrl-tab", NextTab, Both),
        spec(key(","), OpenSettings, Both),
        spec(key("alt-e"), NextEnvironment, Both),
        spec(key("y"), ToggleHistory, Both),
        spec(key("p"), GoToFile, Both),
        spec(key("shift-o"), GoToRequest, Both),
        spec(key("t"), GoToWorkspaceRequest, Both),
        spec(key("shift-p"), CommandPalette, Both),
        spec(key("1"), FocusEditor, Both),
        spec(key("2"), FocusResponse, Both),
        spec(key("j"), ToggleResponse, Both),
        spec(key("\\"), ToggleSplit, Both),
        spec(key("q"), Quit, Global),
    ]
}

/// Shortcuts of VS Code's REST Client / JetBrains' HTTP Client, added to the defaults.
fn preset(keymap: Keymap) -> Vec<Spec> {
    use gpui_component::input::GoToDefinition;
    use Ctx::*;
    let mac = cfg!(target_os = "macos");
    match keymap {
        Keymap::Default => vec![],
        Keymap::Vscode => vec![
            spec(key("alt-r"), RunRequest, Editor),
            spec(key("alt-k"), CancelRequests, Both),
            spec(key("alt-h"), ToggleHistory, Both),
            spec(key("alt-c"), CopyAsCurl, Editor),
        ],
        Keymap::Jetbrains => vec![
            spec(if mac { "ctrl-shift-r" } else { "ctrl-shift-f10" }, RunRequest, Editor),
            spec("alt-enter", QuickFix, Editor),
            spec("shift-f6", RenameSymbol, Editor),
            spec(key("b"), GoToDefinition, Editor),
            spec(if mac { "cmd-shift-o" } else { "ctrl-shift-n" }, GoToFile, Both),
            spec(key("f12"), GoToRequest, Both),
            spec(if mac { "cmd-alt-o" } else { "ctrl-alt-shift-n" }, GoToWorkspaceRequest, Both),
            spec(key("e"), NextTab, Both),
        ],
    }
}

/// One `keymap.json` binding: keys, action name (`None` unbinds), its argument, context.
type Entry = (String, Option<String>, Option<Value>, Option<String>);

pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let blocks: Vec<Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut out = vec![];
    for b in blocks {
        let ctx = b.get("context").and_then(Value::as_str).map(String::from);
        let Some(bindings) = b.get("bindings").and_then(Value::as_object) else { return Err("each entry needs a \"bindings\" object".into()) };
        for (keys, v) in bindings {
            let (name, arg) = match v {
                Value::Null => (None, None),
                Value::String(s) => (Some(s.clone()), None),
                Value::Array(a) if a.len() == 2 && a[0].is_string() => (a[0].as_str().map(String::from), Some(a[1].clone())),
                _ => return Err(format!("{keys}: expected an action name, [name, argument] or null")),
            };
            out.push((keys.clone(), name, arg, ctx.clone()));
        }
    }
    Ok(out)
}

/// Rebuild every shortcut; problems with the user's `keymap.json` come back as messages.
pub fn apply(keymap: Keymap, user: Option<&Path>, cx: &mut App) -> Vec<String> {
    let mut errors = vec![];
    let mut bindings: Vec<KeyBinding> = cx.try_global::<Base>().map(|b| b.0.clone()).unwrap_or_default();
    // No context: in the editor and outside it.
    fn add(out: &mut Vec<KeyBinding>, errors: &mut Vec<String>, keys: &str, action: Box<dyn Action>, ctx: Option<&str>, cx: &App) {
        let contexts = match ctx {
            Some(c) => vec![Some(c)],
            None => vec![Some("Respite"), Some("Input")],
        };
        for c in contexts {
            let pred = c.and_then(|c| KeyBindingContextPredicate::parse(c).ok()).map(Rc::new);
            match KeyBinding::load(keys, action.boxed_clone(), pred, false, None, cx) {
                Ok(b) => out.push(b),
                Err(e) => errors.push(format!("keymap: {keys}: {e}")),
            }
        }
    }
    for (keys, action, ctx) in defaults().into_iter().chain(preset(keymap)) {
        match ctx {
            Ctx::Editor => add(&mut bindings, &mut errors, &keys, action, Some("Input"), cx),
            Ctx::Both => add(&mut bindings, &mut errors, &keys, action, None, cx),
            Ctx::Global => match KeyBinding::load(&keys, action, None, false, None, cx) {
                Ok(b) => bindings.push(b),
                Err(e) => errors.push(format!("keymap: {keys}: {e}")),
            },
        }
    }
    if let Some(path) = user.filter(|p| p.exists()) {
        match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| parse(&t)) {
            Err(e) => errors.push(format!("{}: {e}", path.display())),
            Ok(entries) => {
                for (keys, name, arg, ctx) in entries {
                    let action = match name {
                        None => Ok(Box::new(NoAction) as Box<dyn Action>),
                        Some(n) => cx.build_action(&n, arg).map_err(|e| format!("{keys}: {e}")),
                    };
                    let action = match action {
                        Ok(a) => a,
                        Err(e) => {
                            errors.push(format!("{}: {e}", path.display()));
                            continue;
                        }
                    };
                    if let Some(c) = ctx.as_deref().filter(|c| KeyBindingContextPredicate::parse(c).is_err()) {
                        errors.push(format!("{}: {keys}: bad context '{c}'", path.display()));
                        continue;
                    }
                    add(&mut bindings, &mut errors, &keys, action, ctx.as_deref(), cx);
                }
            }
        }
    }
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
    errors
}

#[cfg(test)]
mod tests {
    #[test]
    fn user_keymap() {
        let k = super::parse(r#"[{"bindings": {"cmd-r": "respite::RunRequest"}}, {"context": "Input", "bindings": {"cmd-.": null, "f5": ["respite::RunRequest", {}]}}]"#).unwrap();
        assert_eq!(k.len(), 3);
        assert_eq!((k[0].0.as_str(), k[0].1.as_deref(), k[0].3.as_deref()), ("cmd-r", Some("respite::RunRequest"), None));
        assert!(k.iter().any(|e| e.0 == "cmd-." && e.1.is_none() && e.3.as_deref() == Some("Input")));
        assert!(super::parse(r#"[{"bindings": {"x": 1}}]"#).is_err());
        assert!(super::parse("{}").is_err());
    }
}
