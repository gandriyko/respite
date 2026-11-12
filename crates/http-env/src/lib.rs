//! Environments and `{{variable}}` resolution.
//!
//! Two env-file formats, mixable, searched in every directory from the workspace root
//! down to the `.http` file:
//! - JetBrains: `http-client.env.json` / `http-client.private.env.json`, one file with all
//!   environments (`{"$shared": {…}, "dev": {…}}`);
//! - Respite: one file per environment, `respite.json` (shared), `respite.<env>.json`,
//!   `respite.<env>.private.json` (flat `{"baseUrl": "…"}`).
//!
//! Within a directory: JetBrains public, JetBrains private, then Respite files (public before
//! private); deeper directories override shallower ones.

mod dynamic;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use http_syntax::{Diagnostic, FileVariable, HttpFile, Severity};
use serde_json::{Map, Value};

/// Per directory: the first existing name wins (`rest-client.*` = old JetBrains names).
const PUBLIC: [&str; 2] = ["http-client.env.json", "rest-client.env.json"];
const PRIVATE: [&str; 2] = ["http-client.private.env.json", "rest-client.private.env.json"];

/// Respite per-environment file name → (environment key, private). `respite.json` and
/// `respite.private.json` hold `$shared`.
pub fn respite_env_file(name: &str) -> Option<(String, bool)> {
    if name == "respite.json" {
        return Some(("$shared".into(), false));
    }
    let mid = name.strip_prefix("respite.")?.strip_suffix(".json")?;
    let (env, private) = match mid.strip_suffix(".private") {
        Some(env) => (env, true),
        None if mid == "private" => ("", true),
        None => (mid, false),
    };
    Some((if env.is_empty() { "$shared".into() } else { env.into() }, private))
}

