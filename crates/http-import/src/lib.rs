//! Import API collections from other tools into `.http` files + `respite-<env>.json` env files.
//!
//! Formats (auto-detected): Postman collection v2.0/v2.1 and environment, Insomnia export
//! v4 (JSON) and v5 (YAML), OpenAPI 3 / Swagger 2 (JSON or YAML), Bruno collection folder,
//! HAR. Everything lands in a new folder named after the collection; nothing is overwritten.

mod bruno;
mod har;
mod insomnia;
mod openapi;
mod postman;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Tool-neutral collection: folders of requests plus environments.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Collection {
    pub name: String,
    pub format: &'static str,
    pub root: Folder,
    /// Environment name (`$shared` for variables of every environment) → variables.
    pub envs: BTreeMap<String, Vec<(String, String)>>,
    /// Same, for secrets (written to `respite-<env>.private.json`).
    pub private_envs: BTreeMap<String, Vec<(String, String)>>,
    /// What could not be converted (scripts, unsupported auth, …).
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Folder {
    pub name: String,
    pub requests: Vec<Req>,
    pub folders: Vec<Folder>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Req {
    pub name: String,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Written as `#` comments above the request line.
    pub notes: Vec<String>,
    /// File variables (`@name = value`) the request needs, e.g. OpenAPI path parameters.
    pub vars: Vec<(String, String)>,
}

impl Req {
    pub fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
    }

    /// Add a header unless the request already sets it.
    pub fn default_header(&mut self, name: &str, value: impl Into<String>) {
        if !self.has_header(name) {
            self.headers.push((name.into(), value.into()));
        }
    }
}

/// Read and convert `path` (a file, or a Bruno collection folder).
pub fn import(path: &Path) -> Result<Collection, String> {
    if path.is_dir() {
        return if path.join("bruno.json").is_file() {
            bruno::collection(path)
        } else {
            Err(format!("{}: not a Bruno collection (no bruno.json)", path.display()))
        };
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let fallback = path.file_stem().map_or_else(|| "imported".into(), |s| s.to_string_lossy().into_owned());
    import_str(&text, &fallback)
}

/// Convert file contents; `fallback_name` names collections that carry no name.
pub fn import_str(text: &str, fallback_name: &str) -> Result<Collection, String> {
    let v: Value = serde_json::from_str(text)
        .or_else(|_| serde_norway::from_str(text))
        .map_err(|e| format!("neither JSON nor YAML: {e}"))?;
    let schema = v["info"]["schema"].as_str().unwrap_or_default();
    let mut c = if schema.contains("getpostman.com") || v["info"]["_postman_id"].is_string() {
        postman::collection(&v)
    } else if v["_postman_variable_scope"].is_string() || (v["values"].is_array() && v["name"].is_string() && v["item"].is_null()) {
        postman::environment(&v)
    } else if v["_type"] == "export" {
        insomnia::v4(&v)
    } else if v["type"].as_str().is_some_and(|t| t.starts_with("collection.insomnia.rest/")) {
        insomnia::v5(&v)
    } else if v["openapi"].is_string() || v["swagger"].is_string() {
        openapi::spec(&v)
    } else if v["log"]["entries"].is_array() {
        har::log(&v)
    } else {
        return Err("unknown format: expected Postman, Insomnia, OpenAPI/Swagger, Bruno or HAR".into());
    };
    if c.name.trim().is_empty() {
        c.name = fallback_name.into();
    }
    Ok(c)
}

/// `.http` files of a collection: a folder's requests go to `<folder>.http`; a folder that has
/// sub-folders also becomes a directory holding them. Top-level requests go to `<collection>.http`.
pub fn render(c: &Collection) -> Vec<(PathBuf, String)> {
    fn walk(f: &Folder, dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        if !f.requests.is_empty() {
            // File variables are file-wide: declare each one once, at its first use.
            let mut seen = std::collections::HashSet::new();
            let text = f
                .requests
                .iter()
                .map(|r| block(&Req { vars: r.vars.iter().filter(|(k, _)| seen.insert(k.clone())).cloned().collect(), ..r.clone() }))
                .collect::<Vec<_>>()
                .join("\n");
            out.push((unique(out, dir.join(format!("{}.http", sanitize(&f.name)))), text));
        }
        for sub in &f.folders {
            let sub_dir = if sub.folders.is_empty() { dir.to_path_buf() } else { dir.join(sanitize(&sub.name)) };
            walk(sub, &sub_dir, out);
        }
    }
    // Case-insensitive: `Pets.http` and `pets.http` are one file on macOS / Windows.
    fn unique(out: &[(PathBuf, String)], p: PathBuf) -> PathBuf {
        let stem = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let taken = |c: &Path| out.iter().any(|(q, _)| q.to_string_lossy().to_lowercase() == c.to_string_lossy().to_lowercase());
        (1..).map(|n| if n == 1 { p.clone() } else { p.with_file_name(format!("{stem}-{n}.http")) }).find(|c| !taken(c)).unwrap()
    }
    let mut out = vec![];
    let root = Folder { name: c.name.clone(), ..c.root.clone() };
    walk(&root, Path::new(""), &mut out);
    out
}

/// One request block.
pub fn block(r: &Req) -> String {
    let mut s = format!("### {}\n", r.name.trim());
    for n in &r.notes {
        for line in n.lines() {
            s += &format!("# {line}\n");
        }
    }
    for (k, v) in &r.vars {
        s += format!("@{k} = {v}").trim_end();
        s += "\n";
    }
    s += &format!("{} {}\n", r.method.to_ascii_uppercase(), r.url);
    for (k, v) in &r.headers {
        s += &format!("{k}: {v}\n");
    }
    if let Some(b) = r.body.as_deref().filter(|b| !b.trim().is_empty()) {
        s += &format!("\n{}\n", b.trim_end());
    }
    s
}

/// Env file of an environment: `respite.json` for `$shared`, else `respite-<env>.json`.
pub(crate) fn env_file(env: &str, private: bool) -> String {
    // `x.private` would read back as the private env `x`: write it as `x-private`.
    let env = sanitize(env);
    let env = env.strip_suffix(".private").map_or(env.clone(), |x| format!("{x}-private"));
    let stem = if env == "$shared" { "respite".to_string() } else { format!("respite-{env}") };
    format!("{stem}{}.json", if private { ".private" } else { "" })
}

/// One env file per environment (public and private) in `dir`. An environment whose files
/// already exist gets the next free name (`dev-2`, …): nothing is replaced.
fn write_envs(c: &Collection, dir: &Path) -> io::Result<Vec<PathBuf>> {
    let taken = |env: &str| [false, true].iter().any(|p| dir.join(env_file(env, *p)).exists());
    let mut written = vec![];
    for env in c.envs.keys().chain(c.private_envs.keys()).collect::<std::collections::BTreeSet<_>>() {
        let name = (1..).map(|n| if n == 1 { env.clone() } else { format!("{env}-{n}") }).find(|n| !taken(n)).unwrap();
        for (private, envs) in [(false, &c.envs), (true, &c.private_envs)] {
            if let Some(vars) = envs.get(env) {
                let json: Map<String, Value> = vars.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
                let path = dir.join(env_file(&name, private));
                std::fs::write(&path, serde_json::to_string_pretty(&json)? + "\n")?;
                written.push(path);
            }
        }
    }
    Ok(written)
}

/// Write the collection into a new folder under `dest` (`<dest>/<name>`, `-2`, `-3`… if taken).
/// An environment-only import (a Postman environment file) goes to `dest` itself instead,
/// without replacing existing environments. Returns the files written.
pub fn write(c: &Collection, dest: &Path) -> io::Result<Vec<PathBuf>> {
    let files = render(c);
    if files.is_empty() && !(c.envs.is_empty() && c.private_envs.is_empty()) {
        std::fs::create_dir_all(dest)?;
        return write_envs(c, dest);
    }
    let base = dest.join(sanitize(&c.name));
    let dir = (1..).map(|n| if n == 1 { base.clone() } else { PathBuf::from(format!("{}-{n}", base.display())) }).find(|d| !d.exists()).unwrap();
    let mut written = vec![];
    for (rel, text) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap_or(&dir))?;
        std::fs::write(&path, text)?;
        written.push(path);
    }
    std::fs::create_dir_all(&dir)?;
    written.extend(write_envs(c, &dir)?);
    Ok(written)
}

