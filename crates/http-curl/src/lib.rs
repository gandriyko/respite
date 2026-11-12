//! cURL command ↔ `.http` request block.
//!
//! Import understands bash/zsh quoting (incl. `$'…'`), Windows cmd (`^` escapes and line
//! continuations, Chrome's "Copy as cURL (cmd)") and PowerShell backtick continuations.

mod tokenize;

use http_exec::SentRequest;
use http_syntax::Request;
use serde_json::Value;

pub use tokenize::tokenize;

pub struct Options<'a> {
    /// Keep the original command as a comment above the block.
    pub keep_original: bool,
    /// Without `-L` add `# @no-redirect` (curl does not follow redirects by default).
    pub no_redirect_without_l: bool,
    /// Header name prefixes to drop (browser noise).
    pub strip_headers: &'a [&'a str],
    /// `(name, value)`: values found in the URL and header values become `{{name}}`.
    pub vars: &'a [(String, String)],
}

impl Default for Options<'_> {
    fn default() -> Self {
        Self { keep_original: true, no_redirect_without_l: true, strip_headers: NOISE, vars: &[] }
    }
}

pub const NOISE: &[&str] = &["sec-ch-ua", "sec-fetch-", "priority"];

/// Text (after trimming) starts with `curl ` / `curl.exe `.
pub fn is_curl(text: &str) -> bool {
    let t = text.trim_start();
    ["curl ", "curl.exe ", "curl\t", "curl.exe\t"].iter().any(|p| t.starts_with(p))
}

/// Every `curl …` command in `text` (one per logical line) → one block each.
pub fn import_all(text: &str, opts: &Options) -> Result<Vec<String>, String> {
    let joined = tokenize::join_continuations(text);
    let mut commands: Vec<String> = vec![];
    for line in joined.lines() {
        if is_curl(line) {
            commands.push(line.trim().to_string());
        } else if let Some(last) = commands.last_mut()
            && !line.trim().is_empty()
        {
            // e.g. a quoted multi-line body
            last.push('\n');
            last.push_str(line);
        }
    }
    if commands.is_empty() {
        return Err("no curl command found".into());
    }
    commands.iter().map(|c| import(c, opts)).collect()
}

#[derive(Default)]
struct Parsed {
    method: Option<String>,
    url: Option<String>,
    headers: Vec<(String, String)>,
    data: Vec<Data>,
    form: Vec<String>,
    json: bool,
    get: bool,
    head: bool,
    follow: bool,
    insecure: bool,
    compressed: bool,
    version: Option<&'static str>,
    max_time: Option<String>,
    connect_timeout: Option<String>,
    output: Option<String>,
    ignored: Vec<String>,
}

enum Data {
    /// `-d`, `--data-binary`, `--data-ascii`: `@file` reads a file.
    Text(String),
    /// `--data-raw`: literal.
    Raw(String),
    UrlEncode(String),
}

/// Flags that take a value we deliberately ignore.
const IGNORED_WITH_VALUE: &[&str] = &[
    "-x", "--proxy", "--cacert", "--capath", "--cert", "-E", "--key", "-w", "--write-out", "--retry",
    "--resolve", "-c", "--cookie-jar", "-D", "--dump-header", "--limit-rate", "--max-redirs", "-K",
    "--config", "-U", "--proxy-user", "--interface", "-r", "--range", "-z", "--time-cond", "--user-agent-file",
];
/// Output/verbosity flags with no effect on the request.
const NOISY: &[&str] = &[
    "-s", "--silent", "-S", "--show-error", "-v", "--verbose", "-i", "--include", "-f", "--fail", "-#",
    "--progress-bar", "-N", "--no-buffer", "-g", "--globoff", "--fail-with-body", "-n", "--netrc",
];
const WITH_VALUE: &[&str] = &[
    "-X", "--request", "-H", "--header", "-d", "--data", "--data-raw", "--data-binary", "--data-ascii",
    "--data-urlencode", "--json", "-F", "--form", "--form-string", "-u", "--user", "-b", "--cookie", "-A",
    "--user-agent", "-e", "--referer", "-m", "--max-time", "--connect-timeout", "-o", "--output", "--url",
];

