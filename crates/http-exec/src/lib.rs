//! Request executor: `{{variables}}` → HTTP (reqwest) → response.
//! Requests referenced as `{{name.response…}}` are run first when no cached response exists.
//!
//! Async but runtime-agnostic in API; reqwest needs a tokio runtime to drive it.
//! Cancel by dropping the future.

mod oauth;
mod store;
mod tls;

pub use oauth::{AuthUi, Token, open_in_browser};
pub use store::{Entry, Store};

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use http_env::{ResolveError, Resolver, Variable};
use http_syntax::{Assert, HttpFile, Request, Subject};
use serde_json::{Value, json};

/// What to run against: a parsed `.http` file plus resolved variables.
pub struct Context<'a> {
    pub path: &'a Path,
    pub file: &'a HttpFile,
    pub env: &'a HashMap<String, Variable>,
    /// `@prompt` answers and `--var` values; highest priority.
    pub overrides: &'a HashMap<String, String>,
}

/// The request as actually sent, after substitution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub http_version: String,
    pub headers: Vec<(String, String)>,
    /// Decompressed body.
    pub body: Vec<u8>,
    /// Until status line + headers arrived.
    pub ttfb: Duration,
    pub total: Duration,
    /// Where the response came from (after redirects); empty in old history entries.
    pub url: String,
    /// Redirects followed, in order.
    pub redirects: Vec<Redirect>,
    /// Cookies the jar sent with the first request (name, value).
    pub cookies_sent: Vec<(String, String)>,
}

/// One followed redirect: `status` from `from` sent the client to `to`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Redirect {
    pub status: u16,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone)]
pub struct Exchange {
    pub request: SentRequest,
    pub response: Response,
    /// Named requests auto-run first to satisfy `{{name.response…}}`, in run order.
    pub dependencies: Vec<String>,
    /// Set when the exchange was written to history.
    pub history_id: Option<String>,
    /// `# @assert` results, in file order; empty for history entries.
    pub tests: Vec<TestResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    pub line: usize,
    /// The assertion as written.
    pub text: String,
    pub passed: bool,
    /// On failure: the actual value, or why it could not be checked.
    pub detail: String,
}