/// File / folder name from a collection or folder name.
pub fn sanitize(name: &str) -> String {
    let s: String = name.trim().chars().map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c }).collect();
    if s.is_empty() { "untitled".into() } else { s }
}

/// JSON value as an env-file string (strings as is, others as JSON).
pub(crate) fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Object(_) | Value::Array(_) => None,
        other => Some(other.to_string()),
    }
}

/// Form-urlencoded body written one pair per line (`a=1⏎&b=2`), as `.http` files do.
pub(crate) fn form_body(pairs: &[(String, String)]) -> String {
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n&")
}

/// Multipart body with a fixed boundary; `Err(path)` parts are files (`< path`).
pub(crate) fn multipart(req: &mut Req, parts: &[(String, Result<String, String>)]) {
    const BOUNDARY: &str = "RespiteFormBoundary";
    let mut out = vec![];
    for (name, part) in parts {
        out.push(format!("--{BOUNDARY}"));
        match part {
            Ok(value) => {
                out.push(format!("Content-Disposition: form-data; name=\"{name}\""));
                out.push(String::new());
                out.push(value.clone());
            }
            Err(file) => {
                let filename = file.rsplit(['/', '\\']).next().unwrap_or(file);
                out.push(format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\""));
                out.push(String::new());
                out.push(format!("< {file}"));
            }
        }
    }
    out.push(format!("--{BOUNDARY}--"));
    req.headers.retain(|(n, _)| !n.eq_ignore_ascii_case("content-type"));
    req.headers.push(("Content-Type".into(), format!("multipart/form-data; boundary={BOUNDARY}")));
    req.body = Some(out.join("\n"));
}

/// GraphQL as `.http` writes it: query, blank line, variables JSON.
pub(crate) fn graphql(req: &mut Req, query: &str, variables: Option<&str>) {
    req.default_header("X-Request-Type", "GraphQL");
    let vars = variables.map(str::trim).filter(|v| !v.is_empty() && *v != "{}");
    req.body = Some(match vars {
        Some(v) => format!("{}\n\n{v}", query.trim()),
        None => query.trim().to_string(),
    });
}

/// Pretty-print a JSON body when it parses; leave anything else as is.
pub(crate) fn pretty_json(text: &str) -> String {
    serde_json::from_str::<Value>(text).ok().and_then(|v| serde_json::to_string_pretty(&v).ok()).unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests;
