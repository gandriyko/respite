//! `.http` language features on the editor's public LSP-style seams: diagnostics, completion,
//! hover and go to definition (Cmd+click / F12). Everything is computed locally and synchronously.

use std::{cell::RefCell, collections::HashMap, ops::Range, path::{Path, PathBuf}, rc::Rc, sync::Arc};

use anyhow::Result;
use gpui::{App, Task, Window};
use gpui_component::input::{CompletionProvider, DefinitionProvider, HoverProvider, Rope, RopeExt as _};
use http_env::Variable;
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, Hover, HoverContents,
    LocationLink, MarkupContent, MarkupKind, Position, TextEdit, Uri,
};

/// What the providers need besides the text; the doc updates it when the environment changes.
pub struct Ctx {
    pub path: Arc<PathBuf>,
    /// The selected environment: hover values.
    pub env: Arc<HashMap<String, Variable>>,
    /// Every environment: completion and diagnostics (a name defined in any of them is fine).
    pub all: Arc<HashMap<String, Variable>>,
}

pub type Shared = Rc<RefCell<Ctx>>;

pub struct Lang(pub Shared);

const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
const HEADERS: &[&str] = &[
    "Accept",
    "Accept-Encoding",
    "Accept-Language",
    "Authorization",
    "Cache-Control",
    "Connection",
    "Content-Encoding",
    "Content-Type",
    "Cookie",
    "Host",
    "If-Match",
    "If-Modified-Since",
    "If-None-Match",
    "Origin",
    "Pragma",
    "Range",
    "Referer",
    "User-Agent",
    "X-API-Key",
    "X-Request-ID",
    "X-Requested-With",
];
/// Most used first: the menu keeps this order.
const MIME: &[&str] = &[
    "application/json",
    "application/json; charset=utf-8",
    "application/x-www-form-urlencoded",
    "multipart/form-data; boundary=RespiteFormBoundary",
    "application/xml",
    "text/xml",
    "text/plain",
    "text/plain; charset=utf-8",
    "text/html",
    "text/csv",
    "text/css",
    "text/javascript",
    "text/markdown",
    "text/event-stream",
    "application/octet-stream",
    "application/graphql",
    "application/graphql-response+json",
    "application/ld+json",
    "application/problem+json",
    "application/merge-patch+json",
    "application/json-patch+json",
    "application/vnd.api+json",
    "application/hal+json",
    "application/x-ndjson",
    "application/soap+xml",
    "application/atom+xml",
    "application/rss+xml",
    "application/javascript",
    "application/yaml",
    "application/toml",
    "application/protobuf",
    "application/x-protobuf",
    "application/grpc",
    "application/grpc-web+proto",
    "application/cbor",
    "application/msgpack",
    "application/jwt",
    "application/pdf",
    "application/zip",
    "application/gzip",
    "application/x-tar",
    "application/wasm",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "multipart/mixed; boundary=RespiteFormBoundary",
    "multipart/related; boundary=RespiteFormBoundary",
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/svg+xml",
    "image/avif",
    "image/x-icon",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "video/mp4",
    "video/webm",
    "font/woff2",
];
/// Typical values per header (matched case-insensitively), offered after `Name:`.
fn header_values(name: &str) -> Vec<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "content-type" => MIME.to_vec(),
        "accept" => [&["*/*"], MIME].concat(),
        "accept-encoding" => vec!["gzip, deflate, br", "gzip", "deflate", "br", "zstd", "identity"],
        "content-encoding" => vec!["gzip", "deflate", "br", "zstd"],
        "accept-language" => vec!["en-US,en;q=0.9", "en", "uk-UA,uk;q=0.9", "*"],
        "authorization" => vec!["Bearer {{token}}", "Basic {{username}} {{password}}"],
        "cache-control" => vec!["no-cache", "no-store", "max-age=0", "must-revalidate"],
        "pragma" => vec!["no-cache"],
        "connection" => vec!["keep-alive", "close"],
        "range" => vec!["bytes=0-1023"],
        "x-requested-with" => vec!["XMLHttpRequest"],
        _ => vec![],
    }
}
/// `{{$…}}` names with a one-line description.
const DYNAMIC: &[(&str, &str)] = &[
    ("$uuid", "Random UUID v4"),
    ("$randomInt", "Random integer: `$randomInt [min max]`, 0..1000 by default"),
    ("$timestamp", "Unix time in seconds: `$timestamp [offset unit]`"),
    ("$isoTimestamp", "Current UTC time, ISO 8601"),
    ("$datetime", "UTC date-time: `$datetime rfc1123|iso8601|\"format\" [offset unit]`"),
    ("$localDatetime", "Local date-time, same arguments as `$datetime`"),
    ("$processEnv", "Process environment variable: `$processEnv NAME`"),
    ("$dotenv", "Value from `.env` next to the file: `$dotenv NAME`"),
];

