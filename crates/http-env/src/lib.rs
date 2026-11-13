//! Environments and `{{variable}}` resolution.
//!
//! Env files, one per environment (flat `{"baseUrl": "…"}`): `respite.json` (shared by all),
//! `respite-<env>.json`, and `respite-<env>.private.json` / `respite.private.json` for secrets.
//! Searched in every directory from the workspace root down to the `.http` file; within a
//! directory public before private, deeper directories override shallower ones.

mod dynamic;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use http_syntax::{Diagnostic, FileVariable, HttpFile, Severity};
use serde_json::{Map, Value};

/// Respite env file name → (environment key, private): `respite-<env>.json`,
/// `respite-<env>.private.json`; `respite.json` / `respite.private.json` hold `$shared`.
pub fn respite_env_file(name: &str) -> Option<(String, bool)> {
    match name {
        "respite.json" => return Some(("$shared".into(), false)),
        "respite.private.json" => return Some(("$shared".into(), true)),
        _ => {}
    }
    let mid = name.strip_prefix("respite-")?.strip_suffix(".json")?;
    let (env, private) = mid.strip_suffix(".private").map_or((mid, false), |env| (env, true));
    (!env.is_empty()).then(|| (env.to_string(), private))
}

/// An env file (for UIs listing / opening them as JSON).
pub fn is_env_file(path: &Path) -> bool {
    path.file_name().is_some_and(|n| respite_env_file(&n.to_string_lossy()).is_some())
}

/// Respite env files in `dir`, public before private, by name.
fn respite_files(dir: &Path) -> Vec<(PathBuf, String, bool)> {
    let mut found: Vec<(PathBuf, String, bool)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let (env, private) = respite_env_file(&e.file_name().to_string_lossy())?;
            e.path().is_file().then(|| (e.path(), env, private))
        })
        .collect();
    found.sort_by(|a, b| (a.2, &a.0).cmp(&(b.2, &b.0)));
    found
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub value: String,
    pub source: PathBuf,
    /// Came from a private env file: mask it in output for agents.
    pub secret: bool,
}

#[derive(Debug, Default)]
pub struct Environments {
    files: Vec<EnvFile>,
    /// Unreadable / invalid env files; merging goes on without them.
    pub errors: Vec<(PathBuf, String)>,
}

#[derive(Debug)]
struct EnvFile {
    path: PathBuf,
    private: bool,
    json: Map<String, Value>,
}

