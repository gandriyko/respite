//! Attaching files to a request: a `multipart/form-data` part per file (adding the
//! `Content-Type` header and the body frame when missing), or a raw `< ./file` body when the
//! request already declares another content type.

use std::path::{Path, PathBuf};

use http_syntax::Request;

/// For new forms. Parts are split on `--<boundary>`, so it must not occur in any part's
/// content: a word like `boundary` easily can, this one practically never (the cURL and
/// Postman imports use it too).
pub const DEFAULT_BOUNDARY: &str = "RespiteFormBoundary";

/// `boundary=` of a multipart `Content-Type`, as written (quotes dropped).
pub fn boundary(content_type: &str) -> Option<&str> {
    let b = content_type.split(';').find_map(|p| p.trim().strip_prefix("boundary="))?;
    Some(b.trim_matches('"'))
}

pub fn content_type(r: &Request) -> Option<&str> {
    r.headers.iter().find(|h| h.name.eq_ignore_ascii_case("content-type")).map(|h| h.value.as_str())
}

/// By extension; unknown ones are `application/octet-stream`.
pub fn mime_for(path: &Path) -> &'static str {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "json" => "application/json",
        "xml" => "application/xml",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "md" => "text/markdown",
        "yaml" | "yml" => "application/yaml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
}

/// `./sub/file` when `file` is under `dir` (the `.http` file's folder), else the absolute path.
pub fn file_ref(file: &Path, dir: &Path) -> String {
    match file.strip_prefix(dir) {
        Ok(rel) => format!("./{}", rel.display()),
        Err(_) => file.display().to_string(),
    }
}

/// Byte offset where 1-based `line` starts (`src.len()` past the end).
fn line_start(src: &str, line: usize) -> usize {
    src.split_inclusive('\n').take(line.saturating_sub(1)).map(str::len).sum()
}

/// A form part for `file` (`file_ref` as written in the body).
pub fn file_part(boundary: &str, file: &Path, file_ref: &str) -> String {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    format!(
        "--{boundary}\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\nContent-Type: {}\n\n< {file_ref}\n",
        mime_for(file)
    )
}

/// Insertions (byte offset in `src`, text) attaching `file` to `r`, last offset first so they
/// can be applied in order. Returns the offset in the *edited* text where the part starts.
pub fn attach(src: &str, r: &Request, file: &Path, file_ref: &str) -> (Vec<(usize, String)>, usize) {
    let header_end = r.headers.iter().map(|h| h.line).chain([r.request_line]).max().unwrap_or(r.request_line);
    // A file without a trailing newline: start the insertion on a new line.
    let at_line = |line: usize| {
        let off = line_start(src, line);
        let nl = if off == src.len() && !src.is_empty() && !src.ends_with('\n') { "\n" } else { "" };
        (off, nl)
    };
    let (after_headers, nl) = at_line(header_end + 1);
    let ct = content_type(r);

    if ct.is_some_and(|c| !c.starts_with("multipart/")) {
        // Raw body: the file is the body (on top of an existing one, which the user then trims).
        return match &r.body {
            None => (vec![(after_headers, format!("{nl}\n< {file_ref}\n"))], after_headers + nl.len() + 1),
            Some(b) => {
                let (off, nl) = at_line(b.line);
                (vec![(off, format!("{nl}< {file_ref}\n"))], off + nl.len())
            }
        };
    }

    let b = ct.and_then(boundary).unwrap_or(DEFAULT_BOUNDARY).to_string();
    let part = file_part(&b, file, file_ref);
    let header = if ct.is_none() { format!("Content-Type: multipart/form-data; boundary={b}\n") } else { String::new() };
    match &r.body {
        // Header (if any), blank line, part, closing boundary: one insertion after the headers.
        None => {
            let text = format!("{nl}{header}\n{part}--{b}--\n");
            let part_at = after_headers + nl.len() + header.len() + 1;
            (vec![(after_headers, text)], part_at)
        }
        Some(body) => {
            let close = format!("--{b}--");
            let lines: Vec<&str> = body.text.lines().collect();
            let (off, nl, text) = match lines.iter().position(|l| l.trim() == close) {
                Some(i) => (line_start(src, body.line + i), "", part.clone()),
                None => {
                    let (off, nl) = at_line(body.line + lines.len());
                    (off, nl, format!("{part}{close}\n"))
                }
            };
            let mut edits = vec![(off, format!("{nl}{text}"))];
            if !header.is_empty() {
                edits.push((after_headers, header.clone()));
            }
            (edits, off + nl.len() + header.len())
        }
    }
}

