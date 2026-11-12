//! Settings: `~/.config/respite/settings.json` (global) overlaid by
//! `<workspace>/.respite/settings.json` (per project, can be committed).
//!
//! JSON with `//` and `/* */` comments, nested by group (`request.timeout_ms` is
//! `{"request": {"timeout_ms": …}}`). Missing keys take defaults, unknown keys are ignored.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Which variant of the theme is shown; `System` follows the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    /// Theme family (`"Gruvbox"`, `"Catppuccin"`, …); `"Default"` = built-in.
    pub theme: String,
    /// UI text size, px.
    pub ui_font_size: f32,
    /// Editor and response font; empty = the theme's monospace font.
    pub font_family: String,
    /// Editor and response font size, px.
    pub font_size: f32,
    pub editor: Editor,
    pub response: ResponseView,
    pub request: Request,
    pub history: History,
    pub curl: Curl,
    pub env: Env,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Editor {
    pub line_numbers: bool,
    pub soft_wrap: bool,
    /// `200 OK · 142 ms · 3.4 KB` after the request line.
    pub inlay_status: bool,
    /// Background on the line under the cursor.
    pub highlight_current_line: bool,
}

/// The response panel (`"response": {…}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResponseView {
    /// Wrap long lines (toggled from the panel's Wrap button).
    pub soft_wrap: bool,
}

impl Default for ResponseView {
    fn default() -> Self {
        Self { soft_wrap: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Request {
    /// Applies when a request has no `# @timeout`; 0 = no limit.
    pub timeout_ms: u64,
    pub follow_redirects: bool,
    pub max_redirects: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub per_request: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Curl {
    pub auto_convert_on_paste: bool,
    pub keep_original_as_comment: bool,
    /// Header-name prefixes dropped on import (browser noise).
    pub strip_headers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Env {
    /// Extra env files above the chain; `~/` is the home directory, relative = workspace root.
    pub files: Vec<String>,
    /// Environments shown with a red warning; `*` matches any suffix, case-insensitive.
    pub prod_pattern: String,
}

impl Default for Env {
    fn default() -> Self {
        Self { files: vec![], prod_pattern: "prod*".into() }
    }
}

impl Env {
    pub fn is_prod(&self, name: &str) -> bool {
        let (p, n) = (self.prod_pattern.to_lowercase(), name.to_lowercase());
        match p.strip_suffix('*') {
            Some(prefix) => n.starts_with(prefix),
            None => !p.is_empty() && n == p,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            theme: "Default".into(),
            ui_font_size: 14.,
            font_family: String::new(),
            font_size: 14.,
            editor: Editor::default(),
            response: ResponseView::default(),
            request: Request::default(),
            history: History::default(),
            curl: Curl::default(),
            env: Env::default(),
        }
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self { line_numbers: true, soft_wrap: false, inlay_status: true, highlight_current_line: false }
    }
}

impl Default for Request {
    fn default() -> Self {
        Self { timeout_ms: 0, follow_redirects: true, max_redirects: 10 }
    }
}

impl Default for History {
    fn default() -> Self {
        Self { per_request: 50, total: 500 }
    }
}

impl Default for Curl {
    fn default() -> Self {
        let strip = ["sec-ch-ua", "sec-fetch-", "priority"].map(String::from).to_vec();
        Self { auto_convert_on_paste: true, keep_original_as_comment: true, strip_headers: strip }
    }
}

impl Settings {
    /// `env.files` as paths (`~/` → home, relative → `root`).
    pub fn env_files(&self, root: &Path) -> Vec<PathBuf> {
        let home = std::env::home_dir().unwrap_or_default();
        self.env.files.iter().map(|p| p.strip_prefix("~/").map_or_else(|| root.join(p), |rest| home.join(rest))).collect()
    }
}

/// Both layers as written, plus the merged result.
#[derive(Clone)]
pub struct Loaded {
    pub global_path: Option<PathBuf>,
    pub workspace_path: Option<PathBuf>,
    global: Value,
    workspace: Value,
    pub settings: Settings,
    /// Unreadable / invalid files (their layer is skipped).
    pub errors: Vec<String>,
}

/// `~/.config/respite/settings.json`
pub fn global_path() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".config/respite/settings.json"))
}

impl Loaded {
    pub fn load(root: Option<&Path>) -> Self {
        Self::load_from(global_path(), root.map(|r| r.join(".respite/settings.json")))
    }

    pub fn load_from(global_path: Option<PathBuf>, workspace_path: Option<PathBuf>) -> Self {
        let mut errors = vec![];
        let mut read = |p: &Option<PathBuf>| match p.as_deref().map(read_jsonc) {
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                errors.push(format!("{}: {e}", p.as_ref().unwrap().display()));
                Value::Object(Map::new())
            }
            None => Value::Object(Map::new()),
        };
        let (global, workspace) = (read(&global_path), read(&workspace_path));
        let mut loaded = Self { global_path, workspace_path, global, workspace, settings: Settings::default(), errors };
        loaded.merge();
        loaded
    }

    fn merge(&mut self) {
        let mut v = serde_json::to_value(Settings::default()).expect("defaults serialize");
        deep_merge(&mut v, &self.global);
        deep_merge(&mut v, &self.workspace);
        match serde_json::from_value(v) {
            Ok(s) => self.settings = s,
            Err(e) => self.errors.push(format!("settings: {e}")),
        }
    }

    /// Set a dotted key (`"request.timeout_ms"`) in the global file and re-merge.
    /// Comments in the global file are not preserved once the UI writes it.
    pub fn set(&mut self, key: &str, value: Value) -> io::Result<()> {
        let path = self.global_path.clone().ok_or_else(|| io::Error::other("no home directory"))?;
        let mut node = &mut self.global;
        for part in key.split('.') {
            if !node.is_object() {
                *node = Value::Object(Map::new());
            }
            node = node.as_object_mut().unwrap().entry(part).or_insert(Value::Null);
        }
        *node = value;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&self.global)? + "\n")?;
        self.merge();
        Ok(())
    }

    /// The workspace file overrides `key`, so editing the global value has no effect.
    pub fn overridden(&self, key: &str) -> bool {
        key.split('.').try_fold(&self.workspace, |v, k| v.get(k)).is_some()
    }
}