impl Environments {
    /// Env files from `root` down to the `.http` file's directory (public, then private in each),
    /// then `extra` files (`envFiles` / `--env-file`, named like the others), later ones
    /// overriding earlier ones.
    pub fn load(http_file: &Path, root: &Path, extra: &[PathBuf]) -> Self {
        let dir = http_file.parent().unwrap_or(root);
        let mut dirs: Vec<&Path> = dir.ancestors().take_while(|d| d.starts_with(root)).collect();
        dirs.reverse();
        let mut paths: Vec<(PathBuf, Option<(String, bool)>)> =
            dirs.into_iter().flat_map(respite_files).map(|(p, env, private)| (p, Some((env, private)))).collect();
        paths.extend(extra.iter().map(|p| (p.clone(), p.file_name().and_then(|n| respite_env_file(&n.to_string_lossy())))));

        let mut envs = Self::default();
        for (path, key) in paths {
            let Some((env, private)) = key else {
                envs.errors.push((path, "not an env file name: respite.json, respite-<env>.json or respite-<env>.private.json".into()));
                continue;
            };
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<Map<String, Value>>(&s).map_err(|e| e.to_string()));
            match parsed {
                // Keyed by environment, so merging and listing work across files.
                Ok(vars) => envs.files.push(EnvFile { path, private, json: Map::from_iter([(env, Value::Object(vars))]) }),
                Err(e) => envs.errors.push((path, e)),
            }
        }
        envs
    }

    /// Every env file in the chain, broken ones included (they still need editing).
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.files.iter().map(|f| f.path.clone()).chain(self.errors.iter().map(|e| e.0.clone())).collect();
        paths.sort();
        paths
    }

    /// Union of environment names from all files (including private-only ones).
    pub fn names(&self) -> Vec<String> {
        let names: BTreeSet<&String> = self.files.iter().flat_map(|f| f.json.keys()).filter(|k| *k != "$shared").collect();
        names.into_iter().cloned().collect()
    }

    /// Variables of every environment together (a name defined in any of them counts).
    pub fn all_variables(&self) -> HashMap<String, Variable> {
        let mut out = self.variables(None);
        for name in self.names() {
            out.extend(self.variables(Some(&name)));
        }
        out
    }

    /// Merged variables: `$shared` from every file, then the `env` block from every file.
    /// Shallow merge by key; objects (e.g. `SSLConfiguration`) are not variables.
    ///
    /// `Security.Auth.<id>` (JetBrains HTTP Client's OAuth 2.0 config) is merged field by field
    /// across files, so a private file can add just `"Client Secret"`. Each config comes back as
    /// `$auth.<id>` holding its JSON — a name no `{{variable}}` can reach (`{{$…}}` is dynamic).
    ///
    /// `SSLConfiguration` (JetBrains' client certificate block, plus our `caCertificate`) is merged
    /// the same way into `$ssl`, its file paths made absolute against the env file that names them.
    pub fn variables(&self, env: Option<&str>) -> HashMap<String, Variable> {
        let mut out = HashMap::new();
        let mut auth: BTreeMap<String, (Map<String, Value>, Variable)> = BTreeMap::new();
        let mut ssl: Option<(Map<String, Value>, Variable)> = None;
        for key in std::iter::once("$shared").chain(env) {
            for f in &self.files {
                let Some(Value::Object(block)) = f.json.get(key) else { continue };
                for (k, v) in block {
                    let value = match v {
                        Value::String(s) => s.clone(),
                        Value::Object(o) if k == "Security" => {
                            let Some(Value::Object(configs)) = o.get("Auth") else { continue };
                            for (id, cfg) in configs {
                                let Value::Object(cfg) = cfg else { continue };
                                let e = auth.entry(id.clone()).or_insert_with(|| (Map::new(), Variable { value: String::new(), source: f.path.clone(), secret: false }));
                                e.0.extend(cfg.clone());
                                e.1.secret |= f.private;
                            }
                            continue;
                        }
                        Value::Object(o) if k == "SSLConfiguration" => {
                            let dir = f.path.parent().unwrap_or(Path::new("."));
                            let e = ssl.get_or_insert_with(|| (Map::new(), Variable { value: String::new(), source: f.path.clone(), secret: false }));
                            for (field, v) in o {
                                let v = match field.as_str() {
                                    "clientCertificate" | "clientCertificateKey" | "caCertificate" => absolute_paths(v, dir),
                                    _ => v.clone(),
                                };
                                e.0.insert(field.clone(), v);
                            }
                            e.1.source = f.path.clone();
                            continue;
                        }
                        Value::Object(_) | Value::Array(_) => continue,
                        other => other.to_string(),
                    };
                    out.insert(k.clone(), Variable { value, source: f.path.clone(), secret: f.private });
                }
            }
        }
        for (id, (cfg, var)) in auth {
            out.insert(format!("$auth.{id}"), Variable { value: Value::Object(cfg).to_string(), ..var });
        }
        if let Some((cfg, var)) = ssl {
            out.insert("$ssl".into(), Variable { value: Value::Object(cfg).to_string(), ..var });
        }
        out
    }
}