/// Attach `files` to the request under the editor's cursor; `false` when it isn't in one.
pub fn attach_files(
    s: &mut gpui_component::input::EditorState,
    files: &[PathBuf],
    dir: &Path,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<gpui_component::input::EditorState>,
) -> bool {
    use gpui_component::input::RopeExt as _;
    for file in files {
        // Re-read per file: the previous one changed the text.
        let src = s.value().to_string();
        let parsed = crate::highlight::parse(&src);
        let line = s.cursor_position().line as usize + 1;
        let Some(r) = parsed.request_at(line) else { return false };
        let (edits, part_at) = attach(&src, r, file, &file_ref(file, dir));
        // Bottom-up, so earlier offsets stay valid; then the cursor goes to the part.
        for (off, text) in edits {
            let pos = s.text().offset_to_position(off);
            s.set_cursor_position(pos, window, cx);
            s.insert(text, window, cx);
        }
        let pos = s.text().offset_to_position(part_at);
        s.set_cursor_position(pos, window, cx);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(src: &str, file: &str) -> (String, usize) {
        let parsed = http_syntax::parse(src);
        let (edits, at) = attach(src, &parsed.requests[0], Path::new(file), "./a b.png");
        let mut out = src.to_string();
        for (off, text) in edits {
            out.insert_str(off, &text);
        }
        (out, at)
    }

    #[test]
    fn new_multipart_body() {
        let (out, at) = apply("### Up\nPOST /u\nX-A: 1\n", "/d/a b.png");
        assert_eq!(
            out,
            "### Up\nPOST /u\nX-A: 1\nContent-Type: multipart/form-data; boundary=RespiteFormBoundary\n\n--RespiteFormBoundary\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"a b.png\"\nContent-Type: image/png\n\n< ./a b.png\n--RespiteFormBoundary--\n"
        );
        assert!(out[at..].starts_with("--RespiteFormBoundary\n"));
        // Still a valid request: one multipart body with the file in it.
        let r = &http_syntax::parse(&out).requests[0];
        assert!(r.body.as_ref().unwrap().text.contains("< ./a b.png"));
    }

    #[test]
    fn appended_before_closing_boundary() {
        let src = "POST /u\nContent-Type: multipart/form-data; boundary=XY\n\n--XY\nContent-Disposition: form-data; name=\"t\"\n\nhi\n--XY--\n\n### Next\nGET /n\n";
        let (out, at) = apply(src, "/d/r.pdf");
        assert!(out.contains("hi\n--XY\nContent-Disposition: form-data; name=\"file\"; filename=\"r.pdf\"\nContent-Type: application/pdf\n\n< ./a b.png\n--XY--\n\n### Next"), "{out}");
        assert!(out[at..].starts_with("--XY\nContent-Disposition: form-data; name=\"file\""));
    }

    /// The same edits through a real editor (which is where they're applied in the app).
    #[gpui::test]
    fn in_the_editor(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, IntoElement, Render};
        use gpui_component::input::EditorState;
        struct Host;
        impl Render for Host {
            fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                gpui::div()
            }
        }
        let src = "GET /b\nContent-Type: multipart/form-data; boundary=boundary\n\n--boundary\nContent-Disposition: form-data; name=\"field\"\n\nvalue\n--boundary--\n### Next\nGET /n\n";
        let window = cx.update(|cx| {
            gpui_component::init(cx);
            cx.open_window(Default::default(), |_, cx| cx.new(|_| Host)).unwrap()
        });
        let out = window
            .update(cx, |_, window, cx| {
                let editor = cx.new(|cx| EditorState::new(window, cx).language("http").default_value(src));
                editor.update(cx, |s, cx| {
                    s.set_cursor_position(gpui_component::input::Position::new(6, 2), window, cx);
                    assert!(attach_files(s, &[PathBuf::from("/x/f.bin")], Path::new("/p"), window, cx));
                    s.value().to_string()
                })
            })
            .unwrap();
        assert!(out.contains("value\n--boundary\nContent-Disposition: form-data; name=\"file\"; filename=\"f.bin\""), "{out}");
    }

    #[test]
    fn after_a_text_field() {
        let src = "GET /b\nAuthorization: Bearer {{token}}\nContent-Type: multipart/form-data; boundary=boundary\n\n--boundary\nContent-Disposition: form-data; name=\"field\"\n\nvalue\n--boundary--\n";
        let (out, _) = apply(src, "/d/x.bin");
        assert!(out.contains("value\n--boundary\nContent-Disposition: form-data; name=\"file\""), "{out}");
    }

    #[test]
    fn raw_body_and_no_trailing_newline() {
        let (out, _) = apply("POST /u\nContent-Type: application/json", "/d/x.json");
        assert_eq!(out, "POST /u\nContent-Type: application/json\n\n< ./a b.png\n");
        assert_eq!(boundary("multipart/form-data; boundary=\"q\""), Some("q"));
        assert_eq!(file_ref(Path::new("/p/sub/f.txt"), Path::new("/p")), "./sub/f.txt");
        assert_eq!(file_ref(Path::new("/x/f.txt"), Path::new("/p")), "/x/f.txt");
    }
}