/// Editor diagnostic for a parse / check finding; the line is underlined as a whole.
pub fn diagnostic(d: &http_syntax::Diagnostic) -> lsp_types::Diagnostic {
    let line = d.line.saturating_sub(1) as u32;
    lsp_types::Diagnostic {
        range: lsp_types::Range::new(Position::new(line, 0), Position::new(line, u32::MAX)),
        severity: Some(match d.severity {
            http_syntax::Severity::Error => lsp_types::DiagnosticSeverity::ERROR,
            http_syntax::Severity::Warning => lsp_types::DiagnosticSeverity::WARNING,
        }),
        code: Some(lsp_types::NumberOrString::String(d.code.into())),
        source: Some("respite".into()),
        message: d.message.clone(),
        ..Default::default()
    }
}

/// `file://` URI for `path` (percent-encoded), and back: definitions in other files travel
/// through the editor as URIs and come back to [`crate`]'s `show_document` handler.
pub fn file_uri(path: &Path) -> Option<Uri> {
    let mut s = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => s.push(b as char),
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s.parse().ok()
}

pub fn uri_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str().strip_prefix("file://")?;
    let (mut out, b) = (Vec::with_capacity(s.len()), s.as_bytes());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(Ok(v)) = s.get(i + 1..i + 3).map(|h| u8::from_str_radix(h, 16))
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// The line `row` (0-based) of `text`, without its line break, and its start offset.
fn line_at(text: &Rope, row: usize) -> (String, usize) {
    let line = text.slice_line(row).to_string();
    (line.trim_end_matches(['\n', '\r']).to_string(), text.line_start_offset(row))
}

/// `{{expr}}` around byte `offset`: the trimmed expression and the byte range of the braces.
fn var_at(text: &Rope, offset: usize) -> Option<(String, Range<usize>)> {
    let row = text.offset_to_position(offset).line as usize;
    let (line, start) = line_at(text, row);
    let col = offset - start;
    let mut pos = 0;
    while let Some(open) = line[pos..].find("{{").map(|o| o + pos) {
        let close = line[open..].find("}}")? + open + 2;
        if (open..close).contains(&col) {
            return Some((line[open + 2..close - 2].trim().to_string(), start + open..start + close));
        }
        pos = close;
    }
    None
}

fn range(text: &Rope, r: Range<usize>) -> lsp_types::Range {
    lsp_types::Range::new(text.offset_to_position(r.start), text.offset_to_position(r.end))
}

fn line_range(row: usize) -> lsp_types::Range {
    lsp_types::Range::new(Position::new(row as u32, 0), Position::new(row as u32, 0))
}

fn code(v: &str) -> String {
    format!("`{}`", v.replace('`', "'"))
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// 0-based line of `"key":` in a JSON env file (first match), else 0.
fn key_line(path: &Path, key: &str) -> usize {
    let needle = format!("\"{key}\"");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.lines().position(|l| l.trim_start().starts_with(&needle) && l.contains(':')))
        .unwrap_or(0)
}

/// Appended after the headers when `multipart/form-data` is picked for a request without a body.
/// Its boundary is [`crate::attach::DEFAULT_BOUNDARY`], the one in the `MIME` entry.
const FORM_BODY: &str =
    "\n\n--RespiteFormBoundary\nContent-Disposition: form-data; name=\"field\"\n\nvalue\n--RespiteFormBoundary--";

