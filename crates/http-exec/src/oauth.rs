//! OAuth 2.0 for `{{$auth.token("id")}}` / `{{$auth.idToken("id")}}`, configured like JetBrains
//! HTTP Client: `Security.Auth.<id>` in env files. Grants: Client Credentials, Password,
//! Authorization Code (system browser, loopback redirect, PKCE) and Device Authorization, with
//! refresh tokens; `Type: Mock` hands out a fixed token.
// ponytail: Implicit is left out (dropped by OAuth 2.1); add it if someone's server needs it.

use std::collections::HashMap;
use std::hash::{Hash as _, Hasher as _};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

/// An acquired token, as cached in `~/.respite/history/<project>/oauth.<env>.json`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Token {
    /// Hash of the resolved config: a changed Client ID, scope, URL or secret drops the token.
    pub config: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// Unix seconds; `None`: the server gave no `expires_in`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// When to stop using it: before `expires_at` by [`MARGIN`], or by half of a shorter life.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Seconds before expiry at which a token counts as stale (clock skew, slow requests).
const MARGIN: i64 = 30;
/// How long to wait for the user to finish signing in in the browser.
const BROWSER_WAIT: Duration = Duration::from_secs(300);

pub(crate) fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

impl Token {
    pub fn fresh(&self, now: i64) -> bool {
        self.stale_at.or(self.expires_at).is_none_or(|s| s > now)
    }

    /// `token` → access token, `idToken` → ID token.
    pub fn get(&self, kind: &str) -> Option<&str> {
        match kind {
            "idToken" => self.id_token.as_deref(),
            _ => Some(&self.access_token),
        }
    }

    /// Values to mask in history.
    pub(crate) fn secrets(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.access_token.as_str()).chain(self.refresh_token.as_deref()).chain(self.id_token.as_deref())
    }
}

/// How a sign-in reaches the user: a message to show (a device code, "finish in the browser")
/// and a URL to open. The app shows the message in the response panel, the CLI on stderr.
#[derive(Clone)]
pub struct AuthUi {
    pub notice: Arc<dyn Fn(String) + Send + Sync>,
    pub open_url: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Default for AuthUi {
    fn default() -> Self {
        Self { notice: Arc::new(|_| {}), open_url: Arc::new(open_in_browser) }
    }
}

/// The system browser; a failure only means the user opens the URL from the notice.
pub fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    if let Err(e) = cmd.arg(url).spawn() {
        eprintln!("respite: could not open the browser: {e}");
    }
}

/// A config with `{{variables}}` resolved, plus its hash.
pub(crate) struct Config {
    pub map: Map<String, Value>,
    pub hash: String,
}

impl Config {
    pub fn new(map: Map<String, Value>) -> Self {
        let mut h = std::hash::DefaultHasher::new();
        Value::Object(map.clone()).to_string().hash(&mut h);
        Self { map, hash: format!("{:016x}", h.finish()) }
    }

    fn str(&self, field: &str) -> Option<&str> {
        self.map.get(field).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    fn flag(&self, field: &str) -> bool {
        self.map.get(field).and_then(Value::as_bool).unwrap_or(false)
    }

    fn need(&self, field: &str) -> Result<&str, Error> {
        self.str(field).ok_or_else(|| Error::Config(format!("needs \"{field}\"")))
    }

    /// `"Acquire Automatically": false` — only an explicit `respite auth token` gets one.
    pub fn automatic(&self) -> bool {
        self.map.get("Acquire Automatically").and_then(Value::as_bool).unwrap_or(true)
    }
}

/// Every string in `v` passed through `r`.
pub(crate) fn resolve_strings<E>(v: &Value, r: &dyn Fn(&str) -> Result<String, E>) -> Result<Value, E> {
    Ok(match v {
        Value::String(s) => Value::String(r(s)?),
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve_strings(x, r)).collect::<Result<_, _>>()?),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| Ok((k.clone(), resolve_strings(x, r)?))).collect::<Result<_, E>>()?),
        other => other.clone(),
    })
}

