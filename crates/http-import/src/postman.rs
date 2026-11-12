//! Postman collection v2.0 / v2.1 and Postman environment files.

use serde_json::Value;

use crate::{Collection, Folder, Req, form_body, graphql, multipart, pretty_json, scalar};

pub fn collection(v: &Value) -> Collection {
    let mut c = Collection { name: str_of(&v["info"]["name"]), format: "Postman collection", ..Default::default() };
    let vars: Vec<(String, String)> = v["variable"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| Some((x["key"].as_str()?.to_string(), scalar(&x["value"]).unwrap_or_default())))
        .collect();
    if !vars.is_empty() {
        c.envs.insert("$shared".into(), vars);
    }
    if has_scripts(v) {
        c.warnings.push("collection-level scripts were not converted".into());
    }
    let mut warnings = vec![];
    c.root = folder(&c.name, &v["item"], &v["auth"], &mut warnings);
    c.warnings.extend(warnings);
    c
}

/// Postman environment export: `{name, values: [{key, value, enabled, type}]}`.
pub fn environment(v: &Value) -> Collection {
    let name = str_of(&v["name"]);
    let (mut public, mut secret) = (vec![], vec![]);
    for x in v["values"].as_array().into_iter().flatten() {
        if x["enabled"] == false {
            continue;
        }
        let Some(key) = x["key"].as_str() else { continue };
        let pair = (key.to_string(), scalar(&x["value"]).unwrap_or_default());
        if x["type"] == "secret" { secret.push(pair) } else { public.push(pair) }
    }
    let env = if name.is_empty() { "imported".to_string() } else { name.clone() };
    let mut c = Collection { name, format: "Postman environment", ..Default::default() };
    if !public.is_empty() || secret.is_empty() {
        c.envs.insert(env.clone(), public);
    }
    if !secret.is_empty() {
        c.private_envs.insert(env, secret);
    }
    c
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn has_scripts(v: &Value) -> bool {
    v["event"].as_array().into_iter().flatten().any(|e| e["script"]["exec"].as_array().is_some_and(|l| l.iter().any(|x| x.as_str().is_some_and(|s| !s.trim().is_empty()))))
}

fn folder(name: &str, items: &Value, inherited: &Value, warnings: &mut Vec<String>) -> Folder {
    let mut f = Folder { name: name.into(), ..Default::default() };
    for item in items.as_array().into_iter().flatten() {
        let item_name = str_of(&item["name"]);
        if item["item"].is_array() {
            let auth = if item["auth"].is_object() { &item["auth"] } else { inherited };
            if has_scripts(item) {
                warnings.push(format!("{item_name}: folder scripts were not converted"));
            }
            f.folders.push(folder(&item_name, &item["item"], auth, warnings));
        } else if !item["request"].is_null() {
            f.requests.push(request(item, inherited, warnings));
        }
    }
    f
}

fn request(item: &Value, inherited: &Value, warnings: &mut Vec<String>) -> Req {
    let r = &item["request"];
    let mut req = Req { name: str_of(&item["name"]), method: "GET".into(), ..Default::default() };
    if let Some(url) = r.as_str() {
        req.url = url.into(); // v2: a request can be just its URL
        return req;
    }
    req.method = r["method"].as_str().unwrap_or("GET").to_string();
    req.url = url(&r["url"]);
    if let Some(desc) = r["description"].as_str().or(item["description"].as_str()).filter(|d| !d.trim().is_empty()) {
        req.notes.push(desc.lines().next().unwrap_or_default().to_string());
    }
    for h in r["header"].as_array().into_iter().flatten() {
        if h["disabled"] != true
            && let Some(k) = h["key"].as_str()
        {
            req.headers.push((k.into(), str_of(&h["value"])));
        }
    }
    let auth = match r["auth"]["type"].as_str() {
        Some("inherit") | None => inherited,
        Some(_) => &r["auth"],
    };
    apply_auth(&mut req, auth, warnings);
    body(&mut req, &r["body"]);
    if has_scripts(item) {
        req.notes.push("Postman scripts (pre-request / tests) were not converted".into());
        warnings.push(format!("{}: scripts were not converted", req.name));
    }
    req
}

fn url(u: &Value) -> String {
    if let Some(s) = u.as_str() {
        return s.into();
    }
    if let Some(raw) = u["raw"].as_str() {
        return raw.into();
    }
    // No `raw`: rebuild from parts.
    let join = |v: &Value, sep: &str| match v {
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string).or_else(|| x["value"].as_str().map(str::to_string))).collect::<Vec<_>>().join(sep),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let mut s = format!("{}{}", u["protocol"].as_str().map(|p| format!("{p}://")).unwrap_or_default(), join(&u["host"], "."));
    let path = join(&u["path"], "/");
    if !path.is_empty() {
        s += &format!("/{path}");
    }
    let query: Vec<String> = u["query"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|q| q["disabled"] != true)
        .filter_map(|q| Some(format!("{}={}", q["key"].as_str()?, str_of(&q["value"]))))
        .collect();
    if !query.is_empty() {
        s += &format!("?{}", query.join("&"));
    }
    s
}

