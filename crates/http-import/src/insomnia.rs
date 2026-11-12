//! Insomnia export v4 (JSON, `_type: export`) and v5 (YAML, `type: collection.insomnia.rest/5.x`).

use std::collections::HashMap;

use serde_json::Value;

use crate::{Collection, Folder, Req, form_body, graphql, multipart, pretty_json, scalar};

/// `{{ _.var }}` (Insomnia's Nunjucks) → `{{var}}`; `{% … %}` tags are left as they are.
fn templates(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}").map(|c| c + open) else { break };
        let inner = rest[open + 2..close].trim();
        out += &rest[..open];
        out += &format!("{{{{{}}}}}", inner.strip_prefix("_.").unwrap_or(inner));
        rest = &rest[close + 2..];
    }
    out += rest;
    out
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// One request (same shape in v4 resources and v5 items).
fn request(r: &Value, warnings: &mut Vec<String>) -> Req {
    let t = templates;
    let mut req = Req {
        name: str_of(&r["name"]),
        method: r["method"].as_str().unwrap_or("GET").to_string(),
        url: t(r["url"].as_str().unwrap_or_default()),
        ..Default::default()
    };
    for h in r["headers"].as_array().into_iter().flatten() {
        if h["disabled"] != true
            && let Some(n) = h["name"].as_str()
        {
            let value = t(h["value"].as_str().unwrap_or_default());
            req.headers.push((n.into(), value));
        }
    }
    let query: Vec<String> = r["parameters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["disabled"] != true)
        .filter_map(|p| Some(format!("{}={}", p["name"].as_str()?, t(p["value"].as_str().unwrap_or_default()))))
        .collect();
    if !query.is_empty() {
        req.url += &format!("{}{}", if req.url.contains('?') { "&" } else { "?" }, query.join("&"));
    }
    let a = &r["authentication"];
    match a["type"].as_str() {
        None | Some("none") => {}
        _ if a["disabled"] == true => {}
        Some("bearer") => {
            let prefix = a["prefix"].as_str().filter(|p| !p.is_empty()).unwrap_or("Bearer");
            req.default_header("Authorization", format!("{prefix} {}", t(a["token"].as_str().unwrap_or_default())));
        }
        Some("basic") => {
            let (u, p) = (t(a["username"].as_str().unwrap_or_default()), t(a["password"].as_str().unwrap_or_default()));
            req.default_header("Authorization", format!("Basic {u} {p}"));
        }
        Some("apikey") => {
            let (k, v) = (str_of(&a["key"]), t(a["value"].as_str().unwrap_or_default()));
            if a["addTo"] == "queryParams" {
                req.url += &format!("{}{k}={v}", if req.url.contains('?') { "&" } else { "?" });
            } else {
                req.default_header(&k, v);
            }
        }
        Some(other) => {
            req.notes.push(format!("Insomnia auth '{other}' was not converted"));
            warnings.push(format!("{}: auth '{other}' was not converted", req.name));
        }
    }
    let b = &r["body"];
    let mime = b["mimeType"].as_str().unwrap_or_default().to_string();
    let params = |key: &str| -> Vec<&Value> { b[key].as_array().into_iter().flatten().filter(|p| p["disabled"] != true).collect() };
    match mime.as_str() {
        "application/x-www-form-urlencoded" => {
            let pairs: Vec<(String, String)> = params("params").iter().map(|p| (str_of(&p["name"]), t(&str_of(&p["value"])))).collect();
            req.default_header("Content-Type", "application/x-www-form-urlencoded");
            req.body = Some(form_body(&pairs));
        }
        "multipart/form-data" => {
            let parts: Vec<(String, Result<String, String>)> = params("params")
                .iter()
                .map(|p| {
                    let name = str_of(&p["name"]);
                    if p["type"] == "file" { (name, Err(str_of(&p["fileName"]))) } else { (name, Ok(t(&str_of(&p["value"])))) }
                })
                .collect();
            multipart(&mut req, &parts);
        }
        "application/graphql" => {
            // Insomnia stores GraphQL as JSON {query, variables}.
            let g: Value = serde_json::from_str(b["text"].as_str().unwrap_or("{}")).unwrap_or_default();
            let vars = g.get("variables").filter(|v| !v.is_null()).map(|v| serde_json::to_string_pretty(v).unwrap_or_default());
            let query = t(g["query"].as_str().unwrap_or_default());
            graphql(&mut req, &query, vars.as_deref());
            req.default_header("Content-Type", "application/json");
        }
        _ => {
            if let Some(text) = b["text"].as_str().filter(|s| !s.is_empty()) {
                let text = t(text);
                req.body = Some(if mime.contains("json") { pretty_json(&text) } else { text });
                if !mime.is_empty() {
                    req.default_header("Content-Type", mime);
                }
            } else if let Some(file) = b["fileName"].as_str() {
                req.body = Some(format!("< {file}"));
            }
        }
    }
    let scripts = ["preRequestScript", "afterResponseScript"].iter().any(|k| r[*k].as_str().is_some_and(|s| !s.trim().is_empty()))
        || r["scripts"]["preRequest"].as_str().is_some_and(|s| !s.trim().is_empty())
        || r["scripts"]["afterResponse"].as_str().is_some_and(|s| !s.trim().is_empty());
    if scripts {
        req.notes.push("Insomnia scripts were not converted".into());
        warnings.push(format!("{}: scripts were not converted", req.name));
    }
    if crate::block(&req).contains("{%") {
        req.notes.push("Insomnia template tags ({% … %}) were kept as text".into());
        warnings.push(format!("{}: template tags (e.g. response references) were kept as text", req.name));
    }
    req
}

fn env_vars(data: &Value) -> Vec<(String, String)> {
    data.as_object().into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), scalar(v)?))).collect()
}

