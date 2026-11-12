//! HAR (DevTools "Save all as HAR"): every entry becomes a request in one file.

use serde_json::Value;

use crate::{Collection, Folder, Req, form_body, pretty_json};

/// Browser headers that are noise in a hand-edited request.
fn noisy(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with(':') || n.starts_with("sec-ch-ua") || n.starts_with("sec-fetch-") || ["priority", "content-length", "host", "connection"].contains(&n.as_str())
}

pub fn log(v: &Value) -> Collection {
    let mut root = Folder { name: "har".into(), ..Default::default() };
    for e in v["log"]["entries"].as_array().into_iter().flatten() {
        let r = &e["request"];
        let url = r["url"].as_str().unwrap_or_default().to_string();
        let path = url.split("://").nth(1).and_then(|s| s.find('/').map(|i| s[i..].split('?').next().unwrap_or("/").to_string())).unwrap_or_else(|| "/".into());
        let method = r["method"].as_str().unwrap_or("GET").to_string();
        let mut req = Req { name: format!("{method} {path}"), method, url, ..Default::default() };
        if let Some(status) = e["response"]["status"].as_u64() {
            req.notes.push(format!("recorded response: {status}"));
        }
        for h in r["headers"].as_array().into_iter().flatten() {
            if let (Some(n), Some(val)) = (h["name"].as_str(), h["value"].as_str())
                && !noisy(n)
            {
                req.headers.push((n.into(), val.into()));
            }
        }
        let post = &r["postData"];
        let mime = post["mimeType"].as_str().unwrap_or_default();
        if let Some(params) = post["params"].as_array().filter(|p| !p.is_empty() && mime.starts_with("application/x-www-form-urlencoded")) {
            let pairs: Vec<(String, String)> = params.iter().filter_map(|p| Some((p["name"].as_str()?.into(), p["value"].as_str().unwrap_or_default().into()))).collect();
            req.body = Some(form_body(&pairs));
        } else if let Some(text) = post["text"].as_str().filter(|t| !t.is_empty()) {
            req.body = Some(if mime.contains("json") { pretty_json(text) } else { text.into() });
        }
        if req.body.is_some() && !mime.is_empty() {
            req.default_header("Content-Type", mime);
        }
        root.requests.push(req);
    }
    Collection { name: "har-import".into(), format: "HAR", root, ..Default::default() }
}