#[derive(Debug)]
pub enum Error {
    Resolve(ResolveError),
    File(PathBuf, std::io::Error),
    Http(reqwest::Error),
    BadRequest(String),
    DependencyCycle(String),
    /// OAuth 2.0 config `id` is missing or incomplete, or its token request failed.
    Auth { id: String, message: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(e) => e.fmt(f),
            Self::File(p, e) => write!(f, "{}: {e}", p.display()),
            // reqwest's top line hides the cause ("error sending request"): add the chain.
            Self::Http(e) => {
                write!(f, "{e}")?;
                let mut src = std::error::Error::source(e);
                while let Some(c) = src {
                    write!(f, ": {c}")?;
                    src = c.source();
                }
                Ok(())
            }
            Self::BadRequest(m) => f.write_str(m),
            Self::DependencyCycle(n) => write!(f, "request dependency cycle through '{n}'"),
            Self::Auth { id, message } => write!(f, "auth '{id}': {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ResolveError> for Error {
    fn from(e: ResolveError) -> Self {
        Self::Resolve(e)
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e)
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Defaults for requests without the matching directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Used when there is no `# @timeout`.
    pub timeout: Option<Duration>,
    /// `# @no-redirect` still wins.
    pub follow_redirects: bool,
    pub max_redirects: usize,
    /// Skip TLS certificate checks for every request, as if each had `# @insecure`.
    pub insecure: bool,
    /// Proxy URL for every request; `None`: the system proxy (macOS settings, `HTTPS_PROXY`…).
    pub proxy: Option<String>,
    /// Hosts that bypass `proxy` (`NO_PROXY` syntax: `localhost`, `.internal`, `10.0.0.0/8`).
    pub no_proxy: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self { timeout: None, follow_redirects: true, max_redirects: 10, insecure: false, proxy: None, no_proxy: vec![] }
    }
}

/// What every client of a run shares — the request's and the token endpoint's: proxy, client
/// certificate, extra CAs, certificate checks.
fn client_builder(options: &Options, tls: &tls::Tls, insecure: bool) -> Result<reqwest::ClientBuilder, Error> {
    let mut b = reqwest::Client::builder().danger_accept_invalid_certs(insecure || options.insecure || !tls.verify);
    if let Some(id) = &tls.identity {
        b = b.identity(id.clone());
    }
    for ca in &tls.roots {
        b = b.add_root_certificate(ca.clone());
    }
    // An explicit proxy replaces the system one (reqwest drops it once one is added).
    if let Some(url) = &options.proxy {
        let proxy = reqwest::Proxy::all(url).map_err(|e| Error::BadRequest(format!("invalid proxy '{url}': {e}")))?;
        b = b.proxy(proxy.no_proxy(reqwest::NoProxy::from_string(&options.no_proxy.join(","))));
    }
    Ok(b)
}

/// Long-lived: owns the cookie jar and the named-response cache. Share via `Arc`.
pub struct Executor {
    options: std::sync::RwLock<Options>,
    cookies: Arc<reqwest_cookie_store::CookieStoreMutex>,
    /// In-memory layer over `store.latest()`.
    named: Mutex<HashMap<(PathBuf, String), Arc<Exchange>>>,
    /// OAuth 2.0 tokens by config id, for this executor's environment.
    tokens: Mutex<HashMap<String, Token>>,
    /// Where browser and device sign-ins talk to the user.
    auth_ui: std::sync::RwLock<AuthUi>,
    store: Option<Store>,
    env: Option<String>,
    source: &'static str,
}

impl Default for Executor {
    /// No history, in-memory cookies.
    fn default() -> Self {
        Self::new(None, None, "test")
    }
}

struct Prepared {
    sent: SentRequest,
    output: Option<(PathBuf, bool)>,
    /// `# @assert` lines with `{{vars}}` in the expected value resolved.
    asserts: Vec<(usize, String, Result<Assert, String>)>,
    tls: tls::Tls,
}

impl Executor {
    /// With a `store`: history, cross-run named-response cache and cookies persisted per `env`.
    /// `source` tags history entries (`gui` / `cli` / `mcp`).
    pub fn new(store: Option<Store>, env: Option<&str>, source: &'static str) -> Self {
        let jar = store
            .as_ref()
            .and_then(|s| std::fs::File::open(s.cookies_path(env)).ok())
            .and_then(|f| cookie_store::serde::json::load(std::io::BufReader::new(f)).ok())
            .unwrap_or_default();
        let tokens = store.as_ref().map(|s| oauth::load(&s.oauth_path(env))).unwrap_or_default();
        Self {
            options: Default::default(),
            cookies: Arc::new(reqwest_cookie_store::CookieStoreMutex::new(jar)),
            named: Mutex::default(),
            tokens: Mutex::new(tokens),
            auth_ui: Default::default(),
            store,
            env: env.map(Into::into),
            source,
        }
    }

    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// Takes effect for requests started afterwards.
    pub fn set_options(&self, options: Options) {
        *self.options.write().unwrap() = options;
    }

    /// Run `cx.file.requests[index]`.
    pub async fn run(&self, cx: &Context<'_>, index: usize) -> Result<Exchange, Error> {
        self.run_inner(cx, index, &mut vec![], &mut vec![]).await
    }

    /// Cached exchange of named request `name` in `path`: memory, then history on disk.
    fn cached(&self, path: &Path, name: &str) -> Option<Arc<Exchange>> {
        let key = (path.to_path_buf(), name.to_string());
        let mut named = self.named.lock().unwrap();
        if let Some(ex) = named.get(&key) {
            return Some(ex.clone());
        }
        let ex = Arc::new(self.store.as_ref()?.latest(path, name)?);
        named.insert(key, ex.clone());
        Some(ex)
    }

    // Persistence failures must not fail the request; report and go on.
    fn persist(&self, cx: &Context<'_>, req: &Request, ex: &mut Exchange) {
        let Some(store) = &self.store else { return };
        if req.directive("no-log").is_none() {
            // Short values would mask unrelated text.
            let tokens = self.tokens.lock().unwrap();
            let secrets: Vec<&str> = cx
                .env
                .values()
                .filter(|v| v.secret && v.value.len() >= 4)
                .map(|v| v.value.as_str())
                .chain(tokens.values().flat_map(Token::secrets))
                .collect();
            match store.record(self.source, cx.path, req, ex, &secrets) {
                Ok(id) => ex.history_id = Some(id),
                Err(e) => eprintln!("respite: history not saved: {e}"),
            }
        }
        let saved = std::fs::File::create(store.cookies_path(self.env.as_deref())).map_err(|e| e.to_string()).and_then(|mut f| {
            cookie_store::serde::json::save(&self.cookies.lock().unwrap(), &mut f).map_err(|e| e.to_string())
        });
        if let Err(e) = saved {
            eprintln!("respite: cookies not saved: {e}");
        }
    }