/// One curl command → one request block.
pub fn import(command: &str, opts: &Options) -> Result<String, String> {
    let tokens = tokenize(command)?;
    let mut it = tokens.iter().skip_while(|t| !t.starts_with("curl")).skip(1).peekable();
    let mut p = Parsed::default();
    while let Some(tok) = it.next() {
        // --flag=value, -Xvalue, -sSL
        let (flag, inline) = if let Some(rest) = tok.strip_prefix("--") {
            match rest.split_once('=') {
                Some((f, v)) if WITH_VALUE.contains(&format!("--{f}").as_str()) => (format!("--{f}"), Some(v.to_string())),
                _ => (tok.clone(), None),
            }
        } else if tok.starts_with('-') && tok.len() > 2 && !tok.starts_with("--") {
            let short = &tok[..2];
            if WITH_VALUE.contains(&short) || IGNORED_WITH_VALUE.contains(&short) {
                (short.to_string(), Some(tok[2..].to_string()))
            } else {
                // bundle of boolean flags: -sSLk
                for c in tok[1..].chars() {
                    apply_bool(&mut p, &format!("-{c}"));
                }
                continue;
            }
        } else {
            (tok.clone(), None)
        };
        let f = flag.as_str();
        if !f.starts_with('-') || f == "-" {
            p.url.get_or_insert_with(|| tok.clone());
            continue;
        }
        if WITH_VALUE.contains(&f) || IGNORED_WITH_VALUE.contains(&f) {
            let value = match inline {
                Some(v) => v,
                None => it.next().cloned().ok_or_else(|| format!("{f} needs a value"))?,
            };
            apply_value(&mut p, f, value);
        } else {
            apply_bool(&mut p, f);
        }
    }
    let url = p.url.clone().ok_or("no URL in curl command")?;
    Ok(build(&p, &url, command, opts))
}

fn apply_bool(p: &mut Parsed, f: &str) {
    match f {
        "-G" | "--get" => p.get = true,
        "-I" | "--head" => p.head = true,
        "-L" | "--location" | "--location-trusted" => p.follow = true,
        "-k" | "--insecure" => p.insecure = true,
        "--compressed" => p.compressed = true,
        "--http2" | "--http2-prior-knowledge" => p.version = Some("HTTP/2"),
        "--http1.1" => p.version = Some("HTTP/1.1"),
        f if NOISY.contains(&f) => {}
        f => p.ignored.push(f.to_string()),
    }
}

fn apply_value(p: &mut Parsed, f: &str, v: String) {
    match f {
        "-X" | "--request" => p.method = Some(v.to_ascii_uppercase()),
        "--url" => p.url = Some(v),
        "-H" | "--header" => {
            if let Some((n, val)) = v.split_once(':') {
                p.headers.push((n.trim().to_string(), val.trim().to_string()));
            }
        }
        "-d" | "--data" | "--data-binary" | "--data-ascii" => p.data.push(Data::Text(v)),
        "--data-raw" => p.data.push(Data::Raw(v)),
        "--data-urlencode" => p.data.push(Data::UrlEncode(v)),
        "--json" => {
            p.json = true;
            p.data.push(Data::Text(v));
        }
        "-F" | "--form" | "--form-string" => p.form.push(v),
        "-u" | "--user" => {
            let (user, pass) = v.split_once(':').unwrap_or((&v, ""));
            p.headers.push(("Authorization".into(), format!("Basic {user} {pass}").trim_end().to_string()));
        }
        "-b" | "--cookie" if v.contains('=') => p.headers.push(("Cookie".into(), v)),
        "-A" | "--user-agent" => p.headers.push(("User-Agent".into(), v)),
        "-e" | "--referer" => p.headers.push(("Referer".into(), v)),
        "-m" | "--max-time" => p.max_time = Some(v),
        "--connect-timeout" => p.connect_timeout = Some(v),
        "-o" | "--output" => p.output = Some(v),
        // -b <cookie file>, --proxy, --cacert, …
        _ => p.ignored.push(f.to_string()),
    }
}

