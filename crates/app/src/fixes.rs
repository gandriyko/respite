//! Quick fixes (Cmd+. when nothing is running): define a missing variable in the file or an env
//! file, add `Content-Type` to a JSON body, drop a JSON trailing comma (body or env file).

use std::{ops::Range, path::PathBuf};

use anyhow::{Context as _, Result};
use gpui::{App, Entity, SharedString, Task, Window};
use gpui_component::input::{CodeActionProvider, EditorState, RopeExt as _};
use lsp_types::{CodeAction, CodeActionKind, ShowDocumentParams, TextEdit};
use serde_json::{Map, Value, json};

use crate::lang::Shared;

pub struct Fixes(pub Shared);

#[derive(Debug, PartialEq)]
pub enum Fix {
    /// Replace a byte range of the editor text.
    Edit { title: String, range: Range<usize>, text: String },
    /// Add `"name": ""` to an env file (created if missing) and open it there.
    DefineInEnv { title: String, file: PathBuf, name: String },
}

/// Fixes for the cursor's line (0-based `row`) of `src`.
pub fn fixes(src: &str, row: usize, ctx: &crate::lang::Ctx) -> Vec<Fix> {
    let mut out = vec![];
    if crate::workspace::is_env_file(&ctx.path) {
        if let Some(at) = http_syntax::trailing_comma(src) {
            out.push(Fix::Edit { title: "Remove trailing comma".into(), range: at..at + 1, text: String::new() });
        }
        return out;
    }
    let file = crate::highlight::parse(src);
    let line_no = row + 1;
    let line = src.lines().nth(row).unwrap_or("");
    let req = file.request_at(line_no);

    // `{{name}}` on this line that nothing defines.
    let prompted = |n: &str| {
        req.is_some_and(|r| r.directives.iter().any(|d| d.name == "prompt" && d.value.as_deref().and_then(|v| v.split_whitespace().next()) == Some(n)))
    };
    let mut rest = line;
    let mut seen = vec![];
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}").map(|c| c + open) else { break };
        let name = rest[open + 2..close].trim().to_string();
        rest = &rest[close + 2..];
        let plain = !name.is_empty() && !name.starts_with('$') && !name.contains(['.', ' ']);
        if !plain || seen.contains(&name) || file.variables.iter().any(|v| v.name == name) || ctx.all.contains_key(&name) || prompted(&name) {
            continue;
        }
        seen.push(name.clone());
        out.push(Fix::Edit { title: format!("Define @{name} in this file"), range: 0..0, text: format!("@{name} = \n") });
        let root = http_env::workspace_root(&ctx.path);
        let stem = ctx.env_name.as_ref().map_or("respite".to_string(), |e| format!("respite-{e}"));
        for private in [false, true] {
            let f = format!("{stem}{}.json", if private { ".private" } else { "" });
            out.push(Fix::DefineInEnv { title: format!("Define {name} in {f}"), file: root.join(&f), name: name.clone() });
        }
    }

    if let Some(r) = req {
        // A JSON-looking body without `Content-Type` (the server may not read it as JSON).
        if let Some(b) = &r.body
            && !r.headers.iter().any(|h| h.name.eq_ignore_ascii_case("content-type"))
            && b.text.trim_start().starts_with(['{', '['])
            && b.line >= 2
        {
            // Before the blank line that separates headers from the body.
            let at = src.split_inclusive('\n').take(b.line - 2).map(str::len).sum();
            out.push(Fix::Edit { title: "Add Content-Type: application/json".into(), range: at..at, text: "Content-Type: application/json\n".into() });
        }
        if let Some(b) = &r.body
            && let Some(at) = http_syntax::trailing_comma(&b.text)
        {
            let start: usize = src.split_inclusive('\n').take(b.line - 1).map(str::len).sum();
            out.push(Fix::Edit { title: "Remove trailing comma".into(), range: start + at..start + at + 1, text: String::new() });
        }
    }
    out
}

/// Add `name` to the env file (keeping the others and their order); its 0-based line.
fn define_in_env(file: &std::path::Path, name: &str) -> Result<u32> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".into(),
        Err(e) => return Err(e.into()),
    };
    let mut vars: Map<String, Value> = serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", file.display()))?;
    vars.entry(name).or_insert(Value::String(String::new()));
    let out = serde_json::to_string_pretty(&vars)? + "\n";
    std::fs::write(file, &out)?;
    let key = format!("\"{name}\"");
    Ok(out.lines().position(|l| l.trim_start().starts_with(&key)).unwrap_or(0) as u32)
}