impl Lang {
    /// For `Content-Type` on line `row` of a request without a body: the offset where its
    /// headers end and the header lines after `row` (with their line breaks), which a form
    /// body completion rewrites unchanged.
    fn form_skeleton(&self, text: &Rope, row: usize, name: &str) -> Option<(usize, String)> {
        if !name.eq_ignore_ascii_case("content-type") {
            return None;
        }
        let file = crate::highlight::parse(&text.to_string());
        let r = file.request_at(row + 1).filter(|r| r.body.is_none())?;
        let last = r.headers.iter().map(|h| h.line).max()?.max(row + 1) - 1;
        let end = text.line_end_offset(last);
        Some((end, text.slice(text.line_end_offset(row)..end).to_string()))
    }
    /// Markdown for the variable under the cursor: its value (secrets masked) and where it's from.
    fn describe(&self, expr: &str, file: &http_syntax::HttpFile) -> String {
        if let Some((_, about)) = DYNAMIC.iter().find(|(n, _)| expr.split_whitespace().next() == Some(n)) {
            return format!("Dynamic variable: {about}. New value on every send.");
        }
        if let Some((name, path)) = expr.split_once('.') {
            return match file.requests.iter().any(|r| r.name.as_deref() == Some(name)) {
                true => format!("{} of the last response of request {}", code(path), code(name)),
                false => format!("No request named {} in this file", code(name)),
            };
        }
        if let Some(v) = file.variables.iter().find(|v| v.name == expr) {
            return format!("{}\n\nFile variable, line {}", code(&v.value), v.line);
        }
        let ctx = self.0.borrow();
        let shown = |v: &Variable| if v.secret { "••••••".to_string() } else { code(&v.value) };
        // `respite.json` / `respite.private.json` apply with any environment, and with none.
        let scope = |v: &Variable| match file_name(&v.source).starts_with("respite.") {
            true => ", shared by all environments",
            false => "",
        };
        // Hover links would leave the app (the editor opens them in the browser); point at go to definition.
        let edit = if cfg!(target_os = "macos") { "Cmd+click or F12 to edit" } else { "Ctrl+click or F12 to edit" };
        match (ctx.env.get(expr), ctx.all.get(expr)) {
            (Some(v), _) => format!("{}\n\nFrom {}{}. {edit}", shown(v), code(&file_name(&v.source)), scope(v)),
            (None, Some(v)) => format!("Not set in the selected environment (defined in {}). {edit}", code(&file_name(&v.source))),
            (None, None) => "Undefined variable".into(),
        }
    }

    fn definition(&self, text: &Rope, offset: usize) -> Option<LocationLink> {
        let (expr, braces) = var_at(text, offset)?;
        let file = crate::highlight::parse(&text.to_string());
        let ctx = self.0.borrow();
        let here = |line: usize| (ctx.path.to_path_buf(), line.saturating_sub(1));
        let (path, row) = if let Some((name, _)) = expr.split_once('.') {
            here(file.requests.iter().find(|r| r.name.as_deref() == Some(name))?.request_line)
        } else if let Some(v) = file.variables.iter().find(|v| v.name == expr) {
            here(v.line)
        } else {
            let v = ctx.env.get(&expr).or_else(|| ctx.all.get(&expr))?;
            (v.source.clone(), key_line(&v.source, &expr))
        };
        Some(LocationLink {
            origin_selection_range: Some(range(text, braces)),
            target_uri: file_uri(&path)?,
            target_range: line_range(row),
            target_selection_range: line_range(row),
        })
    }