    /// Build the request without sending it (`--dry-run`, "Copy as cURL").
    /// Fails with a `Reference` error if it needs a response that is not cached yet.
    pub fn prepare(&self, cx: &Context<'_>, index: usize) -> Result<SentRequest, Error> {
        self.prepare_inner(cx, &cx.file.requests[index], &RefCell::default()).map(|p| p.sent)
    }

    fn run_inner<'a>(
        &'a self,
        cx: &'a Context<'a>,
        index: usize,
        stack: &'a mut Vec<usize>,
        ran: &'a mut Vec<String>,
    ) -> BoxFuture<'a, Result<Exchange, Error>> {
        Box::pin(async move {
            let req = &cx.file.requests[index];
            let first_dep = ran.len();
            stack.push(index);
            let mut fetched: Vec<String> = vec![];
            let prepared = loop {
                let misses = RefCell::new(vec![]);
                match self.prepare_inner(cx, req, &misses) {
                    Ok(p) => break p,
                    Err(Error::Resolve(ResolveError::Reference(_))) if !misses.borrow().is_empty() => {
                        for name in misses.take() {
                            if let Some(id) = name.strip_prefix("$auth.") {
                                if fetched.iter().any(|f| f == id) {
                                    let message = "the token response has no id_token".into();
                                    return Err(Error::Auth { id: id.into(), message });
                                }
                                self.auto_token(cx, id).await?;
                                fetched.push(id.into());
                                continue;
                            }
                            let dep = cx
                                .file
                                .requests
                                .iter()
                                .position(|r| r.name.as_deref() == Some(name.as_str()))
                                .ok_or_else(|| Error::BadRequest(format!("no request named '{name}' in this file")))?;
                            if stack.contains(&dep) {
                                return Err(Error::DependencyCycle(name));
                            }
                            self.run_inner(cx, dep, stack, ran).await?;
                            ran.push(name);
                        }
                    }
                    Err(e) => return Err(e),
                }
            };
            stack.pop();

            let options = self.options.read().unwrap().clone();
            let response = send(req, &prepared.sent, self.cookies.clone(), &options, &prepared.tls).await?;
            if let Some((path, overwrite)) = &prepared.output {
                write_output(path, *overwrite, &response.body)?;
            }
            let tests = prepared.asserts.iter().map(|(line, text, a)| check(*line, text, a, &response)).collect();
            let mut ex = Exchange {
                request: prepared.sent,
                response,
                dependencies: ran[first_dep..].to_vec(),
                history_id: None,
                tests,
            };
            self.persist(cx, req, &mut ex);
            if let Some(name) = &req.name {
                let key = (cx.path.to_path_buf(), name.clone());
                self.named.lock().unwrap().insert(key, Arc::new(ex.clone()));
            }
            Ok(ex)
        })
    }

    /// Token of OAuth 2.0 config `id`: the cached one while fresh, else refreshed or acquired
    /// (`force`: always the full grant). Saved to the token cache.
    pub async fn auth_token(&self, cx: &Context<'_>, id: &str, force: bool) -> Result<Token, Error> {
        let cfg = self.auth_config(cx, id)?;
        let previous = self.tokens.lock().unwrap().get(id).cloned();
        if !force && let Some(t) = previous.as_ref().filter(|t| t.config == cfg.hash && t.fresh(oauth::now())) {
            return Ok(t.clone());
        }
        let options = self.options.read().unwrap().clone();
        let builder = client_builder(&options, &tls::load(cx.env)?, false)?.timeout(options.timeout.unwrap_or(Duration::from_secs(30)));
        let ui = self.auth_ui.read().unwrap().clone();
        let token = oauth::acquire(&builder.build()?, &cfg, previous.as_ref().filter(|_| !force), &ui).await.map_err(|e| match e {
            oauth::Error::Http(h) => Error::Http(h),
            oauth::Error::Config(message) | oauth::Error::Server(message) | oauth::Error::Pending(message) => Error::Auth { id: id.into(), message },
        })?;
        let mut tokens = self.tokens.lock().unwrap();
        tokens.insert(id.into(), token.clone());
        if let Some(store) = &self.store
            && let Err(e) = oauth::save(&store.oauth_path(self.env.as_deref()), &tokens)
        {
            eprintln!("respite: OAuth tokens not saved: {e}");
        }
        Ok(token)
    }

    /// How sign-ins that need the user (browser, device code) reach them.
    pub fn set_auth_ui(&self, ui: AuthUi) {
        *self.auth_ui.write().unwrap() = ui;
    }

    /// Forget every token of this environment, on disk too.
    pub fn clear_tokens(&self) {
        self.tokens.lock().unwrap().clear();
        if let Some(store) = &self.store {
            let _ = std::fs::remove_file(store.oauth_path(self.env.as_deref()));
        }
    }

    /// Cached token of `id`, fresh or not (hover, `respite auth`).
    pub fn cached_token(&self, id: &str) -> Option<Token> {
        self.tokens.lock().unwrap().get(id).cloned()
    }

    /// A token for a request: like [`Self::auth_token`], unless the config says
    /// `"Acquire Automatically": false`.
    async fn auto_token(&self, cx: &Context<'_>, id: &str) -> Result<Token, Error> {
        if !self.auth_config(cx, id)?.automatic() {
            let message = format!("\"Acquire Automatically\" is false; get a token with `respite auth token {id}`");
            return Err(Error::Auth { id: id.into(), message });
        }
        self.auth_token(cx, id, false).await
    }

    /// `Security.Auth.<id>` from the env files with `{{variables}}` resolved.
    fn auth_config(&self, cx: &Context<'_>, id: &str) -> Result<oauth::Config, Error> {
        let missing = || Error::Auth { id: id.into(), message: "no such config in Security.Auth of the env files".into() };
        let raw: Value = serde_json::from_str(&cx.env.get(&format!("$auth.{id}")).ok_or_else(missing)?.value).map_err(|_| missing())?;
        let dir = cx.path.parent().unwrap_or(Path::new("."));
        let resolver = Resolver { overrides: cx.overrides, file_vars: &cx.file.variables, env: cx.env, dir };
        match oauth::resolve_strings(&raw, &|s: &str| resolver.resolve(s, &|_, _| None))? {
            Value::Object(map) => Ok(oauth::Config::new(map)),
            _ => Err(missing()),
        }
    }

    /// `misses` collects named requests (and `$auth.<id>` configs) referenced but not cached yet.
    fn prepare_inner(&self, cx: &Context<'_>, req: &Request, misses: &RefCell<Vec<String>>) -> Result<Prepared, Error> {
        let dir = cx.path.parent().unwrap_or(Path::new("."));
        let resolver = Resolver { overrides: cx.overrides, file_vars: &cx.file.variables, env: cx.env, dir };
        let miss = |name: &str| {
            let mut m = misses.borrow_mut();
            if !m.iter().any(|n| n == name) {
                m.push(name.to_string());
            }
        };
        let refs = |name: &str, path: &str| {
            if let Some(id) = name.strip_prefix("$auth.") {
                let value = self.auth_config(cx, id).ok().and_then(|cfg| {
                    let tokens = self.tokens.lock().unwrap();
                    let t = tokens.get(id).filter(|t| t.config == cfg.hash && t.fresh(oauth::now()))?;
                    t.get(path).map(String::from)
                });
                if value.is_none() {
                    miss(name);
                }
                return value;
            }
            match self.cached(cx.path, name) {
                Some(ex) => extract(&ex, path),
                None => {
                    miss(name);
                    None
                }
            }
        };
        let r = |s: &str| resolver.resolve(s, &refs);

        let mut method = req.method.clone();
        let mut url = r(&req.url)?;
        // An undefined variable sent as written is fine in a header or body, never in the URL:
        // say which one instead of letting the URL parser fail on `{{`.
        if let Some(open) = url.find("{{")
            && let Some(len) = url[open..].find("}}")
        {
            let var = &url[open..open + len + 2];
            return Err(Error::BadRequest(format!(
                "the URL has {var}, which is not defined: add it to the file or the selected environment (an env file with invalid JSON is skipped)"
            )));
        }
        let mut headers =
            req.headers.iter().map(|h| Ok((r(&h.name)?, r(&h.value)?))).collect::<Result<Vec<_>, ResolveError>>()?;

        if url.starts_with('/') {
            if let Some(host) = header(&headers, "host") {
                url = format!("http://{host}{url}");
            }
        } else if !url.contains("://") {
            url = format!("http://{url}");
        }

        for (name, value) in &mut headers {
            if name.eq_ignore_ascii_case("authorization")
                && let Some(cred) = value.strip_prefix("Basic ")
            {
                let raw = match cred.split_whitespace().collect::<Vec<_>>()[..] {
                    [user, pass] => Some(format!("{user}:{pass}")),
                    [user_pass] if user_pass.contains(':') => Some(user_pass.to_string()),
                    _ => None, // already base64
                };
                if let Some(raw) = raw {
                    *value = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw));
                }
            }
        }

        let content_type = header(&headers, "content-type").unwrap_or_default().to_ascii_lowercase();
        let mut body = match &req.body {
            Some(b) => build_body(&b.text, &content_type, dir, &r)?,
            None => vec![],
        };

        let gql = headers
            .iter()
            .position(|(n, v)| n.eq_ignore_ascii_case("x-request-type") && v.eq_ignore_ascii_case("graphql"));
        if method == "GRAPHQL" || gql.is_some() {
            if let Some(i) = gql {
                headers.remove(i);
            }
            if method == "GRAPHQL" {
                method = "POST".into();
            }
            let text = String::from_utf8_lossy(&body).into_owned();
            let (query, vars) = text.split_once("\n\n").unwrap_or((&text, ""));
            let variables: Value = if vars.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(vars).map_err(|e| Error::BadRequest(format!("GraphQL variables: {e}")))?
            };
            body = serde_json::to_vec(&json!({ "query": query.trim(), "variables": variables })).unwrap();
            if content_type.is_empty() {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
        }

        let output = match &req.output {
            Some(o) => Some((dir.join(r(&o.path)?), o.overwrite)),
            None => None,
        };
        let mut asserts = vec![];
        for (line, text, a) in req.asserts() {
            let a = match a {
                Ok(a) => Ok(Assert { expected: r(&a.expected)?, ..a }),
                e => e,
            };
            asserts.push((line, text.to_string(), a));
        }
        Ok(Prepared { sent: SentRequest { method, url, headers, body }, output, asserts, tls: tls::load(cx.env)? })
    }
}