/// Which request a custom parameter or header is for.
#[derive(Clone, Copy, PartialEq)]
enum Leg {
    /// The browser's authorization request, or the device authorization request.
    Auth,
    Token,
}

/// `Custom Request Parameters` / `Custom Request Headers` for `leg`: `"name": "v"`,
/// `"name": ["a", "b"]`, or `{"Value": …, "Use": "Everywhere" | "In Auth Request" | "In Token Request"}`.
fn custom(cfg: &Config, field: &str, leg: Leg) -> Vec<(String, String)> {
    let Some(Value::Object(o)) = cfg.map.get(field) else { return vec![] };
    let mut out = vec![];
    for (name, v) in o {
        let value = match v {
            Value::Object(spec) => {
                let wanted = match spec.get("Use").and_then(Value::as_str) {
                    Some("In Auth Request") => leg == Leg::Auth,
                    Some("In Token Request") => leg == Leg::Token,
                    _ => true,
                };
                if !wanted {
                    continue;
                }
                spec.get("Value").cloned().unwrap_or(Value::Null)
            }
            v => v.clone(),
        };
        let values = match value {
            Value::Array(a) => a,
            v => vec![v],
        };
        out.extend(values.into_iter().filter_map(|v| match v {
            Value::String(s) => Some((name.clone(), s)),
            Value::Null => None,
            v => Some((name.clone(), v.to_string())),
        }));
    }
    out
}

/// Get a token: a refresh grant when `previous` (same config) has a refresh token, falling back to
/// the configured grant when that fails.
pub(crate) async fn acquire(client: &reqwest::Client, cfg: &Config, previous: Option<&Token>, ui: &AuthUi) -> Result<Token, Error> {
    match cfg.str("Type").unwrap_or("OAuth2") {
        "Mock" => {
            return Ok(Token {
                config: cfg.hash.clone(),
                access_token: cfg.need("Token")?.into(),
                id_token: cfg.str("ID Token").map(Into::into),
                refresh_token: None,
                expires_at: None,
                stale_at: None,
                scope: None,
            });
        }
        "OAuth2" => {}
        t => return Err(Error::Config(format!("unsupported \"Type\": \"{t}\" (OAuth2 or Mock)"))),
    }
    let refresh = previous.filter(|p| p.config == cfg.hash).and_then(|p| p.refresh_token.clone());
    if let Some(rt) = &refresh {
        match token_request(client, cfg, &[("grant_type", "refresh_token"), ("refresh_token", rt)]).await {
            Ok(mut t) => {
                // Servers may keep the refresh token and not send it again.
                t.refresh_token = t.refresh_token.or(refresh);
                return Ok(t);
            }
            Err(Error::Http(e)) => return Err(Error::Http(e)),
            Err(_) => {} // expired or revoked: the full grant below
        }
    }
    match cfg.str("Grant Type") {
        Some("Client Credentials") => token_request(client, cfg, &[("grant_type", "client_credentials")]).await,
        Some("Password") => {
            let (user, pass) = (cfg.need("Username")?, cfg.need("Password")?);
            token_request(client, cfg, &[("grant_type", "password"), ("username", user), ("password", pass)]).await
        }
        Some("Authorization Code") => authorization_code(client, cfg, ui).await,
        Some("Device Authorization") => device(client, cfg, ui).await,
        Some("Implicit") => Err(Error::Config("\"Grant Type\": \"Implicit\" is not supported; use \"Authorization Code\" with \"PKCE\": true".into())),
        Some(g) => Err(Error::Config(format!("unknown \"Grant Type\": \"{g}\""))),
        None => Err(Error::Config("needs \"Grant Type\"".into())),
    }
}

#[derive(Debug)]
pub(crate) enum Error {
    /// The config is incomplete or asks for something unsupported.
    Config(String),
    /// The authorization server answered with an error, or the sign-in failed.
    Server(String),
    /// Device flow: `authorization_pending` / `slow_down` — poll again.
    Pending(String),
    Http(reqwest::Error),
}