/// Root of a `.http` file's env chain, history and saved state; the app and the CLI share it.
/// See [`root_in`]; the sidebar's projects come from `~/.respite/workspaces.json`.
pub fn workspace_root(file: &Path) -> PathBuf {
    let home = std::env::home_dir();
    let projects: Vec<PathBuf> = home
        .as_ref()
        .and_then(|h| std::fs::read(h.join(".respite/workspaces.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    root_in(file, home.as_deref(), collections_dir().as_deref(), &projects)
}

/// In order: the deepest sidebar project holding `file`; the nearest ancestor with `.git` or
/// `.respite`; its collection (`<collections>/<name>`, each one its own root); its directory.
/// Neither `home` (its `.respite` is the app's data folder) nor `collections` itself is a root.
pub fn root_in(file: &Path, home: Option<&Path>, collections: Option<&Path>, projects: &[PathBuf]) -> PathBuf {
    let dir = file.parent().unwrap_or(Path::new("."));
    if let Some(p) = projects.iter().filter(|p| file.starts_with(p) && Some(p.as_path()) != collections).max_by_key(|p| p.components().count()) {
        return p.clone();
    }
    for d in dir.ancestors().take_while(|d| Some(*d) != home && Some(*d) != collections) {
        if d.parent() == collections || d.join(".git").exists() || d.join(".respite").exists() {
            return d.to_path_buf();
        }
    }
    dir.to_path_buf()
}

/// `~/.config/respite`: the default home of collections (one folder each), next to `settings.json`.
pub fn collections_dir() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".config/respite"))
}

/// Undefined `{{variables}}`, unknown `{{name.response…}}` requests, cycles and bad `{{$…}}`,
/// checked against the union of all environments (a variable defined in any of them is fine).
pub fn check(file: &HttpFile, envs: &Environments, dir: &Path) -> Vec<Diagnostic> {
    check_vars(file, &envs.all_variables(), dir)
}

/// [`check`] against already merged variables ([`Environments::all_variables`]).
pub fn check_vars(file: &HttpFile, env: &HashMap<String, Variable>, dir: &Path) -> Vec<Diagnostic> {
    let names: HashSet<&str> = file.requests.iter().filter_map(|r| r.name.as_deref()).collect();
    let refs = |name: &str, _: &str| (names.contains(name) || (name.starts_with("$auth.") && env.contains_key(name))).then(String::new);
    let mut out: Vec<Diagnostic> = vec![];
    for r in &file.requests {
        // `@prompt` variables get their value at send time.
        let overrides: HashMap<String, String> = r
            .directives
            .iter()
            .filter(|d| d.name == "prompt")
            .filter_map(|d| d.value.as_deref()?.split_whitespace().next())
            .map(|v| (v.to_string(), String::new()))
            .collect();
        let resolver = Resolver { overrides: &overrides, file_vars: &file.variables, env, dir };
        let body = r.body.iter().flat_map(|b| b.text.lines().enumerate().map(move |(i, l)| (b.line + i, l)));
        let pieces = std::iter::once((r.request_line, r.url.as_str()))
            .chain(r.headers.iter().flat_map(|h| [(h.line, h.name.as_str()), (h.line, h.value.as_str())]))
            .chain(body);
        for (line, text) in pieces {
            let mut rest = text;
            while let Some(open) = rest.find("{{") {
                let Some(close) = rest[open..].find("}}").map(|c| c + open + 2) else { break };
                if let Err(e) = resolver.resolve(&rest[open..close], &refs) {
                    let (severity, code) = match &e {
                        // Depends on the machine running it, not on the file.
                        ResolveError::Undefined(v) if v.starts_with('$') => (Severity::Warning, "undefined_variable"),
                        ResolveError::Undefined(_) => (Severity::Error, "undefined_variable"),
                        ResolveError::Reference(v) if v.starts_with("$auth.") => (Severity::Error, "unknown_auth"),
                        ResolveError::Reference(_) => (Severity::Error, "unknown_request"),
                        ResolveError::Cycle(_) => (Severity::Error, "cyclic_variable"),
                        ResolveError::Dynamic(_) => (Severity::Error, "invalid_dynamic_variable"),
                    };
                    let d = Diagnostic { line, severity, code, message: e.to_string() };
                    if !out.contains(&d) {
                        out.push(d);
                    }
                }
                rest = &rest[close..];
            }
        }
    }
    out
}