fn header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Body lines: `< file` (raw bytes), `<@ file` (with variables), else text with variables.
/// Multipart joins lines with CRLF; form-urlencoded joins continuation lines (`&a=b`) directly.
fn build_body(
    text: &str,
    content_type: &str,
    dir: &Path,
    r: &dyn Fn(&str) -> Result<String, ResolveError>,
) -> Result<Vec<u8>, Error> {
    if content_type.starts_with("application/x-www-form-urlencoded") {
        return Ok(r(&text.lines().map(str::trim).collect::<String>())?.into_bytes());
    }
    let sep: &[u8] = if content_type.starts_with("multipart/") { b"\r\n" } else { b"\n" };
    let read = |p: &str| -> Result<(PathBuf, Vec<u8>), Error> {
        let path = dir.join(r(p.trim())?);
        std::fs::read(&path).map(|b| (path.clone(), b)).map_err(|e| Error::File(path, e))
    };
    let mut out = vec![];
    for (i, line) in text.lines().enumerate() {
        if i > 0 {
            out.extend_from_slice(sep);
        }
        let t = line.trim();
        if let Some(p) = t.strip_prefix("<@") {
            let (path, bytes) = read(p)?;
            let s = String::from_utf8(bytes).map_err(|e| Error::BadRequest(format!("{}: {e}", path.display())))?;
            out.extend(r(&s)?.into_bytes());
        } else if let Some(p) = t.strip_prefix("< ") {
            out.extend(read(p)?.1);
        } else {
            out.extend(r(line)?.into_bytes());
        }
    }
    Ok(out)
}