/// Auth fields are `[{key, value}]` in v2.1 and `{key: value}` in v2.0.
fn field(auth: &Value, kind: &str, key: &str) -> String {
    let section = &auth[kind];
    match section {
        Value::Array(a) => a.iter().find(|x| x["key"] == key).and_then(|x| scalar(&x["value"])).unwrap_or_default(),
        _ => scalar(&section[key]).unwrap_or_default(),
    }
}

fn apply_auth(req: &mut Req, auth: &Value, warnings: &mut Vec<String>) {
    match auth["type"].as_str() {
        None | Some("noauth") => {}
        Some("bearer") => req.default_header("Authorization", format!("Bearer {}", field(auth, "bearer", "token"))),
        Some("basic") => {
            let (u, p) = (field(auth, "basic", "username"), field(auth, "basic", "password"));
            req.default_header("Authorization", format!("Basic {u} {p}"));
        }
        Some("apikey") => {
            let (k, v) = (field(auth, "apikey", "key"), field(auth, "apikey", "value"));
            if field(auth, "apikey", "in") == "query" {
                req.url.push(if req.url.contains('?') { '&' } else { '?' });
                req.url += &format!("{k}={v}");
            } else {
                req.default_header(&k, v);
            }
        }
        Some(other) => {
            req.notes.push(format!("Postman auth '{other}' was not converted"));
            warnings.push(format!("{}: auth '{other}' was not converted", req.name));
        }
    }
}

fn body(req: &mut Req, b: &Value) {
    let pairs = |key: &str| -> Vec<(String, String)> {
        b[key].as_array().into_iter().flatten().filter(|p| p["disabled"] != true).filter_map(|p| Some((p["key"].as_str()?.to_string(), str_of(&p["value"])))).collect()
    };
    match b["mode"].as_str() {
        Some("raw") => {
            let text = str_of(&b["raw"]);
            let ct = match b["options"]["raw"]["language"].as_str() {
                Some("json") => Some("application/json"),
                Some("xml") => Some("application/xml"),
                Some("html") => Some("text/html"),
                Some("javascript") => Some("application/javascript"),
                _ => None,
            };
            if let Some(ct) = ct {
                req.default_header("Content-Type", ct);
            }
            req.body = Some(if ct == Some("application/json") { pretty_json(&text) } else { text });
        }
        Some("urlencoded") => {
            req.default_header("Content-Type", "application/x-www-form-urlencoded");
            req.body = Some(form_body(&pairs("urlencoded")));
        }
        Some("formdata") => {
            let parts: Vec<(String, Result<String, String>)> = b["formdata"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["disabled"] != true)
                .filter_map(|p| {
                    let key = p["key"].as_str()?.to_string();
                    Some(if p["type"] == "file" {
                        let src = p["src"].as_str().or_else(|| p["src"][0].as_str()).unwrap_or("./file");
                        (key, Err(local(src)))
                    } else {
                        (key, Ok(str_of(&p["value"])))
                    })
                })
                .collect();
            multipart(req, &parts);
        }
        Some("graphql") => {
            let vars = b["graphql"]["variables"].as_str().map(str::to_string);
            graphql(req, b["graphql"]["query"].as_str().unwrap_or_default(), vars.as_deref());
            req.default_header("Content-Type", "application/json");
        }
        Some("file") => {
            if let Some(src) = b["file"]["src"].as_str() {
                req.body = Some(format!("< {}", local(src)));
            }
        }
        _ => {}
    }
}

/// Relative paths get `./` like the rest of the `.http` syntax.
fn local(p: &str) -> String {
    if p.starts_with(['/', '.', '~']) { p.into() } else { format!("./{p}") }
}
