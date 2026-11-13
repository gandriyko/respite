//! Error-tolerant parser for `.http` / `.rest` files
//! (VS Code REST Client + JetBrains HTTP Client syntax; VS Code wins on conflicts).
//!
//! All line numbers are 1-based. Parsing never fails: problems go to `diagnostics`.

use std::collections::HashMap;
use std::ops::RangeInclusive;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpFile {
    pub variables: Vec<FileVariable>,
    pub requests: Vec<Request>,
    pub diagnostics: Vec<Diagnostic>,
}

/// `@name = value`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVariable {
    pub name: String,
    pub value: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Text after `###`.
    pub title: Option<String>,
    /// From `# @name`.
    pub name: Option<String>,
    pub directives: Vec<Directive>,
    pub method: String,
    /// URL with multiline `?` / `&` continuations joined, variables unresolved.
    pub url: String,
    pub http_version: Option<String>,
    pub headers: Vec<Header>,
    /// Raw body (may contain `< file`, multipart parts, `{{vars}}`); trailing blank lines trimmed.
    pub body: Option<Body>,
    /// `>> path` / `>>! path`
    pub output: Option<Output>,
    /// `< {% %}` / `> {% %}` / `> handler.js` present (not executed in v1).
    pub has_scripts: bool,
    /// Whole block, including its `###` line.
    pub lines: RangeInclusive<usize>,
    pub request_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    pub name: String,
    pub value: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    pub text: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub path: String,
    pub overwrite: bool,
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
}

pub const DIRECTIVES: &[&str] = &[
    "name",
    "prompt",
    "note",
    "no-redirect",
    "no-cookie-jar",
    "no-log",
    "no-auto-encoding",
    "timeout",
    "connection-timeout",
    "insecure",
    "assert",
];

/// `# @assert <subject> <operator> [expected]`; subjects and operators follow httpyac's `??`.
/// Kept in a comment so VS Code REST Client and JetBrains still run the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assert {
    pub subject: Subject,
    pub op: String,
    /// Raw, may hold `{{vars}}`; empty for unary operators.
    pub expected: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    Status,
    Header(String),
    /// Whole body as text, or a JSONPath (`$.a[0]`; a bare `a.b` means `$.a.b`).
    Body(Option<String>),
    /// Total time in ms; the expected value may carry `ms` / `s` / `m`.
    Duration,
}

pub const ASSERT_UNARY: &[&str] = &["exists", "isTrue", "isFalse", "isNumber", "isString", "isBoolean", "isArray"];
pub const ASSERT_BINARY: &[&str] =
    &["==", "equals", "!=", "<", "<=", ">", ">=", "contains", "includes", "startsWith", "endsWith", "matches"];

fn word<'a>(s: &mut &'a str) -> &'a str {
    let (w, rest) = s.split_once(char::is_whitespace).unwrap_or((*s, ""));
    *s = rest.trim_start();
    w
}

pub fn parse_assert(text: &str) -> Result<Assert, String> {
    let mut s = text.trim();
    let is_op = |w: &str| ASSERT_UNARY.contains(&w) || ASSERT_BINARY.contains(&w);
    let subject = match word(&mut s) {
        "status" => Subject::Status,
        "duration" => Subject::Duration,
        "header" => match word(&mut s) {
            "" => return Err("header name expected: header <name> <operator> …".into()),
            n => Subject::Header(n.into()),
        },
        "body" => {
            let mut peek = s;
            match word(&mut peek) {
                w if w.is_empty() || is_op(w) => Subject::Body(None),
                w => {
                    s = peek;
                    Subject::Body(Some(w.into()))
                }
            }
        }
        "" => return Err("expected: @assert <subject> <operator> [value]".into()),
        w => return Err(format!("unknown subject '{w}'; use status, header <name>, body [path] or duration")),
    };
    let op = word(&mut s);
    let expected = s.trim().to_string();
    match op {
        "" => Err("operator expected, e.g. ==, contains, exists".into()),
        o if ASSERT_UNARY.contains(&o) && !expected.is_empty() => Err(format!("'{o}' takes no value")),
        o if ASSERT_BINARY.contains(&o) && expected.is_empty() => Err(format!("'{o}' needs a value")),
        o if !is_op(o) => Err(format!("unknown operator '{o}'")),
        o => Ok(Assert { subject, op: o.into(), expected }),
    }
}

