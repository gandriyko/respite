//! OpenAPI 3.x and Swagger 2.0: one request per operation, one file per tag.

use serde_json::{Value, json};

use crate::{Collection, Folder, Req, scalar};

const METHODS: [&str; 8] = ["get", "post", "put", "patch", "delete", "head", "options", "trace"];

pub fn spec(v: &Value) -> Collection {
    let swagger2 = v["swagger"].is_string();
    let title = v["info"]["title"].as_str().unwrap_or_default().to_string();
    let mut c = Collection { name: title.clone(), format: if swagger2 { "Swagger 2" } else { "OpenAPI 3" }, ..Default::default() };

    let base_url = if swagger2 {
        let scheme = v["schemes"][0].as_str().unwrap_or("https");
        let host = v["host"].as_str().unwrap_or("localhost");
        format!("{scheme}://{host}{}", v["basePath"].as_str().unwrap_or("").trim_end_matches('/'))
    } else {
        let url = v["servers"][0]["url"].as_str().unwrap_or("http://localhost").trim_end_matches('/').to_string();
        // Server variables: `{port}` → its default.
        v["servers"][0]["variables"].as_object().into_iter().flatten().fold(url, |u, (k, var)| {
            u.replace(&format!("{{{k}}}"), var["default"].as_str().unwrap_or_default())
        })
    };
    c.envs.insert("$shared".into(), vec![("baseUrl".into(), base_url)]);

    // One folder (→ file) per first tag, in order of first appearance.
    let mut folders: Vec<Folder> = vec![];
    let mut needs_token = false;
    for (path, item) in v["paths"].as_object().into_iter().flatten() {
        for method in METHODS {
            let op = &item[method];
            if !op.is_object() {
                continue;
            }
            // Path-level parameters apply to every operation of the path.
            let params: Vec<Value> = item["parameters"].as_array().into_iter().flatten().chain(op["parameters"].as_array().into_iter().flatten()).map(|p| resolve(v, p).clone()).collect();
            let req = operation(v, swagger2, path, method, op, &params, &mut needs_token, &mut c.warnings);
            // Untagged operations: "default", as Swagger UI groups them.
            let tag = op["tags"][0].as_str().unwrap_or("default").to_string();
            match folders.iter_mut().find(|f| f.name == tag) {
                Some(f) => f.requests.push(req),
                None => folders.push(Folder { name: tag, requests: vec![req], ..Default::default() }),
            }
        }
    }
    if needs_token {
        c.private_envs.insert("$shared".into(), vec![("token".into(), String::new())]);
    }
    c.root = Folder { name: title, folders, ..Default::default() };
    c
}

/// Follow a local `$ref` (`#/components/…`, `#/definitions/…`).
fn resolve<'a>(root: &'a Value, v: &'a Value) -> &'a Value {
    let mut cur = v;
    for _ in 0..16 {
        let Some(r) = cur["$ref"].as_str() else { return cur };
        let Some(ptr) = r.strip_prefix('#') else { return cur };
        match root.pointer(ptr) {
            Some(next) => cur = next,
            None => return cur,
        }
    }
    cur
}