/// `response.body.$.a.b`, `response.body.*`, `response.headers.X-Token`, same for `request.`.
fn extract(ex: &Exchange, path: &str) -> Option<String> {
    let (kind, rest) = path.split_once('.')?;
    let (part, selector) = rest.split_once('.')?;
    let (headers, body) = match kind {
        "response" => (&ex.response.headers, &ex.response.body),
        "request" => (&ex.request.headers, &ex.request.body),
        _ => return None,
    };
    match part {
        "headers" => header(headers, selector).map(str::to_string),
        "body" if selector == "*" => Some(String::from_utf8_lossy(body).into_owned()),
        "body" if selector.starts_with('$') => {
            let json: Value = serde_json::from_slice(body).ok()?;
            let path = serde_json_path::JsonPath::parse(selector).ok()?;
            Some(match path.query(&json).first()? {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            })
        }
        // ponytail: XPath for XML bodies (sxd-xpath) when someone needs it.
        _ => None,
    }
}

/// `10 s`, `500ms`, `2m`; bare numbers in `default_unit_ms`.
fn millis(v: &str, default_unit_ms: u64) -> Option<u64> {
    let end = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let n: u64 = v[..end].parse().ok()?;
    Some(match v[end..].trim() {
        "" => n * default_unit_ms,
        "ms" => n,
        "s" => n * 1000,
        "m" => n * 60_000,
        _ => return None,
    })
}