impl Request {
    /// `# @assert` lines in order: (line, text as written, parsed).
    pub fn asserts(&self) -> impl Iterator<Item = (usize, &str, Result<Assert, String>)> {
        self.directives.iter().filter(|d| d.name == "assert").map(|d| {
            let text = d.value.as_deref().unwrap_or("");
            (d.line, text, parse_assert(text))
        })
    }

    /// Label for trees and search: `@name`, then `###` title, then `METHOD url`.
    pub fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| format!("{} {}", self.method, self.url))
    }

    pub fn directive(&self, name: &str) -> Option<&Directive> {
        self.directives.iter().find(|d| d.name == name)
    }
}

impl HttpFile {
    /// Request block containing `line` (what Cmd+Enter sends).
    pub fn request_at(&self, line: usize) -> Option<&Request> {
        self.requests.iter().find(|r| r.lines.contains(&line))
    }
}

pub fn parse(src: &str) -> HttpFile {
    // str::lines strips both "\n" and "\r\n".
    let lines: Vec<&str> = src.lines().collect();
    let mut file = HttpFile::default();
    let (mut start, mut sep, mut title) = (0, None, None);
    for i in 0..=lines.len() {
        let is_sep = i < lines.len() && lines[i].starts_with("###");
        if is_sep || i == lines.len() {
            if start < i {
                parse_block(&lines, start, i, sep, title.take(), &mut file);
            }
            if is_sep {
                let t = lines[i].trim_start_matches('#').trim();
                title = (!t.is_empty()).then(|| t.to_string());
                sep = Some(i);
                start = i + 1;
            }
        }
    }

    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut dups = vec![];
    for r in &file.requests {
        if let (Some(name), Some(d)) = (&r.name, r.directive("name")) {
            let first = *seen.entry(name).or_insert(d.line);
            if first != d.line {
                dups.push(diag(d.line, Severity::Error, "duplicate_name",
                    format!("request name '{name}' already used on line {first}")));
            }
        }
    }
    file.diagnostics.extend(dups);
    file.diagnostics.sort_by_key(|d| d.line);
    file
}

fn diag(idx_or_line: usize, severity: Severity, code: &'static str, message: String) -> Diagnostic {
    Diagnostic { line: idx_or_line, severity, code, message }
}

/// `{{var}}` replaced by `0` padded with spaces: valid both inside a string and as a bare value,
/// and every byte keeps its line and column.
pub fn mask_vars(text: &str) -> String {
    let (mut out, mut rest) = (String::with_capacity(text.len()), text);
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}").map(|c| c + open + 2).filter(|&c| !rest[open..c].contains('\n')) else { break };
        out.push_str(&rest[..open]);
        out.push('0');
        out.extend(std::iter::repeat_n(' ', close - open - 1));
        rest = &rest[close..];
    }
    out + rest
}

/// JSON error as (1-based line within `text`, message without position); `{{vars}}` allowed.
pub fn check_json(text: &str) -> Result<(), (usize, String)> {
    json_error(text).map_or(Ok(()), |(line, _, msg)| Err((line, msg)))
}

/// (1-based line, 1-based byte column, message) of the first JSON error; `{{vars}}` allowed.
pub fn json_error(text: &str) -> Option<(usize, usize, String)> {
    let e = serde_json::from_str::<serde_json::Value>(&mask_vars(text)).err()?;
    let msg = e.to_string();
    let msg = msg.rsplit_once(" at line ").map_or(msg.as_str(), |(m, _)| m).to_string();
    Some((e.line().max(1), e.column().max(1), msg))
}

/// Byte offset of the comma a "trailing comma" error is about.
pub fn trailing_comma(text: &str) -> Option<usize> {
    let (line, col, msg) = json_error(text)?;
    if !msg.starts_with("trailing comma") {
        return None;
    }
    let start: usize = text.split_inclusive('\n').take(line - 1).map(str::len).sum();
    // The error points at (or just past) the closing bracket.
    let before = text[..(start + col).min(text.len())].trim_end();
    let before = before.strip_suffix(['}', ']']).unwrap_or(before).trim_end();
    before.ends_with(',').then(|| before.len() - 1)
}

