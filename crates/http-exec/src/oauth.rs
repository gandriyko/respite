//! OAuth 2.0 for `{{$auth.token("id")}}` / `{{$auth.idToken("id")}}`, configured like JetBrains
//! HTTP Client: `Security.Auth.<id>` in env files. Grants: Client Credentials and Password, with
//! refresh tokens; `Type: Mock` hands out a fixed token.
// ponytail: Authorization Code + PKCE and Device Authorization need a browser; stage 2.

use std::collections::HashMap;
use std::hash::{Hash as _, Hasher as _};
use std::path::Path;

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

    fn need(&self, field: &str) -> Result<&str, String> {
        self.str(field).ok_or_else(|| format!("needs \"{field}\""))
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

/// `Custom Request Parameters` / `Custom Request Headers` meant for the token request:
/// `"name": "v"`, `"name": ["a", "b"]`, or `{"Value": …, "Use": "Everywhere" | "In Token Request"}`.
fn custom(cfg: &Config, field: &str) -> Vec<(String, String)> {
    let Some(Value::Object(o)) = cfg.map.get(field) else { return vec![] };
    let mut out = vec![];
    for (name, v) in o {
        let value = match v {
            Value::Object(spec) if spec.get("Use").and_then(Value::as_str).is_some_and(|u| u == "In Auth Request") => continue,
            Value::Object(spec) => spec.get("Value").cloned().unwrap_or(Value::Null),
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
pub(crate) async fn acquire(client: &reqwest::Client, cfg: &Config, previous: Option<&Token>) -> Result<Token, Error> {
    match cfg.str("Type").unwrap_or("OAuth2") {
        "Mock" => {
            return Ok(Token {
                config: cfg.hash.clone(),
                access_token: cfg.need("Token").map_err(Error::Config)?.into(),
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
        match request(client, cfg, &[("grant_type", "refresh_token"), ("refresh_token", rt)]).await {
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
        Some("Client Credentials") => request(client, cfg, &[("grant_type", "client_credentials")]).await,
        Some("Password") => {
            let (user, pass) = (cfg.need("Username").map_err(Error::Config)?, cfg.need("Password").map_err(Error::Config)?);
            request(client, cfg, &[("grant_type", "password"), ("username", user), ("password", pass)]).await
        }
        Some(g @ ("Authorization Code" | "Device Authorization" | "Implicit")) => {
            Err(Error::Config(format!("\"Grant Type\": \"{g}\" is not supported yet (Client Credentials, Password)")))
        }
        Some(g) => Err(Error::Config(format!("unknown \"Grant Type\": \"{g}\""))),
        None => Err(Error::Config("needs \"Grant Type\"".into())),
    }
}

#[derive(Debug)]
pub(crate) enum Error {
    /// The config is incomplete or asks for something unsupported.
    Config(String),
    /// The token endpoint answered with an error.
    Server(String),
    Http(reqwest::Error),
}

async fn request(client: &reqwest::Client, cfg: &Config, grant: &[(&str, &str)]) -> Result<Token, Error> {
    let url = cfg.need("Token URL").map_err(Error::Config)?;
    let client_id = cfg.need("Client ID").map_err(Error::Config)?;
    let secret = cfg.str("Client Secret");
    let mut form: Vec<(String, String)> = grant.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    if let Some(scope) = cfg.str("Scope") {
        form.push(("scope".into(), scope.into()));
    }
    let mut req = client.post(url).header(reqwest::header::ACCEPT, "application/json");
    match cfg.str("Client Credentials").unwrap_or("basic") {
        "basic" => req = req.basic_auth(client_id, secret),
        "in body" => {
            form.push(("client_id".into(), client_id.into()));
            form.extend(secret.map(|s| ("client_secret".to_string(), s.to_string())));
        }
        "none" => form.push(("client_id".into(), client_id.into())),
        m => return Err(Error::Config(format!("unknown \"Client Credentials\": \"{m}\" (basic, in body, none)"))),
    }
    form.extend(custom(cfg, "Custom Request Parameters"));
    for (k, v) in custom(cfg, "Custom Request Headers") {
        req = req.header(k, v);
    }
    let res = req.form(&form).send().await.map_err(Error::Http)?;
    let status = res.status();
    let text = res.text().await.map_err(Error::Http)?;
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let reason = match (json["error"].as_str(), json["error_description"].as_str()) {
            (Some(e), Some(d)) => format!("{e}: {d}"),
            (Some(e), None) => e.into(),
            _ => text.chars().take(300).collect(),
        };
        return Err(Error::Server(format!("token request failed with {status}: {reason}")));
    }
    let Some(access) = json["access_token"].as_str() else {
        return Err(Error::Server(format!("no access_token in the token response: {}", text.chars().take(300).collect::<String>())));
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