/// `(name, resolved value)` of env and file variables — for turning pasted literals back
/// into `{{name}}` (cURL import). Unresolvable file variables are skipped.
pub fn known_values(file: &HttpFile, env: &HashMap<String, Variable>, dir: &Path) -> Vec<(String, String)> {
    let overrides = HashMap::new();
    let r = Resolver { overrides: &overrides, file_vars: &file.variables, env, dir };
    let mut out: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.value.clone())).collect();
    for v in &file.variables {
        if let Ok(value) = r.resolve(&format!("{{{{{}}}}}", v.name), &|_, _| None) {
            out.push((v.name.clone(), value));
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    Undefined(String),
    Cycle(String),
    /// `{{name.response…}}` / `{{name.request…}}` the reference lookup could not answer.
    Reference(String),
    /// Malformed `{{$…}}`.
    Dynamic(String),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undefined(v) => write!(f, "undefined variable '{v}'"),
            Self::Cycle(v) => write!(f, "cyclic variable reference through '{v}'"),
            Self::Reference(v) if v.starts_with("$auth.") => {
                write!(f, "cannot resolve '{v}': no such config in Security.Auth of the env files, or no token")
            }
            Self::Reference(v) => write!(f, "cannot resolve '{v}'"),
            Self::Dynamic(v) => write!(f, "invalid dynamic variable '{v}'"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Answers `{{name.<path>}}`: `(name, "response.body.$.token")` → value.
pub type References<'r> = &'r dyn Fn(&str, &str) -> Option<String>;

/// Precedence: `overrides` (prompt / `--var`) > file variables > env (private > public > `$shared`).
pub struct Resolver<'a> {
    pub overrides: &'a HashMap<String, String>,
    pub file_vars: &'a [FileVariable],
    pub env: &'a HashMap<String, Variable>,
    /// Directory of the `.http` file; `$dotenv` reads `.env` there.
    pub dir: &'a Path,
}

impl Resolver<'_> {
    pub fn resolve(&self, text: &str, refs: References) -> Result<String, ResolveError> {
        self.subst(text, refs, &mut vec![])
    }

    fn subst(&self, text: &str, refs: References, stack: &mut Vec<String>) -> Result<String, ResolveError> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(open) = rest.find("{{") {
            let Some(close) = rest[open..].find("}}").map(|c| c + open) else { break };
            out.push_str(&rest[..open]);
            out.push_str(&self.value(rest[open + 2..close].trim(), refs, stack)?);
            rest = &rest[close + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn value(&self, expr: &str, refs: References, stack: &mut Vec<String>) -> Result<String, ResolveError> {
        // `$auth.token("id")` / `$auth.idToken("id")` → refs("$auth.id", "token" | "idToken").
        if let Some(call) = expr.strip_prefix("$auth.") {
            let (kind, id) = auth_call(call).ok_or_else(|| ResolveError::Dynamic(expr.into()))?;
            return refs(&format!("$auth.{id}"), kind).ok_or_else(|| ResolveError::Reference(expr.into()));
        }
        if let Some(d) = expr.strip_prefix('$') {
            return dynamic::eval(d, self.dir, &mut |name| self.value(name, refs, stack));
        }
        if let Some(v) = self.overrides.get(expr) {
            return Ok(v.clone());
        }
        if let Some(fv) = self.file_vars.iter().rev().find(|v| v.name == expr) {
            if stack.iter().any(|s| s == expr) {
                return Err(ResolveError::Cycle(expr.into()));
            }
            stack.push(expr.into());
            let v = self.subst(&fv.value, refs, stack);
            stack.pop();
            return v;
        }
        if let Some(v) = self.env.get(expr) {
            return Ok(v.value.clone());
        }
        if let Some((name, path)) = expr.split_once('.')
            && (path.starts_with("response.") || path.starts_with("request."))
        {
            return refs(name, path).ok_or_else(|| ResolveError::Reference(expr.into()));
        }
        Err(ResolveError::Undefined(expr.into()))
    }
}

/// A path (`"a.pem"`), `{"path": …, "format": …}`, or a list of either, made absolute against `dir`.
fn absolute_paths(v: &Value, dir: &Path) -> Value {
    match v {
        Value::String(p) => Value::String(dir.join(p).to_string_lossy().into_owned()),
        Value::Object(o) => {
            let mut o = o.clone();
            if let Some(p) = o.get("path").cloned() {
                o.insert("path".into(), absolute_paths(&p, dir));
            }
            Value::Object(o)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| absolute_paths(x, dir)).collect()),
        other => other.clone(),
    }
}

/// `token("api")` → `("token", "api")`; also `idToken`, single quotes.
pub fn auth_call(call: &str) -> Option<(&'static str, &str)> {
    let (kind, rest) = call.split_once('(')?;
    let kind = ["token", "idToken"].into_iter().find(|k| *k == kind.trim())?;
    let arg = rest.strip_suffix(')')?.trim();
    let id = arg.strip_prefix('"').and_then(|a| a.strip_suffix('"')).or_else(|| arg.strip_prefix('\'').and_then(|a| a.strip_suffix('\'')))?;
    (!id.is_empty()).then_some((kind, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn auth_configs() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let auth = |cfg: &str| format!(r#"{{"Security": {{"Auth": {{"api": {cfg}}}}}}}"#);
        fs::write(r.join("respite-dev.json"), auth(r#"{"Type": "OAuth2", "Client ID": "c", "Client Secret": "{{secret}}"}"#)).unwrap();
        fs::write(r.join("respite-dev.private.json"), auth(r#"{"Client Secret": "s3"}"#)).unwrap();
        let envs = Environments::load(&r.join("a.http"), r, &[]);
        let v = envs.variables(Some("dev"));
        let cfg: Value = serde_json::from_str(&v["$auth.api"].value).unwrap();
        assert_eq!((cfg["Client ID"].as_str(), cfg["Client Secret"].as_str(), v["$auth.api"].secret), (Some("c"), Some("s3"), true));
        assert!(!v.contains_key("Security"));

        // SSLConfiguration: merged field by field, paths absolute against their own file.
        fs::create_dir_all(r.join("api")).unwrap();
        fs::write(r.join("respite.json"), r#"{"SSLConfiguration": {"caCertificate": "certs/ca.pem", "verifyHostCertificate": true}}"#).unwrap();
        fs::write(
            r.join("api/respite-dev.private.json"),
            r#"{"SSLConfiguration": {"clientCertificate": {"path": "c.der", "format": "DER"}, "clientCertificateKey": "/abs/k.pem"}}"#,
        )
        .unwrap();
        let v = Environments::load(&r.join("api/a.http"), r, &[]).variables(Some("dev"));
        let ssl: Value = serde_json::from_str(&v["$ssl"].value).unwrap();
        assert_eq!(ssl["caCertificate"], Value::String(r.join("certs/ca.pem").to_string_lossy().into()));
        assert_eq!(ssl["clientCertificate"]["path"], Value::String(r.join("api/c.der").to_string_lossy().into()));
        assert_eq!((ssl["clientCertificateKey"].as_str(), ssl["verifyHostCertificate"].as_bool()), (Some("/abs/k.pem"), Some(true)));

        assert_eq!(auth_call(r#"token("api")"#), Some(("token", "api")));
        assert_eq!(auth_call("idToken('a b')"), Some(("idToken", "a b")));
        assert_eq!([auth_call("token(api)"), auth_call(r#"secret("a")"#), auth_call(r#"token("")"#)], [None; 3]);
        let (o, files) = (HashMap::new(), vec![]);
        let res = Resolver { overrides: &o, file_vars: &files, env: &v, dir: r };
        let refs = |name: &str, kind: &str| (name == "$auth.api").then(|| format!("{kind}-T"));
        assert_eq!(res.resolve(r#"Bearer {{$auth.token("api")}} {{$auth.idToken("api")}}"#, &refs).unwrap(), "Bearer token-T idToken-T");
        assert!(matches!(res.resolve(r#"{{$auth.token("nope")}}"#, &refs), Err(ResolveError::Reference(_))));
        assert!(matches!(res.resolve("{{$auth.token}}", &refs), Err(ResolveError::Dynamic(_))));
    }

    #[test]
    fn root_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        let (home, cols) = (t.join("home"), t.join("home/.config/respite"));
        let root = |f: &str, projects: &[PathBuf]| root_in(&t.join(f), Some(&home), Some(&cols), projects);
        fs::create_dir_all(cols.join("Shop/orders")).unwrap();
        fs::create_dir_all(home.join("repo/.git")).unwrap();
        fs::create_dir_all(home.join("repo/api/v1")).unwrap();
        fs::create_dir_all(home.join("plain/sub")).unwrap();
        fs::create_dir_all(home.join(".respite")).unwrap();
        fs::create_dir_all(home.join(".config/.git")).unwrap(); // a dotfiles repo must not swallow collections

        assert_eq!(root("home/.config/respite/Shop/orders/a.http", &[]), cols.join("Shop"));
        assert_eq!(root("home/.config/respite/respite-dev.json", &[]), cols);
        assert_eq!(root("home/repo/api/v1/a.http", &[]), home.join("repo"));
        // A sidebar project wins over `.git`, the deepest one first; the collections dir never does.
        let projects = [home.join("repo"), home.join("repo/api"), cols.clone(), home.join("plain")];
        assert_eq!(root("home/repo/api/v1/a.http", &projects), home.join("repo/api"));
        assert_eq!(root("home/plain/sub/a.http", &projects), home.join("plain"));
        assert_eq!(root("home/.config/respite/Shop/a.http", &projects), cols.join("Shop"));
        // No marker, no project: the file's folder; `~/.respite` doesn't make home a root.
        assert_eq!(root("home/plain/sub/a.http", &[]), home.join("plain/sub"));
    }

    #[test]
    fn env_chain_merge_order() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::create_dir_all(r.join("billing")).unwrap();
        fs::create_dir_all(r.join("secrets")).unwrap();
        fs::write(r.join("respite.json"), r#"{"apiVersion":"v1","host":"shared"}"#).unwrap();
        fs::write(r.join("respite-dev.json"), r#"{"host":"localhost","token":"pub","port":8080}"#).unwrap();
        fs::write(r.join("respite-prod.json"), "{}").unwrap();
        fs::write(r.join("respite-dev.private.json"), r#"{"token":"root-secret"}"#).unwrap();
        fs::write(r.join("respite-local-anya.private.json"), "{}").unwrap();
        fs::write(r.join("billing/respite-dev.json"), r#"{"host":"billing"}"#).unwrap();
        fs::write(r.join("billing/respite-dev.private.json"), "{ broken").unwrap();
        fs::write(r.join("http-client.env.json"), r#"{"dev":{"host":"jetbrains"}}"#).unwrap(); // not read
        let extra = r.join("secrets/respite-dev.private.json");
        fs::write(&extra, r#"{"stripe":"sk"}"#).unwrap();
        let unnamed = r.join("secrets/acme.json");
        fs::write(&unnamed, r#"{"stripe":"x"}"#).unwrap();

        let envs = Environments::load(&r.join("billing/invoices.http"), r, &[extra, unnamed]);
        assert_eq!(envs.errors.len(), 2); // broken JSON, extra file without an env name
        assert_eq!(envs.names(), ["dev", "local-anya", "prod"]);
        let v = envs.variables(Some("dev"));
        let get = |k: &str| (v[k].value.as_str(), v[k].secret);
        assert_eq!(get("host"), ("billing", false));
        assert_eq!(get("token"), ("root-secret", true));
        assert_eq!(get("apiVersion"), ("v1", false));
        assert_eq!(get("port"), ("8080", false));
        assert_eq!(get("stripe"), ("sk", true));
        assert_eq!(envs.variables(None)["host"].value, "shared");
    }

    #[test]
    fn respite_per_environment_files() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::create_dir(r.join("api")).unwrap();
        fs::write(r.join("respite.json"), r#"{"apiVersion":"v2"}"#).unwrap();
        fs::write(r.join("respite-dev.json"), r#"{"host":"respite-root"}"#).unwrap();
        fs::write(r.join("respite-local.json"), r#"{"host":"localhost"}"#).unwrap();
        fs::write(r.join("api/respite-dev.json"), r#"{"host":"api"}"#).unwrap();
        fs::write(r.join("api/respite-dev.private.json"), r#"{"token":"s3cret"}"#).unwrap();
        fs::write(r.join("respite-broken.json"), "{").unwrap();

        let envs = Environments::load(&r.join("api/users.http"), r, &[]);
        assert_eq!(envs.names(), ["dev", "local"]); // respite-broken.json is an error, not an environment
        assert_eq!(envs.errors.len(), 1);
        let v = envs.variables(Some("dev"));
        let get = |k: &str| (v[k].value.as_str(), v[k].secret);
        assert_eq!(get("host"), ("api", false)); // deeper dir wins
        assert_eq!(get("apiVersion"), ("v2", false)); // respite.json = $shared
        assert_eq!(get("token"), ("s3cret", true));
        assert_eq!(envs.variables(Some("local"))["host"].value, "localhost");

        assert_eq!(respite_env_file("respite.json"), Some(("$shared".into(), false)));
        assert_eq!(respite_env_file("respite.private.json"), Some(("$shared".into(), true)));
        assert_eq!(respite_env_file("respite-prod.private.json"), Some(("prod".into(), true)));
        assert_eq!(respite_env_file("respite-my-local.json"), Some(("my-local".into(), false)));
        assert_eq!(respite_env_file("respite.dev.json"), None); // dot form is not the format
        assert_eq!(respite_env_file("respite-.json"), None);
        assert_eq!(respite_env_file("respite.toml"), None);
        assert!(is_env_file(Path::new("x/respite-dev.json")));
        assert!(!is_env_file(Path::new("package.json")) && !is_env_file(Path::new("http-client.env.json")));
    }

    #[test]
    fn check_reports_undefined_against_all_envs() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("respite-dev.json"), r#"{"host":"h"}"#).unwrap();
        fs::write(root.path().join("respite-prod.json"), r#"{"token":"t"}"#).unwrap();
        let path = root.path().join("a.http");
        let envs = Environments::load(&path, root.path(), &[]);
        let file = http_syntax::parse(
            "# @name login\n# @prompt pass\nPOST https://{{host}}/{{pass}}\nX-T: {{token}} {{nope}} {{nope}}\n\n\
             {\"a\": \"{{login.response.body.*}}\",\n \"b\": \"{{ghost.response.body.*}}\", \"c\": \"{{$processEnv RESPITE_NOPE}}\"}\n",
        );
        let found: Vec<_> = check(&file, &envs, root.path()).into_iter().map(|d| (d.line, d.severity, d.code)).collect();
        assert_eq!(found, [
            (4, Severity::Error, "undefined_variable"),
            (7, Severity::Error, "unknown_request"),
            (7, Severity::Warning, "undefined_variable"),
        ]);
    }

    #[test]
    fn resolve_precedence_refs_and_cycles() {
        let file = http_syntax::parse("@host = file.example\n@base = https://{{host}}/{{v}}\n@a = {{b}}\n@b = {{a}}\nGET /\n");
        let env = HashMap::from([
            ("host".to_string(), Variable { value: "env".into(), source: "e".into(), secret: false }),
            ("v".to_string(), Variable { value: "v1".into(), source: "e".into(), secret: false }),
        ]);
        let overrides = HashMap::from([("v".to_string(), "v2".to_string())]);
        let r = Resolver { overrides: &overrides, file_vars: &file.variables, env: &env, dir: Path::new(".") };
        let refs = |name: &str, path: &str| (name == "login" && path == "response.body.$.token").then(|| "T".to_string());
        assert_eq!(r.resolve("{{base}}/x?t={{ login.response.body.$.token }}", &refs).unwrap(), "https://file.example/v2/x?t=T");
        assert_eq!(r.resolve("{{a}}", &refs), Err(ResolveError::Cycle("a".into())));
        assert_eq!(r.resolve("{{nope}}", &refs), Err(ResolveError::Undefined("nope".into())));
        assert_eq!(r.resolve("{{other.response.body.*}}", &refs), Err(ResolveError::Reference("other.response.body.*".into())));
        assert_eq!(r.resolve("{{unclosed", &refs).unwrap(), "{{unclosed");
    }
}