    fn completions_at(&self, text: &Rope, offset: usize) -> Vec<CompletionItem> {
        let pos = text.offset_to_position(offset);
        let row = pos.line as usize;
        let (line, start) = line_at(text, row);
        let col = (offset - start).min(line.len());
        let before = &line[..col];
        let edit = |word: &str, label: &str, new_text: String, kind, detail: Option<String>| CompletionItem {
            label: label.into(),
            kind: Some(kind),
            detail,
            text_edit: Some(CompletionTextEdit::Edit(TextEdit { range: range(text, offset - word.len()..offset), new_text })),
            ..Default::default()
        };
        let matches = |cand: &str, word: &str| cand.to_lowercase().starts_with(&word.to_lowercase()) && cand != word;

        // Inside `{{`: variables, dynamic variables, request references.
        if let Some(open) = before.rfind("{{")
            && !before[open..].contains("}}")
        {
            let word = before[open + 2..].trim_start();
            let close = if line[col..].starts_with("}}") { "" } else { "}}" };
            let file = crate::highlight::parse(&text.to_string());
            let ctx = self.0.borrow();
            let mut cands: Vec<(String, String, CompletionItemKind)> = vec![];
            cands.extend(file.variables.iter().map(|v| (v.name.clone(), format!("file variable = {}", v.value), CompletionItemKind::VARIABLE)));
            let mut env: Vec<_> = ctx.all.iter().collect();
            env.sort_by(|a, b| a.0.cmp(b.0));
            cands.extend(env.into_iter().map(|(k, v)| (k.clone(), file_name(&v.source), CompletionItemKind::VARIABLE)));
            cands.extend(DYNAMIC.iter().map(|(n, about)| (n.to_string(), about.to_string(), CompletionItemKind::FUNCTION)));
            cands.extend(
                file.requests.iter().filter_map(|r| r.name.as_ref()).map(|n| (format!("{n}.response.body"), "last response body".into(), CompletionItemKind::REFERENCE)),
            );
            let mut seen = std::collections::HashSet::new();
            return cands
                .into_iter()
                .filter(|(n, ..)| matches(n, word) && seen.insert(n.clone()))
                .map(|(n, detail, kind)| edit(word, &n, format!("{n}{close}"), kind, Some(detail)))
                .collect();
        }

        let trimmed = before.trim_start();
        // `# @…` / `// @…`: directives.
        if let Some(word) = trimmed.strip_prefix('#').or_else(|| trimmed.strip_prefix("//")).and_then(|r| r.trim_start().strip_prefix('@')) {
            return http_syntax::DIRECTIVES
                .iter()
                .filter(|d| matches(d, word))
                .map(|d| edit(word, d, d.to_string(), CompletionItemKind::KEYWORD, None))
                .collect();
        }

        // `--` in a multipart request's body: part templates with its boundary.
        if trimmed.starts_with("--") {
            let word = trimmed;
            let file = crate::highlight::parse(&text.to_string());
            let part_of = |r: &http_syntax::Request| {
                let headers_end = r.headers.iter().map(|h| h.line).chain([r.request_line]).max()?;
                let ct = crate::attach::content_type(r).filter(|c| c.starts_with("multipart/"))?;
                (row + 1 > headers_end).then(|| crate::attach::boundary(ct).unwrap_or(crate::attach::DEFAULT_BOUNDARY).to_string())
            };
            if let Some(b) = file.request_at(row + 1).and_then(part_of) {
                let templates = [
                    ("text field", format!("--{b}\nContent-Disposition: form-data; name=\"field\"\n\nvalue")),
                    (
                        "file field (Cmd+I picks a file)",
                        format!("--{b}\nContent-Disposition: form-data; name=\"file\"; filename=\"file.bin\"\nContent-Type: application/octet-stream\n\n< ./file.bin"),
                    ),
                    ("end of form", format!("--{b}--")),
                ];
                return templates
                    .into_iter()
                    .map(|(what, body)| edit(word, &format!("--{b} {what}"), body, CompletionItemKind::SNIPPET, None))
                    .collect();
            }
        }

        let prev = if row == 0 { String::new() } else { line_at(text, row - 1).0 };
        let prev = prev.trim();
        let is_name = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        let is_header = |l: &str| l.split_once(':').is_some_and(|(n, _)| is_name(n));
        let is_request = |l: &str| l.split_whitespace().next().is_some_and(|m| METHODS.contains(&m));
        let in_headers = !prev.is_empty() && (is_header(prev) || is_request(prev));

        // `Name: …` in the header block: that header's typical values. The edit covers
        // everything after the colon, so it also supplies the space after `Name:`.
        if in_headers
            && let Some((name, after)) = trimmed.split_once(':')
            && is_name(name)
        {
            let typed = after.trim_start().to_lowercase();
            let form = self.form_skeleton(text, row, name);
            return header_values(name)
                .into_iter()
                .filter(|v| v.to_lowercase().contains(&typed) && v.to_lowercase() != typed)
                .map(|v| match (&form, v.starts_with("multipart/form-data")) {
                    // The edit may span lines: it reaches the end of the headers, rewrites the
                    // ones below as they are, and appends the body frame after them.
                    (Some((end, rest)), true) => CompletionItem {
                        detail: Some("adds a form body".into()),
                        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                            range: range(text, offset - after.len()..*end),
                            new_text: format!(" {v}{rest}{}", FORM_BODY),
                        })),
                        ..edit(after, v, String::new(), CompletionItemKind::VALUE, None)
                    },
                    _ => edit(after, v, format!(" {v}"), CompletionItemKind::VALUE, None),
                })
                .collect();
        }

        // A first word: a header name right under a request line or header, else a method.
        if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return vec![];
        }
        if in_headers {
            HEADERS.iter().filter(|h| matches(h, trimmed)).map(|h| edit(trimmed, h, format!("{h}: "), CompletionItemKind::PROPERTY, None)).collect()
        } else if prev.is_empty() || prev.starts_with('#') || prev.starts_with("//") {
            METHODS.iter().filter(|m| matches(m, trimmed)).map(|m| edit(trimmed, m, format!("{m} "), CompletionItemKind::KEYWORD, None)).collect()
        } else {
            vec![]
        }
    }
}