/// `# @timeout 10 s` — units ms / s / m, seconds by default.
fn duration(req: &Request, directive: &str) -> Result<Option<Duration>, Error> {
    let Some(d) = req.directive(directive) else { return Ok(None) };
    let v = d.value.as_deref().unwrap_or("");
    let ms = millis(v, 1000).ok_or_else(|| Error::BadRequest(format!("invalid @{directive} '{v}'")))?;
    Ok(Some(Duration::from_millis(ms)))
}

fn check(line: usize, text: &str, a: &Result<Assert, String>, res: &Response) -> TestResult {
    let outcome = a.clone().and_then(|a| {
        let v = actual(&a.subject, res)?;
        let passed = compare(v.as_ref(), &a.op, &a.expected, a.subject == Subject::Duration)?;
        Ok((passed, v))
    });
    let (passed, detail) = match outcome {
        Ok((true, _)) => (true, String::new()),
        Ok((false, v)) => (false, format!("actual: {}", v.as_ref().map_or("(missing)".into(), show))),
        Err(e) => (false, e),
    };
    TestResult { line, text: text.into(), passed, detail }
}

fn actual(subject: &Subject, res: &Response) -> Result<Option<Value>, String> {
    Ok(match subject {
        Subject::Status => Some(json!(res.status)),
        Subject::Duration => Some(json!(res.total.as_millis() as u64)),
        Subject::Header(n) => header(&res.headers, n).map(|v| json!(v)),
        Subject::Body(None) => Some(Value::String(String::from_utf8_lossy(&res.body).into_owned())),
        Subject::Body(Some(p)) => {
            let full = if p.starts_with('$') { p.clone() } else { format!("$.{p}") };
            let path = serde_json_path::JsonPath::parse(&full).map_err(|e| format!("invalid JSONPath '{p}': {e}"))?;
            let json: Value = serde_json::from_slice(&res.body).map_err(|_| "response body is not JSON".to_string())?;
            path.query(&json).first().cloned()
        }
    })
}

fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// httpyac semantics: `exists` / `isTrue` / `isFalse` on presence and truthiness, numbers compared
/// as numbers when both sides parse, everything else on the value's text.
fn compare(v: Option<&Value>, op: &str, expected: &str, is_duration: bool) -> Result<bool, String> {
    let truthy = |v: &Value| match v {
        Value::Null | Value::Bool(false) => false,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    };
    match op {
        "exists" => return Ok(v.is_some_and(|v| !v.is_null())),
        "isTrue" => return Ok(v.is_some_and(truthy)),
        "isFalse" => return Ok(!v.is_some_and(truthy)),
        _ => {}
    }
    let Some(v) = v else { return Ok(false) };
    let text = show(v);
    let expected = expected.strip_prefix('"').and_then(|e| e.strip_suffix('"')).unwrap_or(expected);
    let want = if is_duration { millis(expected, 1).map(|n| n as f64) } else { expected.parse::<f64>().ok() };
    let got = text.parse::<f64>().ok();
    let ord = || match (got, want) {
        (Some(g), Some(w)) => Ok(g.partial_cmp(&w)),
        _ => Err(format!("'{op}' compares numbers; actual: {text}")),
    };
    use std::cmp::Ordering::*;
    Ok(match op {
        "isNumber" => v.is_number(),
        "isString" => v.is_string(),
        "isBoolean" => v.is_boolean(),
        "isArray" => v.is_array(),
        "==" | "equals" => got.zip(want).map_or(text == expected, |(g, w)| g == w),
        "!=" => got.zip(want).map_or(text != expected, |(g, w)| g != w),
        "<" => ord()? == Some(Less),
        "<=" => matches!(ord()?, Some(Less | Equal)),
        ">" => ord()? == Some(Greater),
        ">=" => matches!(ord()?, Some(Greater | Equal)),
        "contains" | "includes" => text.contains(expected),
        "startsWith" => text.starts_with(expected),
        "endsWith" => text.ends_with(expected),
        "matches" => regex::Regex::new(expected).map_err(|e| format!("invalid regex: {e}"))?.is_match(&text),
        o => return Err(format!("unknown operator '{o}'")),
    })
}