#[allow(clippy::too_many_arguments)]
fn operation(
    root: &Value,
    swagger2: bool,
    path: &str,
    method: &str,
    op: &Value,
    params: &[Value],
    needs_token: &mut bool,
    warnings: &mut Vec<String>,
) -> Req {
    let name = op["summary"].as_str().or(op["operationId"].as_str()).map(str::to_string).unwrap_or_else(|| format!("{} {path}", method.to_uppercase()));
    // `/users/{id}` → `/users/{{id}}`
    let mut url = format!("{{{{baseUrl}}}}{}", path.replace('{', "{{").replace('}', "}}"));
    let mut req = Req { name, method: method.to_uppercase(), ..Default::default() };
    if let Some(d) = op["description"].as_str().filter(|d| !d.trim().is_empty()) {
        req.notes.push(d.lines().next().unwrap_or_default().to_string());
    }
    if op["deprecated"] == true {
        req.notes.push("deprecated".into());
    }

    let mut query = vec![];
    let mut form = vec![];
    for p in params {
        let name = p["name"].as_str().unwrap_or_default();
        let required = p["required"] == true;
        let example = [&p["example"], &p["schema"]["example"], &p["default"], &p["schema"]["default"]].into_iter().find_map(scalar);
        // Example inline; without one, a `{{name}}` variable declared as `@name =` in the file.
        // Path parameters are always variables (`/pets/{{id}}`).
        let value = |req: &mut Req, inline: bool| match example.clone() {
            Some(e) if inline => e,
            e => {
                req.vars.push((name.into(), e.unwrap_or_default()));
                format!("{{{{{name}}}}}")
            }
        };
        match p["in"].as_str() {
            Some("path") => _ = value(&mut req, false),
            Some("query") if required => query.push(format!("{name}={}", value(&mut req, true))),
            Some("query") => req.notes.push(format!("optional query: {name}")),
            Some("header") if required => {
                let v = value(&mut req, true);
                req.headers.push((name.into(), v));
            }
            Some("body") if swagger2 => {
                req.default_header("Content-Type", "application/json");
                req.body = Some(sample_json(root, &p["schema"]));
            }
            Some("formData") => form.push(format!("{name}={}", value(&mut req, true))),
            _ => {}
        }
    }
    // Path placeholders the spec forgot to declare as parameters.
    for seg in path.split('{').skip(1) {
        if let Some((name, _)) = seg.split_once('}')
            && !req.vars.iter().any(|(k, _)| k == name)
        {
            req.vars.push((name.into(), String::new()));
        }
    }
    if !query.is_empty() {
        url += &format!("?{}", query.join("&"));
    }
    req.url = url;
    if !form.is_empty() {
        req.default_header("Content-Type", "application/x-www-form-urlencoded");
        req.body = Some(form.join("\n&"));
    }

    // OpenAPI 3 request body: first JSON-ish content type, else the first one.
    let body = resolve(root, &op["requestBody"]);
    // `"content": {}` declares no body at all.
    if let Some(content) = body["content"].as_object()
        && let Some((ct, media)) = content.iter().find(|(k, _)| k.contains("json")).or_else(|| content.iter().next())
    {
        req.default_header("Content-Type", ct.clone());
        let example = media["example"].clone();
        let example = if example.is_null() { media["examples"].as_object().and_then(|m| m.values().next()).map(|e| resolve(root, e)["value"].clone()).unwrap_or_default() } else { example };
        req.body = Some(if !example.is_null() {
            serde_json::to_string_pretty(&example).unwrap_or_default()
        } else if ct.contains("json") {
            sample_json(root, &media["schema"])
        } else {
            String::new()
        });
    }

    // Security: bearer / basic / API key headers with variables.
    let security = op["security"].as_array().or(root["security"].as_array());
    let schemes = if swagger2 { &root["securityDefinitions"] } else { &root["components"]["securitySchemes"] };
    // First requirement is enough.
    if let Some(name) = security.into_iter().flatten().find_map(|s| s.as_object()?.keys().next().cloned()) {
        let s = resolve(root, &schemes[&name]);
        match (s["type"].as_str(), s["scheme"].as_str().map(str::to_ascii_lowercase).as_deref(), s["in"].as_str()) {
            (Some("http"), Some("bearer"), _) | (Some("oauth2" | "openIdConnect"), _, _) => {
                *needs_token = true;
                req.default_header("Authorization", "Bearer {{token}}");
            }
            (Some("http" | "basic"), Some("basic") | None, _) if s["type"] != "apiKey" => {
                req.default_header("Authorization", "Basic {{username}} {{password}}");
            }
            (Some("apiKey"), _, Some("header")) => {
                let header = s["name"].as_str().unwrap_or("X-Api-Key");
                req.default_header(header, "{{apiKey}}");
            }
            (Some("apiKey"), _, Some("query")) => {
                let key = s["name"].as_str().unwrap_or("api_key");
                req.url += &format!("{}{key}={{{{apiKey}}}}", if req.url.contains('?') { "&" } else { "?" });
            }
            _ => warnings.push(format!("{}: security scheme '{name}' was not converted", req.name)),
        }
    }
    req
}

/// Example JSON for a schema: `example` / `default` / enum first value, else a typed placeholder.
fn sample_json(root: &Value, schema: &Value) -> String {
    serde_json::to_string_pretty(&sample(root, schema, 0)).unwrap_or_default()
}

fn sample(root: &Value, schema: &Value, depth: usize) -> Value {
    let s = resolve(root, schema);
    if depth > 6 {
        return Value::Null;
    }
    for key in ["example", "default"] {
        if !s[key].is_null() {
            return s[key].clone();
        }
    }
    if let Some(first) = s["enum"].get(0) {
        return first.clone();
    }
    if let Some(all) = s["allOf"].as_array() {
        let mut obj = serde_json::Map::new();
        for part in all {
            if let Value::Object(m) = sample(root, part, depth + 1) {
                obj.extend(m);
            }
        }
        return Value::Object(obj);
    }
    if let Some(first) = s["oneOf"].get(0).or(s["anyOf"].get(0)) {
        return sample(root, first, depth + 1);
    }
    match s["type"].as_str() {
        Some("object") | None if s["properties"].is_object() => {
            Value::Object(s["properties"].as_object().unwrap().iter().map(|(k, p)| (k.clone(), sample(root, p, depth + 1))).collect())
        }
        Some("array") => json!([sample(root, &s["items"], depth + 1)]),
        Some("string") => match s["format"].as_str() {
            Some("date-time") => json!("2024-01-01T00:00:00Z"),
            Some("date") => json!("2024-01-01"),
            Some("uuid") => json!("{{$uuid}}"),
            Some("email") => json!("user@example.com"),
            _ => json!("string"),
        },
        Some("integer") => json!(0),
        Some("number") => json!(0.0),
        Some("boolean") => json!(true),
        _ => json!({}),
    }
}