impl CompletionProvider for Lang {
    fn completions(&self, text: &Rope, offset: usize, _: CompletionContext, _: &mut Window, _: &mut App) -> Task<Result<CompletionResponse>> {
        Task::ready(Ok(CompletionResponse::Array(self.completions_at(text, offset))))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        // `:` and ` ` open header values right after `Content-Type:`; `/` refines `application/…`.
        !new_text.is_empty() && new_text.chars().all(|c| c.is_alphanumeric() || "{$@._-: /".contains(c))
    }
}

impl HoverProvider for Lang {
    fn hover(&self, text: &Rope, offset: usize, _: &mut Window, _: &mut App) -> Task<Result<Option<Hover>>> {
        let hover = var_at(text, offset).map(|(expr, braces)| {
            let file = crate::highlight::parse(&text.to_string());
            Hover {
                contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value: self.describe(&expr, &file) }),
                range: Some(range(text, braces)),
            }
        });
        Task::ready(Ok(hover))
    }
}

impl DefinitionProvider for Lang {
    fn definitions(&self, text: &Rope, offset: usize, _: &mut Window, _: &mut App) -> Task<Result<Vec<LocationLink>>> {
        Task::ready(Ok(self.definition(text, offset).into_iter().collect()))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*` alone: keep `#[test]` the std one.
    use super::{Ctx, Lang, file_uri, uri_path, var_at};
    use gpui_component::input::Rope;
    use http_env::Variable;
    use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, sync::Arc};

    fn lang() -> Lang {
        let var = |v: &str, secret| Variable { value: v.into(), source: PathBuf::from("/p/respite-dev.json"), secret };
        let env = HashMap::from([("host".to_string(), var("http://x", false)), ("token".to_string(), var("s3cret", true))]);
        let all = Arc::new(env.clone());
        Lang(Rc::new(RefCell::new(Ctx { path: Arc::new(PathBuf::from("/p/a.http")), env: Arc::new(env), all })))
    }

    fn labels(l: &Lang, src: &str) -> Vec<String> {
        let text = Rope::from(src);
        l.completions_at(&text, src.len()).into_iter().map(|i| i.label).collect()
    }

    #[test]
    fn uri_round_trip() {
        let p = PathBuf::from("/Users/a b/Колекції/respite-dev.json");
        assert_eq!(uri_path(&file_uri(&p).unwrap()), Some(p));
    }

