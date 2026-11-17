//! Environments and `{{variable}}` resolution.
//!
//! Env files, one per environment (flat `{"baseUrl": "…"}`): `respite.json` (shared by all)
//! and `respite-<env>.json`, secrets included (masked by name, [`is_secret_name`]). They all
//! live in one folder, [`env_dir`]. A `.http` file picks its environment with `# @env NAME` on
//! its first line.

mod dynamic;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use http_syntax::{Diagnostic, FileVariable, HttpFile, Severity};
use serde_json::{Map, Value};

/// Respite env file name → environment key: `respite-<env>.json`; `respite.json` holds
/// `$shared`. The old `*.private.json` split is not a name any more.
pub fn respite_env_file(name: &str) -> Option<String> {
    if name == "respite.json" {
        return Some("$shared".into());
    }
    let env = name.strip_prefix("respite-")?.strip_suffix(".json")?;
    (!env.is_empty() && !env.ends_with(".private")).then(|| env.to_string())
}

/// An env file (for UIs listing / opening them as JSON).
pub fn is_env_file(path: &Path) -> bool {
    path.file_name().is_some_and(|n| respite_env_file(&n.to_string_lossy()).is_some())
}

/// A name that says its value is a secret (`password`, `apiKey`, `clientSecret`, …): masked in
/// output and history.
pub fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["pass", "pwd", "secret", "token", "key", "credential"].iter().any(|s| lower.contains(s))
}

/// `respite*.json` files in `dir`, by name: env files, and the ones whose name isn't one.
fn respite_files(dir: &Path) -> Vec<(PathBuf, Option<String>)> {
    let mut found: Vec<(PathBuf, Option<String>)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name.starts_with("respite") && name.ends_with(".json") && e.path().is_file()).then(|| (e.path(), respite_env_file(&name)))
        })
        .collect();
    found.sort();
    found
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub value: String,
    pub source: PathBuf,
    /// Its name says it's a secret ([`is_secret_name`]): mask it in output and history.
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
    json: Map<String, Value>,
}

impl Environments {
    /// The env files of [`env_dir`], then `extra` files (`--env-file`, named like the others).
    pub fn load(extra: &[PathBuf]) -> Self {
        Self::load_from(env_dir().as_deref(), extra)
    }

    /// Env files in `dir`, then `extra` ones, later ones overriding earlier ones.
    pub fn load_from(dir: Option<&Path>, extra: &[PathBuf]) -> Self {
        let mut paths: Vec<(PathBuf, Option<String>)> = dir.into_iter().flat_map(respite_files).collect();
        paths.extend(extra.iter().map(|p| (p.clone(), p.file_name().and_then(|n| respite_env_file(&n.to_string_lossy())))));

        let mut envs = Self::default();
        for (path, key) in paths {
            let Some(env) = key else {
                let private = path.to_string_lossy().ends_with(".private.json");
                let e = if private {
                    "private env files are gone: move its values into respite-<env>.json (secrets are masked by name)"
                } else {
                    "not an env file name: respite.json or respite-<env>.json"
                };
                envs.errors.push((path, e.into()));
                continue;
            };
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<Map<String, Value>>(&s).map_err(|e| e.to_string()));
            match parsed {
                // Keyed by environment, so merging and listing work across files.
                Ok(vars) => envs.files.push(EnvFile { path, json: Map::from_iter([(env, Value::Object(vars))]) }),
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
    /// across files (`respite.json`, then the environment's). Each config comes back as
    /// `$auth.<id>` holding its JSON — a name no `{{variable}}` can reach (`{{$…}}` is dynamic) —
    /// and counts as secret.
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
                                let e = auth.entry(id.clone()).or_insert_with(|| (Map::new(), Variable { value: String::new(), source: f.path.clone(), secret: true }));
                                e.0.extend(cfg.clone());
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
                    out.insert(k.clone(), Variable { value, source: f.path.clone(), secret: is_secret_name(k) });
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

/// `~/.config/respite/collections`: the central folder of `.http` files, in subfolders as the
/// user likes. Its siblings: `settings.json`, the keymap and [`env_dir`].
pub fn collections_dir() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".config/respite/collections"))
}

/// `~/.config/respite/env`: every environment, for every `.http` file wherever it lives.
pub fn env_dir() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".config/respite/env"))
}