fn has(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

fn build(p: &Parsed, url: &str, original: &str, opts: &Options) -> String {
    let mut headers: Vec<(String, String)> = p
        .headers
        .iter()
        .filter(|(n, _)| !opts.strip_headers.iter().any(|s| n.to_ascii_lowercase().starts_with(s)))
        .cloned()
        .collect();

    // Body pieces joined like curl: several -d → `a&b`; `-d @file` alone → `< ./file`.
    let mut file_body = None;
    let pieces: Vec<String> = p
        .data
        .iter()
        .filter_map(|d| match d {
            Data::Text(s) if s.starts_with('@') && p.data.len() == 1 => {
                file_body = Some(s[1..].to_string());
                None
            }
            Data::Text(s) | Data::Raw(s) => Some(s.clone()),
            Data::UrlEncode(s) => Some(match s.split_once('=') {
                Some((name, value)) if !name.is_empty() => format!("{name}={}", pct(value)),
                Some((_, value)) => pct(value),
                None => pct(s),
            }),
        })
        .collect();
    let mut data = pieces.join("&");

    let mut url = url.to_string();
    if p.get && !data.is_empty() {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&std::mem::take(&mut data));
    }
    let has_body = !data.is_empty() || file_body.is_some() || !p.form.is_empty();
    let method = p.method.clone().unwrap_or_else(|| {
        if p.head { "HEAD" } else if has_body { "POST" } else { "GET" }.to_string()
    });

    if p.json {
        for (n, v) in [("Content-Type", "application/json"), ("Accept", "application/json")] {
            if !has(&headers, n) {
                headers.push((n.into(), v.into()));
            }
        }
    }
    const BOUNDARY: &str = "RespiteFormBoundary";
    if !p.form.is_empty() {
        headers.retain(|(n, _)| !n.eq_ignore_ascii_case("content-type"));
        headers.push(("Content-Type".into(), format!("multipart/form-data; boundary={BOUNDARY}")));
    } else if has_body && !has(&headers, "content-type") {
        headers.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
    }
    if p.compressed && !has(&headers, "accept-encoding") {
        headers.push(("Accept-Encoding".into(), "gzip, deflate, br".into()));
    }

    let content_type = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.to_ascii_lowercase());
    let body = if !p.form.is_empty() {
        Some(multipart(&p.form, BOUNDARY))
    } else if let Some(f) = file_body {
        Some(format!("< {}", local_path(&f)))
    } else if data.is_empty() {
        None
    } else if let Ok(v) = serde_json::from_str::<Value>(&data)
        && (v.is_object() || v.is_array())
    {
        Some(serde_json::to_string_pretty(&v).unwrap_or(data))
    } else if content_type.is_some_and(|c| c.starts_with("application/x-www-form-urlencoded")) {
        Some(form_lines(&data).replace('&', "\n&"))
    } else {
        Some(data)
    };

    // Variables: longest values first so `baseUrl` wins over `host`.
    let mut vars: Vec<&(String, String)> = opts.vars.iter().filter(|(_, v)| v.len() >= 4).collect();
    vars.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let subst = |s: &str| vars.iter().fold(s.to_string(), |acc, (n, v)| acc.replace(v.as_str(), &format!("{{{{{n}}}}}")));

    let (base, query) = url.split_once('?').map_or((url.as_str(), None), |(b, q)| (b, Some(q)));
    let path = base.split("://").nth(1).and_then(|r| r.find('/').map(|i| &r[i..])).unwrap_or("/");

    let mut out = vec![format!("### {method} {path}")];
    if opts.keep_original {
        let flat: String = tokenize::join_continuations(original).split_whitespace().collect::<Vec<_>>().join(" ");
        out.push(match flat.char_indices().nth(100) {
            Some((i, _)) => format!("# {}… (original truncated)", &flat[..i]),
            None => format!("# {flat}"),
        });
    }
    if !p.ignored.is_empty() {
        out.push(format!("# ignored: {}", p.ignored.join(", ")));
    }
    if !p.follow && opts.no_redirect_without_l {
        out.push("# @no-redirect".into());
    }
    if p.insecure {
        out.push("# @insecure".into());
    }
    if let Some(t) = &p.max_time {
        out.push(format!("# @timeout {t} s"));
    }
    if let Some(t) = &p.connect_timeout {
        out.push(format!("# @connection-timeout {t} s"));
    }
    let version = p.version.map(|v| format!(" {v}")).unwrap_or_default();
    let params: Vec<&str> = query.map(|q| q.split('&').collect()).unwrap_or_default();
    if params.len() > 2 {
        out.push(format!("{method} {}{version}", subst(base)));
        for (i, kv) in params.iter().enumerate() {
            out.push(format!("    {}{}", if i == 0 { '?' } else { '&' }, subst(kv)));
        }
    } else {
        out.push(format!("{method} {}{version}", subst(&url)));
    }
    for (n, v) in &headers {
        out.push(format!("{n}: {}", subst(v)));
    }
    if let Some(b) = body {
        out.push(String::new());
        out.push(b);
    }
    if let Some(o) = &p.output {
        out.push(String::new());
        out.push(format!(">> {}", local_path(o)));
    }
    out.join("\n") + "\n"
}