/// POST a form to an endpoint of the authorization server with the client's credentials.
async fn post(client: &reqwest::Client, cfg: &Config, url: &str, fields: &[(&str, &str)], leg: Leg) -> Result<Value, Error> {
    let client_id = cfg.need("Client ID")?;
    let secret = cfg.str("Client Secret");
    let mut form: Vec<(String, String)> = fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    if let Some(scope) = cfg.str("Scope") {
        form.push(("scope".into(), scope.into()));
    }
    let mut req = client.post(url).header(reqwest::header::ACCEPT, "application/json");
    match (cfg.str("Client Credentials").unwrap_or("basic"), secret) {
        // A public client (no secret) identifies itself in the body.
        ("basic", Some(s)) => req = req.basic_auth(client_id, Some(s)),
        ("basic" | "none", None) | ("none", Some(_)) => form.push(("client_id".into(), client_id.into())),
        ("in body", s) => {
            form.push(("client_id".into(), client_id.into()));
            form.extend(s.map(|s| ("client_secret".to_string(), s.to_string())));
        }
        (m, _) => return Err(Error::Config(format!("unknown \"Client Credentials\": \"{m}\" (basic, in body, none)"))),
    }
    form.extend(custom(cfg, "Custom Request Parameters", leg));
    for (k, v) in custom(cfg, "Custom Request Headers", leg) {
        req = req.header(k, v);
    }
    let res = req.form(&form).send().await.map_err(Error::Http)?;
    let status = res.status();
    let text = res.text().await.map_err(Error::Http)?;
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(json);
    }
    match (json["error"].as_str(), json["error_description"].as_str()) {
        (Some(e @ ("authorization_pending" | "slow_down")), _) => Err(Error::Pending(e.into())),
        (Some(e), Some(d)) => Err(Error::Server(format!("{status}: {e}: {d}"))),
        (Some(e), None) => Err(Error::Server(format!("{status}: {e}"))),
        _ => Err(Error::Server(format!("{status}: {}", text.chars().take(300).collect::<String>()))),
    }
}

async fn token_request(client: &reqwest::Client, cfg: &Config, grant: &[(&str, &str)]) -> Result<Token, Error> {
    let url = cfg.need("Token URL")?;
    let json = post(client, cfg, url, grant, Leg::Token).await.map_err(|e| match e {
        Error::Server(m) => Error::Server(format!("token request failed with {m}")),
        e => e,
    })?;
    let Some(access) = json["access_token"].as_str() else {
        return Err(Error::Server(format!("no access_token in the token response: {}", json.to_string().chars().take(300).collect::<String>())));
    };
    // Some servers send `expires_in` as a string.
    let expires_in = json["expires_in"].as_i64().or_else(|| json["expires_in"].as_str()?.parse().ok());
    let s = |k: &str| json[k].as_str().map(String::from);
    let t = now();
    Ok(Token {
        config: cfg.hash.clone(),
        access_token: access.into(),
        refresh_token: s("refresh_token"),
        id_token: s("id_token"),
        expires_at: expires_in.map(|e| t + e),
        stale_at: expires_in.map(|e| t + e - MARGIN.min(e / 2)),
        scope: s("scope"),
    })
}

fn random() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// `"PKCE": true` (S256, generated verifier) or `{"Code Challenge Method": "Plain" | "SHA-256",
/// "Code Verifier": "…"}` → (verifier, challenge, method). Off when absent or `false`.
fn pkce(cfg: &Config) -> Result<Option<(String, String, &'static str)>, Error> {
    let (method, verifier) = match cfg.map.get("PKCE") {
        None | Some(Value::Null | Value::Bool(false)) => return Ok(None),
        Some(Value::Bool(true)) => ("SHA-256", None),
        Some(Value::Object(o)) => (
            o.get("Code Challenge Method").and_then(Value::as_str).unwrap_or("SHA-256"),
            o.get("Code Verifier").and_then(Value::as_str).filter(|v| !v.is_empty()),
        ),
        Some(v) => return Err(Error::Config(format!("\"PKCE\" must be true or an object, not {v}"))),
    };
    // 64 characters from the unreserved set, as RFC 7636 asks (43–128).
    let verifier = verifier.map_or_else(|| random() + &random(), String::from);
    Ok(Some(match method {
        "SHA-256" | "S256" => {
            use base64::Engine as _;
            use sha2::Digest as _;
            let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
            (verifier, challenge, "S256")
        }
        "Plain" | "plain" => (verifier.clone(), verifier, "plain"),
        m => return Err(Error::Config(format!("unknown PKCE \"Code Challenge Method\": \"{m}\" (SHA-256, Plain)"))),
    }))
}