fn comment(t: &str) -> Option<&str> {
    t.strip_prefix("//").or_else(|| t.strip_prefix('#')).map(str::trim)
}

fn file_variable(t: &str) -> Option<(&str, &str)> {
    let (name, value) = t.strip_prefix('@')?.split_once('=')?;
    let name = name.trim();
    (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| (name, value.trim()))
}

fn is_tail(t: &str) -> bool {
    t.starts_with(">>") || t.starts_with("> {%") || (t.starts_with("> ") && t.ends_with(".js"))
}

/// Index of the line closing a `{%` script opened on line `i`.
fn skip_script(lines: &[&str], i: usize, end: usize) -> usize {
    let open = lines[i].find("{%").map_or(0, |p| p + 2);
    (i..end)
        .find(|&j| lines[j][if j == i { open } else { 0 }..].contains("%}"))
        .unwrap_or(end - 1)
}

fn scripts_warning(i: usize) -> Diagnostic {
    diag(i + 1, Severity::Warning, "scripts_not_supported",
        "scripts are not executed in this version; the request is sent without them".into())
}

fn parse_block(
    lines: &[&str],
    start: usize,
    end: usize,
    sep: Option<usize>,
    title: Option<String>,
    file: &mut HttpFile,
) {
    let mut i = start;
    let mut directives = vec![];
    let mut has_scripts = false;

    // Preamble: comments, directives, file variables, pre-request script.
    loop {
        if i >= end {
            return; // block without a request line (e.g. only variables)
        }
        let t = lines[i].trim();
        if t.is_empty() {
        } else if let Some(c) = comment(t) {
            if let Some(d) = c.strip_prefix('@').filter(|d| !d.is_empty()) {
                let (name, value) = match d.split_once(char::is_whitespace) {
                    Some((n, v)) => (n, Some(v.trim().to_string()).filter(|v| !v.is_empty())),
                    None => (d, None),
                };
                if !DIRECTIVES.contains(&name) {
                    file.diagnostics.push(diag(i + 1, Severity::Warning, "unknown_directive",
                        format!("unknown directive '@{name}'")));
                }
                if name == "assert"
                    && let Err(e) = parse_assert(value.as_deref().unwrap_or(""))
                {
                    file.diagnostics.push(diag(i + 1, Severity::Error, "invalid_assert", e));
                }
                directives.push(Directive { name: name.into(), value, line: i + 1 });
            }
        } else if t.starts_with("< {%") {
            has_scripts = true;
            file.diagnostics.push(scripts_warning(i));
            i = skip_script(lines, i, end);
        } else if let Some((name, value)) = file_variable(t) {
            file.variables.push(FileVariable { name: name.into(), value: value.into(), line: i + 1 });
        } else {
            break;
        }
        i += 1;
    }

    // Request line: [METHOD] URL [HTTP/x].
    let request_line = i + 1;
    let mut toks: Vec<&str> = lines[i].split_whitespace().collect();
    let http_version = (toks.len() > 1 && toks.last().is_some_and(|t| t.starts_with("HTTP/")))
        .then(|| toks.pop().unwrap().to_string());
    let method = if toks.len() > 1 && toks[0].bytes().all(|b| b.is_ascii_uppercase()) {
        toks.remove(0).to_string()
    } else {
        "GET".to_string()
    };
    let mut url = toks.join(" ");
    i += 1;
    while i < end && matches!(lines[i].trim_start().chars().next(), Some('?' | '&')) {
        url.push_str(lines[i].trim());
        i += 1;
    }

    // Headers until a blank line.
    let mut headers = vec![];
    while i < end {
        let t = lines[i].trim();
        if t.is_empty() {
            i += 1;
            break;
        }
        if is_tail(t) {
            break;
        }
        if comment(t).is_none() {
            match t.split_once(':').filter(|(n, _)| !n.trim().is_empty()) {
                Some((n, v)) => headers.push(Header { name: n.trim().into(), value: v.trim().into(), line: i + 1 }),
                None => file.diagnostics.push(diag(i + 1, Severity::Error, "invalid_header",
                    format!("header without ':' — '{t}'"))),
            }
        }
        i += 1;
    }

    // Body until a response handler / `>>` redirect.
    let body_start = i;
    while i < end && !is_tail(lines[i].trim_start()) {
        i += 1;
    }
    let mut body_end = i;
    while body_end > body_start && lines[body_end - 1].trim().is_empty() {
        body_end -= 1;
    }
    let body = (body_end > body_start)
        .then(|| Body { text: lines[body_start..body_end].join("\n"), line: body_start + 1 });
    let json = headers.iter().any(|h| h.name.eq_ignore_ascii_case("content-type") && h.value.to_ascii_lowercase().contains("json"));
    if let Some(b) = body.as_ref().filter(|b| json && !b.text.trim_start().starts_with('<'))
        && let Err((line, message)) = check_json(&b.text)
    {
        file.diagnostics.push(diag(b.line + line - 1, Severity::Error, "invalid_json", format!("invalid JSON body: {message}")));
    }

    // Tail: handler scripts and output redirect.
    let mut output = None;
    while i < end {
        let t = lines[i].trim_start();
        if let Some(rest) = t.strip_prefix(">>") {
            let overwrite = rest.starts_with('!');
            let path = rest.trim_start_matches('!').trim().to_string();
            output = Some(Output { path, overwrite, line: i + 1 });
        } else if t.starts_with("> {%") {
            has_scripts = true;
            file.diagnostics.push(scripts_warning(i));
            i = skip_script(lines, i, end);
        } else if is_tail(t) {
            has_scripts = true;
            file.diagnostics.push(scripts_warning(i));
        }
        i += 1;
    }

    let name = directives.iter().find(|d| d.name == "name").and_then(|d| d.value.clone());
    file.requests.push(Request {
        title,
        name,
        directives,
        method,
        url,
        http_version,
        headers,
        body,
        output,
        has_scripts,
        lines: sep.unwrap_or(start) + 1..=end,
        request_line,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_block() {
        let f = parse(
            "### Створити користувача\r\n\
             # @name createUser\r\n\
             # @timeout 10 s\r\n\
             POST {{baseUrl}}/users?notify=true HTTP/1.1\r\n\
             Content-Type: application/json\r\n\
             Authorization: Bearer {{token}}\r\n\
             \r\n\
             {\r\n  \"id\": \"{{$uuid}}\"\r\n}\r\n\r\n",
        );
        assert!(f.diagnostics.is_empty(), "{:?}", f.diagnostics);
        let r = &f.requests[0];
        assert_eq!(r.title.as_deref(), Some("Створити користувача"));
        assert_eq!(r.name.as_deref(), Some("createUser"));
        assert_eq!(r.directive("timeout").unwrap().value.as_deref(), Some("10 s"));
        assert_eq!((r.method.as_str(), r.url.as_str()), ("POST", "{{baseUrl}}/users?notify=true"));
        assert_eq!(r.http_version.as_deref(), Some("HTTP/1.1"));
        assert_eq!(r.headers[1].value, "Bearer {{token}}");
        let b = r.body.as_ref().unwrap();
        assert_eq!((b.text.as_str(), b.line), ("{\n  \"id\": \"{{$uuid}}\"\n}", 8));
        assert_eq!((r.lines.clone(), r.request_line), (1..=11, 4));
        assert_eq!(f.request_at(9).unwrap().label(), "createUser");
    }

    #[test]
    fn file_variables_default_method_and_multiline_query() {
        let f = parse(
            "@host = api.example.com\n\
             @baseUrl = https://{{host}}/v1\n\
             \n\
             ### Список\n\
             {{baseUrl}}/users\n    ?page=1\n    &limit=20\n\
             Accept: */*\n\
             ###\n\
             @token = {{login.response.body.$.access_token}}\n\
             // comment\n\
             DELETE /x\n",
        );
        assert_eq!(f.variables.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["host", "baseUrl", "token"]);
        assert_eq!(f.variables[2].value, "{{login.response.body.$.access_token}}");
        assert_eq!(f.requests.len(), 2);
        assert_eq!((f.requests[0].method.as_str(), f.requests[0].url.as_str()), ("GET", "{{baseUrl}}/users?page=1&limit=20"));
        assert_eq!(f.requests[0].headers.len(), 1);
        assert_eq!(f.requests[1].label(), "DELETE /x");
        assert!(f.request_at(2).is_none());
    }

    #[test]
    fn scripts_redirect_and_diagnostics() {
        let f = parse(
            "### a\n\
             # @name a\n\
             # @bogus\n\
             < {%\n  request.variables.set('x', '1');\n%}\n\
             POST /a\n\
             no colon here\n\
             \n\
             < ./data.json\n\
             \n\
             > {%\n  client.test('ok', () => {});\n%}\n\
             >>! ./out/a.json\n\
             ### b\n\
             # @name a\n\
             GET /b\n\
             > {% client.log(1) %}\n",
        );
        let r = &f.requests[0];
        assert!(r.has_scripts && f.requests[1].has_scripts);
        assert_eq!(r.body.as_ref().unwrap().text, "< ./data.json");
        assert_eq!(r.output, Some(Output { path: "./out/a.json".into(), overwrite: true, line: 15 }));
        assert!(f.requests[1].body.is_none());
        let codes: Vec<_> = f.diagnostics.iter().map(|d| (d.line, d.code)).collect();
        assert_eq!(codes, [
            (3, "unknown_directive"),
            (4, "scripts_not_supported"),
            (8, "invalid_header"),
            (12, "scripts_not_supported"),
            (17, "duplicate_name"),
            (19, "scripts_not_supported"),
        ]);
    }

    #[test]
    fn asserts() {
        let a = |s| parse_assert(s);
        let ok = |subject, op: &str, expected: &str| Ok(Assert { subject, op: op.into(), expected: expected.into() });
        assert_eq!(a("status == 201"), ok(Subject::Status, "==", "201"));
        assert_eq!(a("header content-type contains json"), ok(Subject::Header("content-type".into()), "contains", "json"));
        assert_eq!(a("body $.items[0].name == Ann Lee"), ok(Subject::Body(Some("$.items[0].name".into())), "==", "Ann Lee"));
        assert_eq!(a("body id exists"), ok(Subject::Body(Some("id".into())), "exists", ""));
        assert_eq!(a("body contains ok"), ok(Subject::Body(None), "contains", "ok"));
        assert_eq!(a("duration < 2 s"), ok(Subject::Duration, "<", "2 s"));
        for bad in ["", "code == 1", "status", "status ~ 1", "status ==", "body exists 1", "header"] {
            assert!(a(bad).is_err(), "{bad}");
        }
        let f = parse("# @assert status == 200\n# @assert status ~ 1\nGET /a\n");
        assert_eq!(f.diagnostics.iter().map(|d| (d.line, d.code)).collect::<Vec<_>>(), [(2, "invalid_assert")]);
        assert_eq!(f.requests[0].asserts().count(), 2);
    }

    #[test]
    fn invalid_json_body() {
        let f = parse("POST /a\nContent-Type: application/json\n\n{\n  \"a\": {{n}},\n  \"b\": \"{{s}}\",\n}\n");
        let d: Vec<_> = f.diagnostics.iter().map(|d| (d.line, d.code, d.message.as_str())).collect();
        assert_eq!(d, [(7, "invalid_json", "invalid JSON body: trailing comma")]);
        // Not JSON by Content-Type, or a file reference: not checked.
        assert!(parse("POST /a\n\n{ broken\n").diagnostics.is_empty());
        assert!(parse("POST /a\nContent-Type: application/json\n\n< ./a.json\n").diagnostics.is_empty());

        let body = "{\n  \"a\": {{n}},\n  \"b\": [1, 2,]\n,}";
        assert_eq!(trailing_comma(body), Some(body.find("2,").unwrap() + 1));
        assert_eq!(trailing_comma("{\"a\": 1 , \n }"), Some(8));
        assert_eq!(trailing_comma("{\"a\" 1}"), None);
    }

    #[test]
    fn multipart_body_kept_raw() {
        let f = parse(
            "POST /upload\n\
             Content-Type: multipart/form-data; boundary=B\n\
             \n\
             --B\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"r.pdf\"\n\
             \n\
             < ./r.pdf\n\
             --B--\n",
        );
        assert_eq!(f.requests[0].body.as_ref().unwrap().text.lines().count(), 5);
        assert!(f.diagnostics.is_empty());
    }
}