/// Undefined `{{variables}}`, unknown `{{name.response…}}` requests, cycles and bad `{{$…}}`,
/// checked against the union of all environments (a variable defined in any of them is fine).
/// Also a `# @env` naming no environment.
pub fn check(file: &HttpFile, envs: &Environments, dir: &Path) -> Vec<Diagnostic> {
    let mut out = check_vars(file, &envs.all_variables(), dir);
    if let Some(env) = &file.env
        && !envs.names().contains(env)
    {
        let message = format!("no environment '{env}': add respite-{env}.json to the env folder");
        out.insert(0, Diagnostic { line: 1, severity: Severity::Error, code: "unknown_environment", message });
    }
    out
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
        // Merged field by field: `respite.json` has the config, the environment adds the secret.
        fs::write(r.join("respite.json"), auth(r#"{"Type": "OAuth2", "Client ID": "c", "Client Secret": "{{secret}}"}"#)).unwrap();
        fs::write(r.join("respite-dev.json"), auth(r#"{"Client Secret": "s3"}"#)).unwrap();
        let envs = Environments::load_from(Some(r), &[]);
        let v = envs.variables(Some("dev"));
        let cfg: Value = serde_json::from_str(&v["$auth.api"].value).unwrap();
        assert_eq!((cfg["Client ID"].as_str(), cfg["Client Secret"].as_str(), v["$auth.api"].secret), (Some("c"), Some("s3"), true));
        assert!(!v.contains_key("Security"));

        // SSLConfiguration: merged field by field, paths absolute against their own file.
        fs::create_dir_all(r.join("api")).unwrap();
        fs::write(r.join("respite.json"), r#"{"SSLConfiguration": {"caCertificate": "certs/ca.pem", "verifyHostCertificate": true}}"#).unwrap();
        let extra = r.join("api/respite-dev.json");
        fs::write(&extra, r#"{"SSLConfiguration": {"clientCertificate": {"path": "c.der", "format": "DER"}, "clientCertificateKey": "/abs/k.pem"}}"#)
            .unwrap();
        let v = Environments::load_from(Some(r), &[extra]).variables(Some("dev"));
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
    fn env_dir_merge_order() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::create_dir_all(r.join("secrets")).unwrap();
        fs::write(r.join("respite.json"), r#"{"apiVersion":"v1","host":"shared"}"#).unwrap();
        fs::write(r.join("respite-dev.json"), r#"{"host":"localhost","token":"root-secret","port":8080}"#).unwrap();
        fs::write(r.join("respite-prod.json"), "{}").unwrap();
        fs::write(r.join("respite-local-anya.json"), "{}").unwrap();
        fs::write(r.join("respite-dev.private.json"), r#"{"token":"old"}"#).unwrap(); // the old split: an error, not read
        fs::write(r.join("respite-broken.json"), "{ broken").unwrap();
        fs::write(r.join("http-client.env.json"), r#"{"dev":{"host":"jetbrains"}}"#).unwrap(); // not read
        let extra = r.join("secrets/respite-dev.json");
        fs::write(&extra, r#"{"apiKey":"sk","host":"ci"}"#).unwrap();
        let unnamed = r.join("secrets/acme.json");
        fs::write(&unnamed, r#"{"stripe":"x"}"#).unwrap();

        let envs = Environments::load_from(Some(r), &[extra, unnamed]);
        // Broken JSON, the private file, an extra file without an env name.
        assert_eq!(envs.errors.len(), 3, "{:?}", envs.errors);
        assert!(envs.errors.iter().any(|(p, e)| p.ends_with("respite-dev.private.json") && e.contains("respite-<env>.json")));
        assert_eq!(envs.names(), ["dev", "local-anya", "prod"]);
        let v = envs.variables(Some("dev"));
        let get = |k: &str| (v[k].value.as_str(), v[k].secret);
        assert_eq!(get("host"), ("ci", false)); // --env-file wins
        assert_eq!(get("token"), ("root-secret", true)); // secret by name
        assert_eq!(get("apiVersion"), ("v1", false)); // respite.json = $shared
        assert_eq!(get("port"), ("8080", false));
        assert_eq!(get("apiKey"), ("sk", true));
        assert_eq!(envs.variables(None)["host"].value, "shared");
        assert!(Environments::load_from(None, &[]).names().is_empty());
    }

    #[test]
    fn env_file_names() {

        assert_eq!(respite_env_file("respite.json"), Some("$shared".into()));
        assert_eq!(respite_env_file("respite-my-local.json"), Some("my-local".into()));
        assert_eq!([respite_env_file("respite.private.json"), respite_env_file("respite-prod.private.json")], [None, None]);
        assert!(is_secret_name("password") && is_secret_name("apiKey") && is_secret_name("Client Secret") && is_secret_name("access_token"));
        assert!(!is_secret_name("baseUrl") && !is_secret_name("user"));
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
        let envs = Environments::load_from(Some(root.path()), &[]);
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