/// Any env file of either format (for UIs listing / opening them as JSON).
pub fn is_env_file(path: &Path) -> bool {
    path.file_name().is_some_and(|n| {
        let n = n.to_string_lossy();
        n.ends_with(".env.json") || respite_env_file(&n).is_some()
    })
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
    /// then `extra` files (`envFiles` / `--env-file`), later ones overriding earlier ones.
    pub fn load(http_file: &Path, root: &Path, extra: &[PathBuf]) -> Self {
        let dir = http_file.parent().unwrap_or(root);
        let mut dirs: Vec<&Path> = dir.ancestors().take_while(|d| d.starts_with(root)).collect();
        dirs.reverse();
        // (path, Some(env key) for a Respite single-environment file)
        let mut paths: Vec<(PathBuf, Option<(String, bool)>)> = vec![];
        for d in dirs {
            for names in [PUBLIC, PRIVATE] {
                if let Some(p) = names.iter().map(|n| d.join(n)).find(|p| p.is_file()) {
                    paths.push((p, None));
                }
            }
            paths.extend(respite_files(d).into_iter().map(|(p, env, private)| (p, Some((env, private)))));
        }
        paths.extend(extra.iter().map(|p| (p.clone(), None)));

        let mut envs = Self::default();
        for (path, respite) in paths {
            let parsed = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<Map<String, Value>>(&s).map_err(|e| e.to_string()));
            match (parsed, respite) {
                // One environment per file: wrap it so merging treats both formats alike.
                (Ok(vars), Some((env, private))) => {
                    let json = Map::from_iter([(env, Value::Object(vars))]);
                    envs.files.push(EnvFile { path, private, json });
                }
                (Ok(json), None) => {
                    let private = path.file_name().is_some_and(|n| n.to_string_lossy().contains(".private."));
                    envs.files.push(EnvFile { path, private, json });
                }
                (Err(e), _) => envs.errors.push((path, e)),
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

    /// Merged variables: `$shared` from every file, then the `env` block from every file.
    /// Shallow merge by key; objects (e.g. `SSLConfiguration`) are not variables.
    // ponytail: SSLConfiguration deep merge comes with mTLS.
    pub fn variables(&self, env: Option<&str>) -> HashMap<String, Variable> {
        let mut out = HashMap::new();
        for key in std::iter::once("$shared").chain(env) {
            for f in &self.files {
                let Some(Value::Object(block)) = f.json.get(key) else { continue };
                for (k, v) in block {
                    let value = match v {
                        Value::String(s) => s.clone(),
                        Value::Object(_) | Value::Array(_) => continue,
                        other => other.to_string(),
                    };
                    out.insert(k.clone(), Variable { value, source: f.path.clone(), secret: f.private });
                }
            }
        }
        out
    }
}

/// Workspace root for a `.http` file: nearest ancestor with `.git` or `.respite`, else its directory.
pub fn workspace_root(file: &Path) -> PathBuf {
    let dir = file.parent().unwrap_or(Path::new("."));
    dir.ancestors()
        .find(|d| d.join(".git").exists() || d.join(".respite").exists())
        .unwrap_or(dir)
        .to_path_buf()
}

/// Undefined `{{variables}}`, unknown `{{name.response…}}` requests, cycles and bad `{{$…}}`,
/// checked against the union of all environments (a variable defined in any of them is fine).
pub fn check(file: &HttpFile, envs: &Environments, dir: &Path) -> Vec<Diagnostic> {
    let mut env = envs.variables(None);
    for name in envs.names() {
        env.extend(envs.variables(Some(&name)));
    }
    let names: HashSet<&str> = file.requests.iter().filter_map(|r| r.name.as_deref()).collect();
    let refs = |name: &str, _: &str| names.contains(name).then(String::new);
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
        let resolver = Resolver { overrides: &overrides, file_vars: &file.variables, env: &env, dir };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn env_chain_merge_order() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        fs::create_dir(r.join("billing")).unwrap();
        fs::write(r.join("http-client.env.json"),
            r#"{"$shared":{"apiVersion":"v1","host":"shared"},"dev":{"host":"localhost","token":"pub","port":8080},"prod":{}}"#).unwrap();
        fs::write(r.join("http-client.private.env.json"), r#"{"dev":{"token":"root-secret"},"local-anya":{}}"#).unwrap();
        fs::write(r.join("billing/http-client.env.json"), r#"{"dev":{"host":"billing"}}"#).unwrap();
        fs::write(r.join("billing/http-client.private.env.json"), "{ broken").unwrap();
        let extra = r.join("acme.private.env.json");
        fs::write(&extra, r#"{"dev":{"stripe":"sk"}}"#).unwrap();

        let envs = Environments::load(&r.join("billing/invoices.http"), r, &[extra]);
        assert_eq!(envs.errors.len(), 1);
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
        fs::write(r.join("http-client.env.json"), r#"{"dev":{"host":"jetbrains","port":"1"}}"#).unwrap();
        fs::write(r.join("respite.json"), r#"{"apiVersion":"v2"}"#).unwrap();
        fs::write(r.join("respite.dev.json"), r#"{"host":"respite-root"}"#).unwrap();
        fs::write(r.join("respite.local.json"), r#"{"host":"localhost"}"#).unwrap();
        fs::write(r.join("api/respite.dev.json"), r#"{"host":"api"}"#).unwrap();
        fs::write(r.join("api/respite.dev.private.json"), r#"{"token":"s3cret"}"#).unwrap();
        fs::write(r.join("respite.broken.json"), "{").unwrap();

        let envs = Environments::load(&r.join("api/users.http"), r, &[]);
        assert_eq!(envs.names(), ["dev", "local"]); // respite.broken.json is an error, not an environment
        assert_eq!(envs.errors.len(), 1);
        let v = envs.variables(Some("dev"));
        let get = |k: &str| (v[k].value.as_str(), v[k].secret);
        assert_eq!(get("host"), ("api", false)); // deeper dir wins, Respite over JetBrains
        assert_eq!(get("port"), ("1", false)); // JetBrains keys still merge in
        assert_eq!(get("apiVersion"), ("v2", false)); // respite.json = $shared
        assert_eq!(get("token"), ("s3cret", true));
        assert_eq!(envs.variables(Some("local"))["host"].value, "localhost");

        assert_eq!(respite_env_file("respite.json"), Some(("$shared".into(), false)));
        assert_eq!(respite_env_file("respite.private.json"), Some(("$shared".into(), true)));
        assert_eq!(respite_env_file("respite.prod.private.json"), Some(("prod".into(), true)));
        assert_eq!(respite_env_file("respite.toml"), None);
        assert!(is_env_file(Path::new("x/respite.dev.json")) && is_env_file(Path::new("http-client.env.json")));
        assert!(!is_env_file(Path::new("package.json")));
    }

    #[test]
    fn check_reports_undefined_against_all_envs() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("http-client.env.json"), r#"{"dev":{"host":"h"},"prod":{"token":"t"}}"#).unwrap();
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