/// v4: flat `resources` linked by `parentId`.
pub fn v4(v: &Value) -> Collection {
    let resources: Vec<&Value> = v["resources"].as_array().into_iter().flatten().collect();
    fn of_type<'a>(resources: &'a [&'a Value], t: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
        resources.iter().copied().filter(move |r| r["_type"] == t)
    }
    let of_type = |t: &'static str| of_type(&resources, t);
    let workspace = of_type("workspace").next();
    let ws_id = workspace.map(|w| str_of(&w["_id"])).unwrap_or_default();
    let mut c = Collection { name: workspace.map(|w| str_of(&w["name"])).unwrap_or_default(), format: "Insomnia v4", ..Default::default() };

    let mut children: HashMap<String, Vec<&Value>> = HashMap::new();
    for r in &resources {
        children.entry(str_of(&r["parentId"])).or_default().push(r);
    }
    fn build(id: &str, name: &str, children: &HashMap<String, Vec<&Value>>, warnings: &mut Vec<String>) -> Folder {
        let mut f = Folder { name: name.into(), ..Default::default() };
        let mut kids = children.get(id).cloned().unwrap_or_default();
        kids.sort_by(|a, b| a["metaSortKey"].as_f64().unwrap_or(0.).total_cmp(&b["metaSortKey"].as_f64().unwrap_or(0.)));
        for k in kids {
            match k["_type"].as_str() {
                Some("request") => f.requests.push(request(k, warnings)),
                Some("request_group") => f.folders.push(build(&str_of(&k["_id"]), &str_of(&k["name"]), children, warnings)),
                _ => {}
            }
        }
        f
    }
    let mut warnings = vec![];
    c.root = build(&ws_id, &c.name, &children, &mut warnings);
    // Base environment → $shared; its sub-environments → named environments.
    for base in of_type("environment").filter(|e| str_of(&e["parentId"]) == ws_id) {
        let shared = env_vars(&base["data"]);
        if !shared.is_empty() {
            c.envs.insert("$shared".into(), shared);
        }
        let base_id = str_of(&base["_id"]);
        for sub in of_type("environment").filter(|e| str_of(&e["parentId"]) == base_id) {
            c.envs.insert(str_of(&sub["name"]), env_vars(&sub["data"]));
        }
    }
    if of_type("request").next().is_none() && of_type("grpc_request").next().is_some() {
        warnings.push("gRPC requests were skipped".into());
    }
    c.warnings = warnings;
    c
}

/// v5 (Insomnia 10+): nested `collection` items, `children` for folders.
pub fn v5(v: &Value) -> Collection {
    let mut c = Collection { name: str_of(&v["name"]), format: "Insomnia v5", ..Default::default() };
    fn build(name: &str, items: &Value, warnings: &mut Vec<String>) -> Folder {
        let mut f = Folder { name: name.into(), ..Default::default() };
        for it in items.as_array().into_iter().flatten() {
            if it["children"].is_array() {
                f.folders.push(build(&str_of(&it["name"]), &it["children"], warnings));
            } else if it["url"].is_string() {
                f.requests.push(request(it, warnings));
            }
        }
        f
    }
    let mut warnings = vec![];
    c.root = build(&c.name, &v["collection"], &mut warnings);
    let env = &v["environments"];
    let shared = env_vars(&env["data"]);
    if !shared.is_empty() {
        c.envs.insert("$shared".into(), shared);
    }
    for sub in env["subEnvironments"].as_array().into_iter().flatten() {
        c.envs.insert(str_of(&sub["name"]), env_vars(&sub["data"]));
    }
    c.warnings = warnings;
    c
}