async fn send(
    req: &Request,
    sent: &SentRequest,
    cookies: Arc<reqwest_cookie_store::CookieStoreMutex>,
    options: &Options,
    tls: &tls::Tls,
) -> Result<Response, Error> {
    let has = |d: &str| req.directive(d).is_some();
    // Each followed hop is recorded for the Timeline.
    let hops: Arc<Mutex<Vec<Redirect>>> = Arc::default();
    let redirect = if has("no-redirect") || !options.follow_redirects {
        reqwest::redirect::Policy::none()
    } else {
        let (max, hops) = (options.max_redirects, hops.clone());
        reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > max {
                return attempt.error(format!("too many redirects (more than {max})"));
            }
            let from = attempt.previous().last().map(|u| u.to_string()).unwrap_or_default();
            hops.lock().unwrap().push(Redirect { status: attempt.status().as_u16(), from, to: attempt.url().to_string() });
            attempt.follow()
        })
    };
    let cookies_sent = match (has("no-cookie-jar"), reqwest::Url::parse(&sent.url)) {
        (false, Ok(url)) => cookies.lock().unwrap().matches(&url).into_iter().map(|c| (c.name().to_string(), c.value().to_string())).collect(),
        _ => vec![],
    };
    // ponytail: a client per run (redirect/TLS/connect-timeout are client-level in reqwest),
    // so no connection reuse between runs; cache clients by settings if latency matters.
    let mut builder = client_builder(options, tls, has("insecure"))?.redirect(redirect);
    if !has("no-cookie-jar") {
        builder = builder.cookie_provider(cookies);
    }
    if let Some(t) = duration(req, "connection-timeout")? {
        builder = builder.connect_timeout(t);
    }
    let client = builder.build()?;

    let method = reqwest::Method::from_bytes(sent.method.as_bytes())
        .map_err(|_| Error::BadRequest(format!("invalid method '{}'", sent.method)))?;
    let mut rb = client.request(method, &sent.url).body(sent.body.clone());
    for (n, v) in &sent.headers {
        rb = rb.header(n, v);
    }
    if let Some(t) = duration(req, "timeout")?.or(options.timeout) {
        rb = rb.timeout(t);
    }

    // ponytail: total + TTFB only; DNS/TCP/TLS phases need a custom connector.
    let start = Instant::now();
    let resp = rb.send().await?;
    let ttfb = start.elapsed();
    let status = resp.status();
    let http_version = match resp.version() {
        reqwest::Version::HTTP_09 => "HTTP/0.9",
        reqwest::Version::HTTP_10 => "HTTP/1.0",
        reqwest::Version::HTTP_2 => "HTTP/2",
        reqwest::Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/1.1",
    }
    .to_string();
    let headers = resp
        .headers()
        .iter()
        .map(|(n, v)| (n.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let url = resp.url().to_string();
    // ponytail: buffered; stream chunks to the UI when big bodies need it.
    let body = resp.bytes().await?.to_vec();
    let redirects = std::mem::take(&mut *hops.lock().unwrap());
    Ok(Response {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or("").to_string(),
        http_version,
        headers,
        body,
        ttfb,
        total: start.elapsed(),
        url,
        redirects,
        cookies_sent,
    })
}

/// `>>` picks a free name (`out-1.json`) if the file exists; `>>!` overwrites.
fn write_output(path: &Path, overwrite: bool, body: &[u8]) -> Result<(), Error> {
    let io = |p: &Path, e| Error::File(p.to_path_buf(), e);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    let mut target = path.to_path_buf();
    if !overwrite {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let mut n = 1;
        while target.exists() {
            target = path.with_file_name(format!("{stem}-{n}{ext}"));
            n += 1;
        }
    }
    std::fs::write(&target, body).map_err(|e| io(&target, e))
}