/// A listener for the browser's redirect, and the redirect URI to send: `Redirect URL` when it is
/// a loopback `http://` URL, else `http://127.0.0.1:<free port>/callback`.
async fn loopback(redirect: Option<&str>) -> Result<(tokio::net::TcpListener, reqwest::Url), Error> {
    let url = match redirect {
        Some(r) => reqwest::Url::parse(r).map_err(|e| Error::Config(format!("invalid \"Redirect URL\": {e}")))?,
        None => reqwest::Url::parse("http://127.0.0.1:0/callback").expect("valid"),
    };
    let host = url.host_str().unwrap_or("");
    if url.scheme() != "http" || !["127.0.0.1", "localhost", "[::1]"].contains(&host) {
        return Err(Error::Config(format!(
            "\"Redirect URL\" must be a loopback address such as http://127.0.0.1:8765/callback, not {url}: \
             the browser has to come back to Respite"
        )));
    }
    let ip = if host == "[::1]" { "[::1]" } else { "127.0.0.1" };
    let addr = format!("{ip}:{}", url.port().unwrap_or(80));
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| Error::Config(format!("cannot listen on {addr} for the redirect: {e}")))?;
    let mut url = url;
    if redirect.is_none() {
        let port = listener.local_addr().map_err(|e| Error::Config(e.to_string()))?.port();
        url.set_port(Some(port)).expect("http URL takes a port");
    }
    Ok((listener, url))
}

/// Wait for the browser to hit `redirect`; answer it with a page and return the `code`.
async fn callback(listener: &tokio::net::TcpListener, redirect: &reqwest::Url, state: &str) -> Result<String, Error> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    loop {
        let (mut sock, _) = listener.accept().await.map_err(|e| Error::Server(format!("redirect listener: {e}")))?;
        let mut buf = vec![0u8; 8192];
        let mut len = 0;
        while len < buf.len() && !buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut buf[len..]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => len += n,
            }
        }
        let head = String::from_utf8_lossy(&buf[..len]);
        let target = head.split_whitespace().nth(1).unwrap_or("/");
        let url = reqwest::Url::parse(&format!("http://loopback{target}")).ok();
        if url.as_ref().is_none_or(|u| u.path() != redirect.path()) {
            // The browser also asks for /favicon.ico and the like.
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            continue;
        }
        let q: HashMap<String, String> = url.expect("checked").query_pairs().into_owned().collect();
        let result = match (q.get("error"), q.get("code")) {
            (Some(e), _) => Err(Error::Server(format!("sign-in failed: {e}{}", q.get("error_description").map(|d| format!(": {d}")).unwrap_or_default()))),
            _ if q.get("state").map(String::as_str) != Some(state) => Err(Error::Server("sign-in redirect with a wrong state".into())),
            (None, Some(code)) => Ok(code.clone()),
            (None, None) => Err(Error::Server("sign-in redirect without a code".into())),
        };
        let text = match &result {
            Ok(_) => "Signed in. You can close this tab and go back to Respite.",
            Err(_) => "Sign-in failed. See Respite for details.",
        };
        let page = format!("<!doctype html><meta charset=utf-8><title>Respite</title><p style=\"font:16px system-ui;margin:3em\">{text}</p>");
        let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
        let _ = sock.write_all(reply.as_bytes()).await;
        return result;
    }
}

