//! Request executor: `{{variables}}` → HTTP (reqwest) → response.
//! Requests referenced as `{{name.response…}}` are run first when no cached response exists.
//!
//! Async but runtime-agnostic in API; reqwest needs a tokio runtime to drive it.
//! Cancel by dropping the future.

mod store;

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
use http_syntax::{HttpFile, Request};
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
}

#[derive(Debug)]
pub enum Error {
    Resolve(ResolveError),
    File(PathBuf, std::io::Error),
    Http(reqwest::Error),
    BadRequest(String),
    DependencyCycle(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(e) => e.fmt(f),
            Self::File(p, e) => write!(f, "{}: {e}", p.display()),
            Self::Http(e) => write!(f, "{e}"),
            Self::BadRequest(m) => f.write_str(m),
            Self::DependencyCycle(n) => write!(f, "request dependency cycle through '{n}'"),
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
}

impl Default for Options {
    fn default() -> Self {
        Self { timeout: None, follow_redirects: true, max_redirects: 10 }
    }
}

/// Long-lived: owns the cookie jar and the named-response cache. Share via `Arc`.
pub struct Executor {
    options: std::sync::RwLock<Options>,
    cookies: Arc<reqwest_cookie_store::CookieStoreMutex>,
    /// In-memory layer over `store.latest()`.
    named: Mutex<HashMap<(PathBuf, String), Arc<Exchange>>>,
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
        Self {
            options: Default::default(),
            cookies: Arc::new(reqwest_cookie_store::CookieStoreMutex::new(jar)),
            named: Mutex::default(),
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
            let secrets: Vec<&str> =
                cx.env.values().filter(|v| v.secret && v.value.len() >= 4).map(|v| v.value.as_str()).collect();
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
            let prepared = loop {
                let misses = RefCell::new(vec![]);
                match self.prepare_inner(cx, req, &misses) {
                    Ok(p) => break p,
                    Err(Error::Resolve(ResolveError::Reference(_))) if !misses.borrow().is_empty() => {
                        for name in misses.take() {
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
            let response = send(req, &prepared.sent, self.cookies.clone(), &options).await?;
            if let Some((path, overwrite)) = &prepared.output {
                write_output(path, *overwrite, &response.body)?;
            }
            let mut ex =
                Exchange { request: prepared.sent, response, dependencies: ran[first_dep..].to_vec(), history_id: None };
            self.persist(cx, req, &mut ex);
            if let Some(name) = &req.name {
                let key = (cx.path.to_path_buf(), name.clone());
                self.named.lock().unwrap().insert(key, Arc::new(ex.clone()));
            }
            Ok(ex)
        })
    }

    /// `misses` collects named requests referenced but not cached yet.
    fn prepare_inner(&self, cx: &Context<'_>, req: &Request, misses: &RefCell<Vec<String>>) -> Result<Prepared, Error> {
        let dir = cx.path.parent().unwrap_or(Path::new("."));
        let resolver = Resolver { overrides: cx.overrides, file_vars: &cx.file.variables, env: cx.env, dir };
        let refs = |name: &str, path: &str| {
            match self.cached(cx.path, name) {
                Some(ex) => extract(&ex, path),
                None => {
                    misses.borrow_mut().push(name.to_string());
                    None
                }
            }
        };
        let r = |s: &str| resolver.resolve(s, &refs);

        let mut method = req.method.clone();
        let mut url = r(&req.url)?;
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
        Ok(Prepared { sent: SentRequest { method, url, headers, body }, output })
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

/// `# @timeout 10 s` — units ms / s / m, seconds by default.
fn duration(req: &Request, directive: &str) -> Result<Option<Duration>, Error> {
    let Some(d) = req.directive(directive) else { return Ok(None) };
    let v = d.value.as_deref().unwrap_or("");
    let end = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let bad = || Error::BadRequest(format!("invalid @{directive} '{v}'"));
    let n: u64 = v[..end].parse().map_err(|_| bad())?;
    let ms = match v[end..].trim() {
        "" | "s" => n * 1000,
        "ms" => n,
        "m" => n * 60_000,
        _ => return Err(bad()),
    };
    Ok(Some(Duration::from_millis(ms)))
}

async fn send(
    req: &Request,
    sent: &SentRequest,
    cookies: Arc<reqwest_cookie_store::CookieStoreMutex>,
    options: &Options,
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
    let mut builder = reqwest::Client::builder()
        .redirect(redirect)
        .danger_accept_invalid_certs(has("insecure"));
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
