//! Bruno collection folder: `bruno.json`, `*.bru` requests (folders = sub-directories),
//! `environments/*.bru`.

use std::path::Path;

use crate::{Collection, Folder, Req, form_body, graphql, multipart, pretty_json};

/// Top-level `.bru` blocks: `name {` … `}` at column 0, contents dedented by two spaces.
fn blocks(text: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(name) = line.strip_suffix('{').map(str::trim).filter(|n| !n.is_empty() && !line.starts_with(' ')) else { continue };
        let mut body = vec![];
        for l in lines.by_ref() {
            if l == "}" {
                break;
            }
            body.push(l.strip_prefix("  ").unwrap_or(l));
        }
        out.push((name.to_string(), body.join("\n")));
    }
    out
}

/// `key: value` lines; `~key` (disabled) is skipped.
fn pairs(body: &str) -> Vec<(String, String)> {
    body.lines()
        .filter(|l| !l.trim_start().starts_with('~'))
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn get<'a>(blocks: &'a [(String, String)], name: &str) -> Option<&'a str> {
    blocks.iter().find(|(n, _)| n == name).map(|(_, b)| b.as_str())
}

fn request(text: &str, file_stem: &str, warnings: &mut Vec<String>) -> Option<(i64, Req)> {
    let b = blocks(text);
    let meta = pairs(get(&b, "meta").unwrap_or_default());
    let meta_get = |k: &str| meta.iter().find(|(m, _)| m == k).map(|(_, v)| v.clone());
    let method = ["get", "post", "put", "patch", "delete", "options", "head", "connect", "trace"].into_iter().find(|m| get(&b, m).is_some());
    let (method, spec) = match (method, get(&b, "graphql")) {
        (Some(m), _) => (m.to_uppercase(), pairs(get(&b, m).unwrap())),
        (None, Some(g)) => ("POST".into(), pairs(g)),
        (None, None) => return None,
    };
    let spec_get = |k: &str| spec.iter().find(|(s, _)| s == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let mut req = Req { name: meta_get("name").unwrap_or_else(|| file_stem.into()), method, url: spec_get("url"), ..Default::default() };
    req.headers = pairs(get(&b, "headers").unwrap_or_default());

    let query = pairs(get(&b, "params:query").or(get(&b, "query")).unwrap_or_default());
    if !query.is_empty() && !req.url.contains('?') {
        req.url += &format!("?{}", query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"));
    }

    match spec_get("auth").as_str() {
        "bearer" => {
            let token = pairs(get(&b, "auth:bearer").unwrap_or_default()).into_iter().find(|(k, _)| k == "token").map(|(_, v)| v).unwrap_or_default();
            req.default_header("Authorization", format!("Bearer {token}"));
        }
        "basic" => {
            let p = pairs(get(&b, "auth:basic").unwrap_or_default());
            let f = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            req.default_header("Authorization", format!("Basic {} {}", f("username"), f("password")));
        }
        "apikey" => {
            let p = pairs(get(&b, "auth:apikey").unwrap_or_default());
            let f = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            req.default_header(&f("key"), f("value"));
        }
        "" | "none" | "inherit" => {}
        other => {
            req.notes.push(format!("Bruno auth '{other}' was not converted"));
            warnings.push(format!("{}: auth '{other}' was not converted", req.name));
        }
    }

    match spec_get("body").as_str() {
        "json" => {
            req.default_header("Content-Type", "application/json");
            req.body = get(&b, "body:json").map(pretty_json);
        }
        "xml" => {
            req.default_header("Content-Type", "application/xml");
            req.body = get(&b, "body:xml").map(str::to_string);
        }
        "text" => req.body = get(&b, "body:text").map(str::to_string),
        "formUrlEncoded" => {
            req.default_header("Content-Type", "application/x-www-form-urlencoded");
            req.body = Some(form_body(&pairs(get(&b, "body:form-urlencoded").unwrap_or_default())));
        }
        "multipartForm" => {
            let parts: Vec<(String, Result<String, String>)> = pairs(get(&b, "body:multipart-form").unwrap_or_default())
                .into_iter()
                .map(|(k, v)| match v.strip_prefix("@file(").and_then(|r| r.strip_suffix(')')) {
                    Some(path) => (k, Err(path.split('|').next().unwrap_or(path).to_string())),
                    None => (k, Ok(v)),
                })
                .collect();
            multipart(&mut req, &parts);
        }
        "graphql" => {
            graphql(&mut req, get(&b, "body:graphql").unwrap_or_default(), get(&b, "body:graphql:vars"));
            req.default_header("Content-Type", "application/json");
        }
        _ => {}
    }

    if b.iter().any(|(n, body)| (n.starts_with("script:") || n == "tests") && !body.trim().is_empty()) {
        req.notes.push("Bruno scripts / tests were not converted".into());
        warnings.push(format!("{}: scripts were not converted", req.name));
    }
    let seq = meta_get("seq").and_then(|s| s.parse().ok()).unwrap_or(i64::MAX);
    Some((seq, req))
}

fn folder(dir: &Path, name: &str, warnings: &mut Vec<String>) -> Folder {
    let mut f = Folder { name: name.into(), ..Default::default() };
    let mut entries: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
    entries.sort();
    let mut requests = vec![];
    for p in entries {
        let file_name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if p.is_dir() {
            if file_name != "environments" && !file_name.starts_with('.') && file_name != "node_modules" {
                let sub = folder(&p, &file_name, warnings);
                if !sub.requests.is_empty() || !sub.folders.is_empty() {
                    f.folders.push(sub);
                }
            }
        } else if p.extension().is_some_and(|e| e == "bru") && file_name != "folder.bru" && file_name != "collection.bru" {
            let stem = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            match std::fs::read_to_string(&p) {
                Ok(text) => requests.extend(request(&text, &stem, warnings)),
                Err(e) => warnings.push(format!("{}: {e}", p.display())),
            }
        }
    }
    requests.sort_by_key(|(seq, _)| *seq);
    f.requests = requests.into_iter().map(|(_, r)| r).collect();
    f
}

pub fn collection(dir: &Path) -> Result<Collection, String> {
    let meta: serde_json::Value = std::fs::read_to_string(dir.join("bruno.json"))
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        .map_err(|e| format!("bruno.json: {e}"))?;
    let name = meta["name"].as_str().map(str::to_string).unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().into_owned());
    let mut c = Collection { name: name.clone(), format: "Bruno", ..Default::default() };
    let mut warnings = vec![];
    c.root = folder(dir, &name, &mut warnings);
    // environments/<env>.bru: `vars { k: v }`, secrets listed in `vars:secret [ … ]`.
    let mut env_files: Vec<_> = std::fs::read_dir(dir.join("environments")).into_iter().flatten().flatten().map(|e| e.path()).collect();
    env_files.sort();
    for p in env_files.into_iter().filter(|p| p.extension().is_some_and(|e| e == "bru")) {
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let env = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        c.envs.insert(env.clone(), pairs(get(&blocks(&text), "vars").unwrap_or_default()));
        let secrets: Vec<(String, String)> = text
            .split_once("vars:secret [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(list, _)| list.split([',', '\n']).map(|s| s.trim().trim_start_matches('~')).filter(|s| !s.is_empty()).map(|s| (s.to_string(), String::new())).collect())
            .unwrap_or_default();
        if !secrets.is_empty() {
            warnings.push(format!("{env}: secret values are not exported by Bruno; fill them in respite-{}.private.json", crate::sanitize(&env)));
            c.private_envs.insert(env, secrets);
        }
    }
    c.warnings = warnings;
    Ok(c)
}