/// Form bodies span lines in `.http` (`a=1⏎&b=2`) but are sent joined, as the executor does.
fn form_lines(s: &str) -> String {
    s.lines().map(str::trim).collect()
}

/// `name=value`, `name=@file;type=x/y` parts.
fn multipart(parts: &[String], boundary: &str) -> String {
    let mut out = vec![];
    for part in parts {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        out.push(format!("--{boundary}"));
        if let Some(file) = value.strip_prefix('@') {
            let mut attrs = file.split(';');
            let path = attrs.next().unwrap_or("");
            let filename = path.rsplit(['/', '\\']).next().unwrap_or(path);
            out.push(format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\""));
            if let Some(t) = attrs.find_map(|a| a.strip_prefix("type=")) {
                out.push(format!("Content-Type: {t}"));
            }
            out.push(String::new());
            out.push(format!("< {}", local_path(path)));
        } else {
            out.push(format!("Content-Disposition: form-data; name=\"{name}\""));
            out.push(String::new());
            out.push(value.to_string());
        }
    }
    out.push(format!("--{boundary}--"));
    out.join("\n")
}

/// Relative paths get `./` like the rest of the `.http` syntax.
fn local_path(p: &str) -> String {
    if p.starts_with(['/', '.', '~']) || p.contains(":\\") { p.to_string() } else { format!("./{p}") }
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Shell-quote for bash: bare when safe, else single quotes.
fn quote(s: &str) -> String {
    let safe = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_@%+=:,./".contains(c));
    if safe { s.to_string() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// Resolved request (variables substituted) → curl.
pub fn export(sent: &SentRequest, req: &Request) -> String {
    let body = (!sent.body.is_empty()).then(|| match String::from_utf8(sent.body.clone()) {
        Ok(text) => ("--data-raw", text),
        // ponytail: non-UTF-8 bodies are exported lossily; curl would need the bytes in a file.
        Err(e) => ("--data-binary", String::from_utf8_lossy(e.as_bytes()).into_owned()),
    });
    render(&sent.method, &sent.url, &sent.headers, body, req)
}

/// Block as written, `{{variables}}` kept; `< file` bodies become `--data-binary @file`.
pub fn export_raw(req: &Request) -> String {
    let headers: Vec<(String, String)> = req.headers.iter().map(|h| (h.name.clone(), h.value.clone())).collect();
    let form = headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("content-type") && v.to_ascii_lowercase().starts_with("application/x-www-form-urlencoded"));
    let body = req.body.as_ref().map(|b| {
        let single = b.text.trim();
        match single.strip_prefix("<@").or_else(|| single.strip_prefix("< ")) {
            Some(path) if !single.contains('\n') => ("--data-binary", format!("@{}", path.trim())),
            _ if form => ("--data-raw", form_lines(&b.text)),
            _ => ("--data-raw", b.text.clone()),
        }
    });
    render(&req.method, &req.url, &headers, body, req)
}

fn render(method: &str, url: &str, headers: &[(String, String)], body: Option<(&str, String)>, req: &Request) -> String {
    let mut args = vec!["curl".to_string()];
    match method {
        "GET" => {}
        "HEAD" => args.push("--head".into()),
        m => args.push(format!("-X {}", quote(m))),
    }
    args.push(quote(url));
    if req.directive("no-redirect").is_none() {
        args.push("-L".into());
    }
    if req.directive("insecure").is_some() {
        args.push("-k".into());
    }
    if req.http_version.as_deref() == Some("HTTP/2") {
        args.push("--http2".into());
    }
    for (directive, flag) in [("timeout", "--max-time"), ("connection-timeout", "--connect-timeout")] {
        if let Some(secs) = req.directive(directive).and_then(|d| seconds(d.value.as_deref()?)) {
            args.push(format!("{flag} {secs}"));
        }
    }
    for (n, v) in headers {
        args.push(format!("-H {}", quote(&format!("{n}: {v}"))));
    }
    if let Some((flag, b)) = body {
        args.push(format!("{flag} {}", quote(&b)));
    }
    args.join(" \\\n  ")
}

/// `10 s` / `500 ms` / `2 m` / `10` → seconds for curl.
fn seconds(v: &str) -> Option<String> {
    let end = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let n: f64 = v[..end].parse().ok()?;
    let s = match v[end..].trim() {
        "" | "s" => n,
        "ms" => n / 1000.,
        "m" => n * 60.,
        _ => return None,
    };
    Some(s.to_string())
}

#[cfg(test)]
mod tests;