impl CodeActionProvider for Fixes {
    fn id(&self) -> SharedString {
        "respite".into()
    }

    fn code_actions(&self, state: Entity<EditorState>, range: Range<usize>, _: &mut Window, cx: &mut App) -> Task<Result<Vec<CodeAction>>> {
        let s = state.read(cx);
        let row = s.text().offset_to_point(range.start).row;
        let found = fixes(&s.value(), row, &self.0.borrow());
        let actions = found
            .into_iter()
            .map(|f| {
                let (title, data) = match f {
                    Fix::Edit { title, range, text } => (title, json!({ "range": [range.start, range.end], "text": text })),
                    Fix::DefineInEnv { title, file, name } => (title, json!({ "file": file, "name": name })),
                };
                CodeAction { title, kind: Some(CodeActionKind::QUICKFIX), data: Some(data), ..Default::default() }
            })
            .collect();
        Task::ready(Ok(actions))
    }

    fn perform_code_action(&self, state: Entity<EditorState>, action: CodeAction, _: bool, window: &mut Window, cx: &mut App) -> Task<Result<()>> {
        let data = action.data.unwrap_or_default();
        let result = (|| -> Result<()> {
            if let (Some(file), Some(name)) = (data["file"].as_str(), data["name"].as_str()) {
                let file = PathBuf::from(file);
                let line = define_in_env(&file, name)?;
                // The env file opens with the cursor on the new variable, ready for its value.
                let uri = crate::lang::file_uri(&file).context("env file path")?;
                let show = state.read(cx).lsp().show_document.clone();
                if let Some(show) = show {
                    let pos = lsp_types::Position::new(line, 0);
                    let params = ShowDocumentParams { uri, external: None, take_focus: Some(true), selection: Some(lsp_types::Range::new(pos, pos)) };
                    show(&params, window, cx);
                }
                return Ok(());
            }
            let (Some(start), Some(end), Some(text)) = (data["range"][0].as_u64(), data["range"][1].as_u64(), data["text"].as_str()) else {
                anyhow::bail!("malformed quick fix");
            };
            state.update(cx, |s, cx| {
                let rope = s.text();
                let range = lsp_types::Range::new(rope.offset_to_position(start as usize), rope.offset_to_position(end as usize));
                s.apply_lsp_edits(&vec![TextEdit { range, new_text: text.into() }], window, cx);
            });
            Ok(())
        })();
        if let Err(e) = &result {
            eprintln!("respite: quick fix: {e:#}");
        }
        Task::ready(result)
    }
}

#[cfg(test)]
mod tests {
    use super::{Fix, fixes};
    use std::{collections::HashMap, path::PathBuf, sync::Arc};

    fn ctx(path: &str) -> crate::lang::Ctx {
        crate::lang::Ctx { path: Arc::new(PathBuf::from(path)), env: Arc::default(), all: Arc::new(HashMap::new()), env_name: Some("dev".into()) }
    }

    #[test]
    fn quick_fixes() {
        let src = "@host = x\nPOST {{host}}/{{id}}\n\n{\"a\": 1,}\n";
        let titles: Vec<String> = fixes(src, 1, &ctx("/p/a.http"))
            .into_iter()
            .map(|f| match f {
                Fix::Edit { title, .. } | Fix::DefineInEnv { title, .. } => title,
            })
            .collect();
        assert_eq!(
            titles,
            [
                "Define @id in this file",
                "Define id in respite-dev.json",
                "Define id in respite-dev.private.json",
                "Add Content-Type: application/json",
                "Remove trailing comma",
            ]
        );
        let edits: Vec<_> = fixes(src, 3, &ctx("/p/a.http"))
            .into_iter()
            .filter_map(|f| match f {
                Fix::Edit { range, text, .. } => Some((range, text)),
                _ => None,
            })
            .collect();
        let (comma, blank) = (src.find(",}").unwrap(), src.find("\n\n").unwrap() + 1);
        assert_eq!(edits, [(blank..blank, "Content-Type: application/json\n".into()), (comma..comma + 1, String::new())]);
        // Env file: only the comma.
        let env = "{\n  \"a\": \"1\",\n}\n";
        assert_eq!(fixes(env, 0, &ctx("/p/respite-dev.json")), [Fix::Edit { title: "Remove trailing comma".into(), range: 12..13, text: String::new() }]);
        assert_eq!(&env[12..13], ",");
    }
}