    #[test]
    fn hover_and_completion() {
        let l = lang();
        let src = "@id = 7\n# @name login\nGET {{host}}/u/{{id}}?t={{token}}\n";
        let text = Rope::from(src);
        let file = http_syntax::parse(src);
        let at = |s: &str| src.find(s).unwrap() + 2;
        assert_eq!(var_at(&text, at("{{host")).unwrap().0, "host");
        assert!(l.describe("host", &file).contains("`http://x`"));
        assert!(!l.describe("host", &file).contains("shared"));
        assert!(!l.describe("token", &file).contains("s3cret"));
        assert!(l.describe("id", &file).contains("File variable"));
        assert!(l.describe("login.response.body", &file).contains("last response"));
        assert_eq!(l.describe("nope", &file), "Undefined variable");

        assert_eq!(labels(&l, "GET {{ho"), ["host"]);
        assert!(labels(&l, "GET {{$u").contains(&"$uuid".to_string()));
        assert_eq!(labels(&l, "# @na"), ["name"]);
        assert_eq!(labels(&l, "\nPO"), ["POST"]);
        assert_eq!(labels(&l, "GET /x\nContent-T"), ["Content-Type"]);
        // Header values: right after the colon, then narrowed by what's typed (anywhere in the value).
        assert!(labels(&l, "GET /x\nContent-Type:").contains(&"application/json".to_string()));
        let json = labels(&l, "GET /x\nAccept: */*\ncontent-type: jso");
        assert_eq!(json.first().map(String::as_str), Some("application/json"));
        assert!(json.iter().all(|v| v.contains("json")), "{json:?}");
        assert!(labels(&l, "GET /x\nX-Unknown: ").is_empty());
        // Picking multipart/form-data for a body-less request adds the form frame after the
        // last header, keeping the headers below the Content-Type line.
        let (src, at) = ("POST /u\nContent-Type: mul\nX-A: 1\n", "POST /u\nContent-Type: mul".len());
        let text = Rope::from(src);
        let item = l.completions_at(&text, at).into_iter().find(|i| i.label.starts_with("multipart/form-data")).unwrap();
        let Some(lsp_types::CompletionTextEdit::Edit(e)) = item.text_edit else { panic!() };
        let mut out = src.to_string();
        out.replace_range(
            gpui_component::input::RopeExt::position_to_offset(&text, &e.range.start)
                ..gpui_component::input::RopeExt::position_to_offset(&text, &e.range.end),
            &e.new_text,
        );
        assert_eq!(
            out,
            "POST /u\nContent-Type: multipart/form-data; boundary=RespiteFormBoundary\nX-A: 1\n\n--RespiteFormBoundary\nContent-Disposition: form-data; name=\"field\"\n\nvalue\n--RespiteFormBoundary--\n"
        );
        let r = &http_syntax::parse(&out).requests[0];
        assert_eq!((r.headers.len(), r.body.as_ref().map(|b| b.text.lines().count())), (2, Some(5)));
        // With a body already there, it's just the value.
        let with_body = "POST /u\nContent-Type: mul\n\nx\n";
        let item = l.completions_at(&Rope::from(with_body), "POST /u\nContent-Type: mul".len()).into_iter().find(|i| i.label.starts_with("multipart/form-data")).unwrap();
        let Some(lsp_types::CompletionTextEdit::Edit(e)) = item.text_edit else { panic!() };
        assert_eq!(e.new_text, " multipart/form-data; boundary=RespiteFormBoundary");

        // `--` in a multipart body: part templates with the request's boundary.
        let mp = "POST /u\nContent-Type: multipart/form-data; boundary=XY\n\n--";
        assert_eq!(labels(&l, mp), ["--XY text field", "--XY file field (Cmd+I picks a file)", "--XY end of form"]);
        assert!(labels(&l, "POST /u\nContent-Type: text/plain\n\n--").is_empty());
        let src = "GET /x\nContent-Type:";
        let text = Rope::from(src);
        let item = l.completions_at(&text, src.len()).into_iter().find(|i| i.label == "text/plain").unwrap();
        let Some(lsp_types::CompletionTextEdit::Edit(e)) = item.text_edit else { panic!() };
        assert_eq!((e.new_text.as_str(), e.range.start.character, e.range.end.character), (" text/plain", 13, 13));
        assert!(labels(&l, "GET /x\n\n{\"a\": tr").is_empty());
    }
}