fn read_jsonc(path: &Path) -> Result<Value, String> {
    match std::fs::File::open(path) {
        Ok(f) => serde_json::from_reader(json_comments::StripComments::new(f)).map_err(|e| e.to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(e.to_string()),
    }
}

/// Objects merge key by key; anything else (arrays included) replaces.
fn deep_merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                deep_merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, o) => *b = o.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_comments_and_set() {
        let dir = tempfile::tempdir().unwrap();
        let (g, w) = (dir.path().join("cfg/settings.json"), dir.path().join("ws.json"));
        std::fs::write(&w, "{\n // project wants a short timeout\n \"request\": {\"timeout_ms\": 500}, \"appearance\": \"light\" /* why not */\n}").unwrap();

        let mut l = Loaded::load_from(Some(g.clone()), Some(w));
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert_eq!((l.settings.appearance, l.settings.request.timeout_ms), (Appearance::Light, 500));
        assert_eq!(l.settings.theme, "Default");
        assert!(l.settings.request.follow_redirects); // default kept beside the override
        assert!(l.overridden("request.timeout_ms") && !l.overridden("font_size"));

        l.set("font_size", 16.into()).unwrap();
        l.set("request.timeout_ms", 9000.into()).unwrap();
        assert_eq!((l.settings.font_size, l.settings.request.timeout_ms), (16., 500)); // workspace still wins
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&g).unwrap()).unwrap();
        assert_eq!(written, serde_json::json!({"font_size": 16, "request": {"timeout_ms": 9000}}));

        let env = Env::default();
        assert!(env.is_prod("prod") && env.is_prod("Production") && !env.is_prod("dev"));

        std::fs::write(&g, "{ broken").unwrap();
        let l = Loaded::load_from(Some(g), None);
        assert_eq!((l.errors.len(), l.settings.font_size), (1, 14.));
    }
}