async fn authorization_code(client: &reqwest::Client, cfg: &Config, ui: &AuthUi) -> Result<Token, Error> {
    let mut url = reqwest::Url::parse(cfg.need("Auth URL")?).map_err(|e| Error::Config(format!("invalid \"Auth URL\": {e}")))?;
    let (listener, redirect) = loopback(cfg.str("Redirect URL")).await?;
    let (state, pkce) = (random(), pkce(cfg)?);
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code").append_pair("client_id", cfg.need("Client ID")?).append_pair("redirect_uri", redirect.as_str());
        q.append_pair("state", &state);
        if let Some(scope) = cfg.str("Scope") {
            q.append_pair("scope", scope);
        }
        if let Some((_, challenge, method)) = &pkce {
            q.append_pair("code_challenge", challenge).append_pair("code_challenge_method", method);
        }
        for (k, v) in custom(cfg, "Custom Request Parameters", Leg::Auth) {
            q.append_pair(&k, &v);
        }
    }
    (ui.notice)(format!("Sign in in the browser to continue. If it didn't open: {url}"));
    (ui.open_url)(url.as_str());
    let code = tokio::time::timeout(BROWSER_WAIT, callback(&listener, &redirect, &state))
        .await
        .map_err(|_| Error::Server("no sign-in within 5 minutes".into()))??;
    let mut grant = vec![("grant_type", "authorization_code"), ("code", code.as_str()), ("redirect_uri", redirect.as_str())];
    if let Some((verifier, ..)) = &pkce {
        grant.push(("code_verifier", verifier));
    }
    token_request(client, cfg, &grant).await
}

async fn device(client: &reqwest::Client, cfg: &Config, ui: &AuthUi) -> Result<Token, Error> {
    let json = post(client, cfg, cfg.need("Device Auth URL")?, &[], Leg::Auth).await.map_err(|e| match e {
        Error::Server(m) => Error::Server(format!("device authorization failed with {m}")),
        e => e,
    })?;
    let field = |k: &str| json[k].as_str().map(String::from);
    let (Some(device_code), Some(user_code)) = (field("device_code"), field("user_code")) else {
        return Err(Error::Server(format!("unexpected device authorization response: {json}")));
    };
    // `verification_url`: Google's older spelling.
    let uri = field("verification_uri").or_else(|| field("verification_url")).unwrap_or_default();
    let complete = field("verification_uri_complete").filter(|_| cfg.flag("Open Complete URI"));
    (ui.notice)(format!("Enter code {user_code} at {uri}"));
    (ui.open_url)(complete.as_deref().unwrap_or(&uri));
    let mut interval = json["interval"].as_u64().unwrap_or(5);
    let deadline = now() + json["expires_in"].as_i64().unwrap_or(600);
    let grant = [("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", device_code.as_str())];
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        match token_request(client, cfg, &grant).await {
            Err(Error::Pending(p)) if p == "slow_down" => interval += 5,
            Err(Error::Pending(_)) => {}
            r => return r,
        }
        if now() > deadline {
            return Err(Error::Server(format!("code {user_code} expired before the sign-in finished")));
        }
    }
}

pub(crate) fn load(path: &Path) -> HashMap<String, Token> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Owner-only (0600): the file holds bearer tokens.
pub(crate) fn save(path: &Path, tokens: &HashMap<String, Token>) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(path)?;
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    f.write_all(&serde_json::to_vec_pretty(tokens).map_err(std::io::Error::other)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_rfc7636() {
        // RFC 7636, appendix B.
        let cfg = Config::new(serde_json::from_str(r#"{"PKCE": {"Code Verifier": "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"}}"#).unwrap());
        let (_, challenge, method) = pkce(&cfg).unwrap().unwrap();
        assert_eq!((challenge.as_str(), method), ("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", "S256"));
        let off = Config::new(serde_json::from_str(r#"{"PKCE": false}"#).unwrap());
        assert!(pkce(&off).unwrap().is_none());
        let generated = Config::new(serde_json::from_str(r#"{"PKCE": true}"#).unwrap());
        assert_eq!(pkce(&generated).unwrap().unwrap().0.len(), 64);
    }
}
