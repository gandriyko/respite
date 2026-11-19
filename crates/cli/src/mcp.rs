//! `respite mcp`: a Model Context Protocol server on stdio (newline-delimited JSON-RPC).
//! Each tool runs this binary's CLI command with `--output json` and returns what it printed,
//! so tools behave exactly like the commands the skill documents.
//!
//! Secrets stay out of the results: every path must lie inside the workspace root (`.http` /
//! `.rest` where a request file is expected), and nothing that prints substituted values
//! unmasked is exposed (`show --resolved`, `export-curl` without `--raw`); `run` masks them.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Set for the child commands: history entries are tagged `mcp` instead of `cli`.
pub(crate) const SOURCE_ENV: &str = "RESPITE_HISTORY_SOURCE";

/// `tools/call` requests in progress: request id (JSON text) → child pid, once spawned.
type Calls = Arc<Mutex<HashMap<String, Option<u32>>>>;

/// `root` is canonical. Each `tools/call` runs on its own thread, so `notifications/cancelled` is
/// read while it runs: the child gets SIGTERM (`run` drops its request) and the call gets no
/// reply. When the client goes away (stdin closes), every child gets SIGTERM.
pub(crate) fn serve(root: &Path) -> u8 {
    let stdin = std::io::stdin();
    // Replies come from several threads: one lock per line keeps them whole. Nothing on the way
    // here may hold `stdout().lock()`, or the reply threads wait forever.
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let calls: Calls = Arc::default();
    let send = |out: &Mutex<std::io::Stdout>, r: Value| {
        let mut o = out.lock().unwrap();
        writeln!(o, "{r}").is_ok() && o.flush().is_ok()
    };
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => msg,
            Err(e) => {
                if !send(&out, json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})) {
                    break;
                }
                continue;
            }
        };
        match (msg["method"].as_str(), msg.get("id")) {
            (Some("notifications/cancelled"), _) => {
                // A call not spawned yet is stopped by its thread; a finished one is not in the map.
                if let Some(Some(pid)) = calls.lock().unwrap().remove(&msg["params"]["requestId"].to_string()) {
                    terminate(pid);
                }
            }
            (Some("tools/call"), Some(id)) => {
                let key = id.to_string();
                calls.lock().unwrap().insert(key.clone(), None);
                let (out, calls, root) = (out.clone(), calls.clone(), root.to_path_buf());
                std::thread::spawn(move || {
                    let on_spawn = |pid| match calls.lock().unwrap().get_mut(&key) {
                        Some(slot) => *slot = Some(pid),
                        None => terminate(pid),
                    };
                    let reply = handle(&msg, &root, &on_spawn);
                    // Gone from the map = cancelled: no reply.
                    if calls.lock().unwrap().remove(&key).is_some()
                        && let Some(r) = reply
                    {
                        send(&out, r);
                    }
                });
            }
            _ => {
                if let Some(r) = handle(&msg, root, &|_| {})
                    && !send(&out, r)
                {
                    break;
                }
            }
        }
    }
    for pid in calls.lock().unwrap().values().flatten() {
        terminate(*pid);
    }
    0
}

/// SIGTERM: `respite run` drops the request in flight (History marker included) and exits.
/// Not `Child::kill()`: that is SIGKILL, and the child could not clean up.
// ponytail: `kill` command instead of a libc dependency. A pid freed between the child's exit and
// its removal from `Calls` could be reused; that window is microseconds.
fn terminate(pid: u32) {
    let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
}

/// The reply to one message; `None` for notifications. `on_spawn` gets a tool's child pid.
fn handle(msg: &Value, root: &Path, on_spawn: &dyn Fn(u32)) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "respite", "version": env!("CARGO_PKG_VERSION")},
            "instructions": format!("Send HTTP requests and test APIs from .http files in the workspace {}. \
                Paths are relative to it and must stay inside it. Start with `list` and `env_list`; \
                check side-effecting requests with `run` and dry_run first; run `check` after editing a .http file. \
                `run` also sends a one-off request with no file (`curl`, or `method` + `url`); `add_request` saves a request \
                to a .http file. Running a whole file that has POST/PUT/PATCH/DELETE requests is refused: name the requests, \
                or ask the user and pass `all: true`. Requests marked `# @note` are refused: ask the user to run them.", root.display()),
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call(params["name"].as_str().unwrap_or(""), &params["arguments"], root, on_spawn),
        m => Err((-32601, format!("unknown method '{m}'"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

fn call(name: &str, args: &Value, root: &Path, on_spawn: &dyn Fn(u32)) -> Result<Value, (i64, String)> {
    let reply = |text: String, error: bool| Ok(json!({"content": [{"type": "text", "text": text}], "isError": error}));
    if name == "add_request" {
        return match add_request(args, root) {
            Ok(t) => reply(t, false),
            Err(e) => reply(e, true),
        };
    }
    let (argv, input) = match argv(name, args, root) {
        Ok(a) => a,
        // A rejected path is the tool's answer, not a protocol error: the agent reads it.
        Err(e) => return Ok(json!({"content": [{"type": "text", "text": e}], "isError": true})),
    };
    let mut child = Command::new(std::env::current_exe().map_err(|e| (-32603, e.to_string()))?)
        .args(&argv)
        .current_dir(root)
        .env(SOURCE_ENV, "mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| (-32603, e.to_string()))?;
    on_spawn(child.id());
    // Always close stdin: no command may wait for input.
    if let Some(mut s) = child.stdin.take() {
        let _ = s.write_all(input.unwrap_or_default().as_bytes());
    }
    let o = child.wait_with_output().map_err(|e| (-32603, e.to_string()))?;
    let code = o.status.code().unwrap_or(-1);
    let mut text = String::from_utf8_lossy(&o.stdout).trim_end().to_string();
    let err = String::from_utf8_lossy(&o.stderr);
    if !err.trim().is_empty() {
        text += &format!("\n\nstderr:\n{}", err.trim_end());
    }
    // `check` finding errors (exit 3) is its answer, not a failed call: the diagnostics are the result.
    let diagnostics = name == "check" && code == 3 && text.starts_with('{');
    if code != 0 && !diagnostics {
        text += &format!("\n\nexit code {code} (1 assert failed or 4xx/5xx · 2 network/timeout · 3 bad input · 4 needs confirmation)");
        if name == "run" && text.contains("\"code\":\"timeout\"") {
            text += "\nA slow server needs a longer limit: pass `timeout` (seconds).";
        }
    }
    if code == 0 {
        text = shape(name, args, &text, root);
    }
    reply(text, code != 0 && !diagnostics)
}

/// Compact `history` entries (`include` adds the rest) and a hint where a list comes back empty.
fn shape(name: &str, a: &Value, out: &str, root: &Path) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(out) else { return out.to_string() };
    let Some(items) = v.as_array_mut() else { return out.to_string() };
    let mut shaped = false;
    if name == "history" {
        let include = |k: &str| a.get("include").and_then(Value::as_array).is_some_and(|l| l.iter().any(|x| x == k));
        let mut keep = vec!["id", "time", "source", "file", "name", "method", "url", "status", "total_ms", "size_bytes"];
        if include("headers") {
            keep.extend(["request_headers", "response_headers", "cookies_sent"]);
        }
        if include("bodies") {
            keep.push("request_body");
        }
        for e in items.iter_mut().filter_map(Value::as_object_mut) {
            e.retain(|k, _| keep.contains(&k.as_str()));
        }
        shaped = true;
    }
    if v.as_array().is_some_and(Vec::is_empty) {
        let w = root.display();
        let hint = match name {
            "list" => format!("No requests found. Workspace: {w}. Create a .http file there (`add_request`), or send a one-off request with `run` (`curl`, or `method` + `url`)."),
            "env_list" => "No environments defined (respite-NAME.json files in the user's env folder, which the tools can't write). A request can still use literal URLs and `vars` on `run`.".to_string(),
            "history" => format!("No history yet: every `run` is recorded here once something has been sent. Workspace: {w}."),
            _ => return out.to_string(),
        };
        return format!("[]\n\n{hint}");
    }
    if shaped { v.to_string() } else { out.to_string() }
}

/// `p` resolved against `root` (symlinks and `..` included) if it stays inside; with `http`, a
/// file must be `.http` / `.rest`. A path that doesn't exist yet is checked through its parent.
fn inside(root: &Path, p: &str, http: bool) -> Result<String, String> {
    let path = root.join(p);
    let real = path.canonicalize().or_else(|e| match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => dir.canonicalize().map(|d| d.join(name)),
        _ => Err(e),
    });
    let real = real.map_err(|e| format!("'{p}': {e}"))?;
    if !real.starts_with(root) {
        return Err(format!("'{p}' is outside the workspace {}", root.display()));
    }
    if http && !real.is_dir() && !real.extension().is_some_and(|e| e == "http" || e == "rest") {
        return Err(format!("'{p}' is not a .http / .rest file"));
    }
    Ok(real.display().to_string())
}

/// CLI arguments for a tool call, plus stdin for the child.
fn argv(name: &str, a: &Value, root: &Path) -> Result<(Vec<String>, Option<String>), String> {
    let str_of = |k: &str| a.get(k).and_then(Value::as_str);
    let need = |k: &str| str_of(k).map(String::from).ok_or_else(|| format!("'{k}' is required"));
    let path = |k: &str, http: bool| -> Result<Option<String>, String> { str_of(k).map(|p| inside(root, p, http)).transpose() };
    let need_path = |k: &str, http: bool| path(k, http)?.ok_or_else(|| format!("'{k}' is required"));
    let list_of = |k: &str| -> Vec<String> {
        a.get(k).and_then(Value::as_array).map(|v| v.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    let text = |x: &Value| x.as_str().map_or_else(|| x.to_string(), String::from);
    let opt = |v: &mut Vec<String>, k: &str, flag: &str| {
        if let Some(x) = a.get(k).filter(|x| !x.is_null()) {
            v.extend([flag.into(), text(x)]);
        }
    };
    let flag = |v: &mut Vec<String>, k: &str, flag: &str| {
        if a.get(k).and_then(Value::as_bool) == Some(true) {
            v.push(flag.into());
        }
    };
    let flag_on = |k: &str| a.get(k).and_then(Value::as_bool) == Some(true);
    let json = ["--output".to_string(), "json".into()];
    let mut v: Vec<String> = vec![];
    let mut input = None;
    match name {
        "list" => {
            v.push("list".into());
            v.extend(path("path", true)?);
            v.extend(json);
        }
        "check" => {
            v.push("check".into());
            for p in list_of("paths") {
                v.push(inside(root, &p, true)?);
            }
            v.extend(json);
        }
        "show" => v.extend(["show".into(), need_path("file", true)?, need("request")?]),
        "run" => {
            let ad_hoc = ad_hoc_block(a)?;
            let named = list_of("requests");
            match &ad_hoc {
                Some(block) => {
                    if !named.is_empty() {
                        return Err("'requests' selects from a file; an ad-hoc request (curl / method + url) has just one".into());
                    }
                    v.extend(["run".into(), "-".into()]);
                    input = Some(block.clone());
                }
                None => {
                    let file = need_path("file", true)?;
                    if named.is_empty() && !flag_on("all") && !flag_on("dry_run") {
                        confirm_whole_file(&file, str_of("file").unwrap_or(&file))?;
                    }
                    v.extend(["run".into(), file]);
                    v.extend(named);
                }
            }
            v.push("--non-interactive".into());
            v.extend(json);
            opt(&mut v, "env", "--env");
            if let Some(vars) = a.get("vars").and_then(Value::as_object) {
                for (k, x) in vars {
                    v.extend(["--var".into(), format!("{k}={}", text(x))]);
                }
            }
            flag(&mut v, "dry_run", "--dry-run");
            flag(&mut v, "fail_fast", "--fail-fast");
            flag(&mut v, "fail_on_status", "--fail-on-status");
            opt(&mut v, "jsonpath", "--jsonpath");
            opt(&mut v, "max_body", "--max-body");
            if let Some(t) = timeout_secs(a)? {
                v.extend(["--timeout".into(), format!("{t}s")]);
            }
            // Response headers are mostly CDN noise; an agent asks for them with `include`.
            let mut include = list_of("include");
            if include.is_empty() {
                include = vec!["body".into(), "timing".into()];
            }
            v.extend(["--include".into(), include.join(",")]);
        }
        "export_curl" => v.extend(["export-curl".into(), need_path("file", true)?, need("request")?, "--raw".into()]),
        "import" => {
            // The CLI's default is the central folder, outside the workspace.
            let to = path("to", false)?.unwrap_or_else(|| root.display().to_string());
            v.extend(["import".into(), need_path("path", false)?, "--to".into(), to]);
            v.extend(json);
        }
        "import_curl" => {
            input = Some(need("curl")?);
            v.push("import-curl".into());
            if let Some(to) = path("to", true)? {
                v.extend(["--to".into(), to]);
            }
            opt(&mut v, "env", "--env");
            flag(&mut v, "no_original", "--no-original");
        }
        "env_list" => {
            v.extend(["env".into(), "list".into()]);
            v.extend(json);
        }
        "env_show" => {
            v.extend(["env".into(), "show".into(), need("name")?]);
            v.extend(json);
        }
        "history" => {
            v.push("history".into());
            opt(&mut v, "last", "--last");
            v.extend(json);
        }
        _ => return Err(format!("unknown tool '{name}'")),
    }
    Ok((v, input))
}

/// Methods that don't change anything on the server.
const SAFE: [&str; 3] = ["GET", "HEAD", "OPTIONS"];

/// Running a whole file sends every request in it: refuse (exit 4 semantics) when any of them
/// isn't GET / HEAD / OPTIONS, so the agent names the ones it means or asks the user.
fn confirm_whole_file(real: &str, shown: &str) -> Result<(), String> {
    let Ok(src) = std::fs::read_to_string(real) else { return Ok(()) }; // the CLI reports it
    let risky: Vec<String> = http_syntax::parse(&src)
        .requests
        .iter()
        .filter(|r| !SAFE.contains(&r.method.to_ascii_uppercase().as_str()))
        .map(|r| format!("{} {}", r.method, r.name.clone().unwrap_or_else(|| format!(":{}", r.request_line))))
        .collect();
    if risky.is_empty() {
        return Ok(());
    }
    Err(format!(
        "'{shown}' has requests that may change data ({}): running the whole file needs confirmation. \
         Name the requests to run in `requests`, or ask the user and pass `all: true`.\n\n\
         exit code 4 (needs confirmation)",
        risky.join(", ")
    ))
}

/// A one-off request as a .http block: from `curl`, or `method` + `url` (+ `headers`, `body`).
/// `None` when `file` is used. Exactly one way must be given.
fn ad_hoc_block(a: &Value) -> Result<Option<String>, String> {
    let given = |k: &str| a.get(k).is_some_and(|x| !x.is_null());
    let (file, curl, direct) = (given("file"), given("curl"), given("method") || given("url"));
    match [file, curl, direct].iter().filter(|g| **g).count() {
        0 => return Err("say what to run: `file` (a .http file), `curl` (a cURL command) or `method` + `url`".into()),
        1 => {}
        _ => return Err("give exactly one of `file`, `curl`, or `method` + `url`".into()),
    }
    if file {
        return Ok(None);
    }
    let block = if curl {
        let text = a["curl"].as_str().ok_or("'curl' must be a string")?;
        let s = crate::settings();
        let strip: Vec<&str> = s.curl.strip_headers.iter().map(String::as_str).collect();
        let opts = http_curl::Options { keep_original: false, strip_headers: &strip, ..Default::default() };
        let mut blocks = http_curl::import_all(text, &opts).map_err(|e| format!("invalid curl: {e}"))?;
        if blocks.len() != 1 {
            return Err(format!("'curl' must be one command, got {}", blocks.len()));
        }
        blocks.remove(0)
    } else {
        request_block(a, None, &[])?
    };
    // Nothing may reach the disk (`-o` becomes `>> file`) and no local file may be sent (`-d @file` becomes `< file`).
    let parsed = http_syntax::parse(&block);
    let [req] = &parsed.requests[..] else { return Err("expected exactly one request".into()) };
    if req.output.is_some() {
        return Err("an ad-hoc request can't save the response to a file (`-o` / `>>`); read it from the result".into());
    }
    if req.body.as_ref().is_some_and(|b| b.text.lines().any(|l| l.trim_start().starts_with('<'))) {
        return Err("an ad-hoc request can't send a local file (`< path` / `-d @file`); put the content in `body`".into());
    }
    Ok(Some(block))
}

/// One request block from `method` / `url` / `headers` / `body`, with `# @name`, a `###` title
/// and `# @assert` lines above the request line when given. Single-line fields only where the
/// format needs it, so no field can smuggle in a directive.
/// `timeout` argument: whole seconds, at least 1 (an agent can raise the limit, not remove it).
fn timeout_secs(a: &Value) -> Result<Option<u64>, String> {
    match a.get("timeout") {
        None | Some(Value::Null) => Ok(None),
        Some(t) => t.as_u64().filter(|&n| n > 0).map(Some).ok_or_else(|| format!("'timeout' must be whole seconds, 1 or more, got {t}")),
    }
}

fn request_block(a: &Value, name: Option<&str>, asserts: &[String]) -> Result<String, String> {
    let one_line = |what: &str, s: &str| -> Result<(), String> {
        if s.chars().any(char::is_control) {
            return Err(format!("'{what}' can't contain line breaks or control characters"));
        }
        Ok(())
    };
    let method = a.get("method").and_then(Value::as_str).unwrap_or("GET");
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(format!("'method' must be upper-case letters, got '{method}'"));
    }
    let url = a.get("url").and_then(Value::as_str).filter(|u| !u.trim().is_empty()).ok_or("'url' is required")?;
    one_line("url", url)?;
    let mut out = String::new();
    if let Some(t) = a.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        one_line("title", t)?;
        out += &format!("### {t}\n");
    }
    if let Some(n) = name {
        if n.is_empty() || n.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("'name' must be one word without whitespace".into());
        }
        out += &format!("# @name {n}\n");
    }
    if let Some(t) = timeout_secs(a)? {
        out += &format!("# @timeout {t} s\n");
    }
    for x in asserts {
        one_line("asserts", x)?;
        out += &format!("# @assert {}\n", x.trim().trim_start_matches("# @assert").trim());
    }
    out += &format!("{method} {url}\n");
    match a.get("headers") {
        None | Some(Value::Null) => {}
        Some(Value::Object(h)) => {
            for (k, x) in h {
                let x = x.as_str().map_or_else(|| x.to_string(), String::from);
                if k.is_empty() || k.contains(':') || k.contains(char::is_whitespace) {
                    return Err(format!("bad header name '{k}'"));
                }
                one_line("headers", k)?;
                one_line("headers", &x)?;
                out += &format!("{k}: {x}\n");
            }
        }
        Some(_) => return Err("'headers' must be an object of name: value".into()),
    }
    if let Some(b) = a.get("body").and_then(Value::as_str).filter(|b| !b.trim().is_empty()) {
        if b.lines().any(|l| l.starts_with("###")) {
            return Err("'body' can't contain a line starting with ###".into());
        }
        out += &format!("\n{}\n", b.trim_end());
    }
    Ok(out)
}

/// `add_request`: append a request block to a .http file in the workspace. The result must
/// pass `check` (no errors in the new block); otherwise nothing is written.
fn add_request(a: &Value, root: &Path) -> Result<String, String> {
    let shown = a.get("file").and_then(Value::as_str).ok_or("'file' is required")?;
    let real = inside(root, shown, true)?;
    let name = a.get("name").and_then(Value::as_str).ok_or("'name' is required")?;
    if a.get("method").is_none() || a.get("url").is_none() {
        return Err("'method' and 'url' are required".into());
    }
    let asserts: Vec<String> = match a.get("asserts") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(l)) => l.iter().map(|x| x.as_str().map(String::from).ok_or("'asserts' must be strings")).collect::<Result<_, _>>()?,
        Some(_) => return Err("'asserts' must be an array of strings".into()),
    };
    let block = request_block(a, Some(name), &asserts)?;
    let existing = std::fs::read_to_string(&real).unwrap_or_default();
    // Blocks are told apart by `###`; a title-less one still needs the line.
    let block = if !existing.trim().is_empty() && !block.starts_with("###") { format!("###\n{block}") } else { block };
    let sep = match existing.as_str() {
        "" => "",
        s if s.ends_with("\n\n") => "",
        s if s.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    let content = format!("{existing}{sep}{block}");

    let file = http_syntax::parse(&content);
    let dir = Path::new(&real).parent().unwrap_or(Path::new("."));
    let mut diags = file.diagnostics.clone();
    diags.extend(http_env::check(&file, &http_env::Environments::load(&[]), dir));
    // Problems already in the file are `check`'s business; the new block must add none.
    let new: Vec<_> = diags.iter().filter(|d| d.line > existing.lines().count()).collect();
    let line = |d: &&http_syntax::Diagnostic| format!("line {} [{}] {}", d.line, d.code, d.message);
    let errors: Vec<String> = new.iter().filter(|d| d.severity == http_syntax::Severity::Error).map(line).collect();
    if !errors.is_empty() {
        return Err(format!("not written, the request has errors:\n{}", errors.join("\n")));
    }
    let request_line = file.requests.iter().find(|r| r.name.as_deref() == Some(name)).map(|r| r.request_line);
    std::fs::write(&real, &content).map_err(|e| format!("'{shown}': {e}"))?;
    let warnings: Vec<String> = new.iter().map(line).collect();
    Ok(json!({"file": real, "name": name, "line": request_line, "warnings": warnings}).to_string())
}

fn tools() -> Value {
    let s = |d: &str| json!({"type": "string", "description": d});
    let b = |d: &str| json!({"type": "boolean", "description": d});
    let strs = |d: &str| json!({"type": "array", "items": {"type": "string"}, "description": d});
    let file = s("Path to the .http / .rest file");
    let request = s("Request: `# @name`, `###` title, `:LINE` (any line of the block) or `#N` (1-based)");
    let tool = |name: &str, d: &str, props: Value, required: &[&str], read_only: bool| {
        json!({"name": name, "description": d, "inputSchema": {"type": "object", "properties": props, "required": required},
               "annotations": {"readOnlyHint": read_only}})
    };
    json!([
        tool("list", "Requests in a .http file or every .http/.rest file under a directory: file, line, name, method, URL. An empty result says where to create one.",
            json!({"path": s("File or directory (default: the workspace)")}), &[], true),
        tool("check", "Diagnostics for .http files and their env files; sends nothing. Run after editing a .http file.",
            json!({"paths": strs("Files or directories (default: the workspace)")}), &[], true),
        tool("show", "A request block as written. To see it with variables substituted (secrets masked), use `run` with dry_run.",
            json!({"file": file, "request": request}), &["file", "request"], true),
        tool("run", "Send requests and return respite.run/v1 JSON: request, response (status, body; headers on `include`), \
            timing_ms, tests (`# @assert` results), dependencies. Say what to run in one of three ways: `file` (+ `requests`; none named = \
            the whole file in order, refused with exit code 4 if it has anything but GET/HEAD/OPTIONS unless `all: true`), `curl` (a cURL command), \
            or `method` + `url` (+ `headers`, `body`). curl / method+url send a one-off request that is written nowhere in the workspace \
            (History records it with an empty file); save a keeper with `add_request`. Secrets are masked. \
            `# @note` requests are refused; `# @prompt` values must come in `vars`.",
            json!({"file": s("Path to the .http / .rest file to run"), "requests": strs("With `file`: requests to run, each as in `show`"),
                   "all": b("With `file` and no `requests`: confirm that every request in the file may be sent. Pass it only after asking the user"),
                   "curl": s("Instead of `file`: a cURL command"),
                   "method": s("Instead of `file`: HTTP method, upper-case (default GET with `url`)"), "url": s("Instead of `file`: request URL"),
                   "headers": json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "With `method` + `url`: request headers"}),
                   "body": s("With `method` + `url`: request body"),
                   "env": s("Environment name (`dev` reads respite-dev.json); default: the file's `# @env`"),
                   "vars": json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "Variable overrides, highest priority"}),
                   "dry_run": b("Resolve and return the request without sending it"),
                   "fail_fast": b("Stop at the first failure"), "fail_on_status": b("Treat 4xx/5xx as failure"),
                   "jsonpath": s("Return only these JSONPath matches of a JSON body, e.g. $.items[*].id"),
                   "max_body": s("Cap the returned body, e.g. 64KB"),
                   "timeout": json!({"type": "integer", "minimum": 1, "description": "Seconds to wait for each request without `# @timeout` (default: the user's request.timeout_ms setting, else 60)"}),
                   "include": json!({"type": "array", "items": {"enum": ["headers", "body", "timing"]}, "description": "Parts to return (default: body, timing; add headers when needed)"})}),
            &[], false),
        tool("add_request", "Append a request block to a .http file in the workspace (created if missing; the folder must exist). \
            `asserts` become `# @assert` lines above the request line. The file is checked like `check` and nothing is written if the new block has errors. \
            Run it afterwards with `run`.",
            json!({"file": s("Path to the .http / .rest file"), "name": s("`# @name` of the request: one word"), "title": s("Optional `###` title"),
                   "method": s("HTTP method, upper-case"), "url": s("Request URL; `{{variables}}` allowed"),
                   "headers": json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "Request headers"}),
                   "body": s("Request body; set a Content-Type header for JSON"),
                   "asserts": strs("Assertions without the `# @assert` prefix, e.g. `status == 200`, `body $.id exists`"),
                   "timeout": json!({"type": "integer", "minimum": 1, "description": "Request timeout in seconds, written as `# @timeout N s`"})}),
            &["file", "name", "method", "url"], false),
        tool("export_curl", "A request as a cURL command, with {{variables}} kept as written.",
            json!({"file": file, "request": request}), &["file", "request"], true),
        tool("import", "Postman / Insomnia / OpenAPI / Swagger / Bruno folder / HAR → .http files in a new folder; its environments go to the user's env folder.",
            json!({"path": s("Export file, spec or Bruno collection folder"), "to": s("Where the new folder goes (default: the workspace)")}),
            &["path"], false),
        tool("import_curl", "cURL command(s) → request block(s): returned, or appended to `to`.",
            json!({"curl": s("One or more cURL commands"), "to": s("Append to this .http file instead of returning"),
                   "env": s("Replace this environment's values with {{name}}"), "no_original": b("Don't keep the command as a comment")}),
            &["curl"], false),
        tool("env_list", "Environment names. A .http file picks one with `# @env NAME` on its first line; `run` takes `env` to override it.",
            json!({}), &[], true),
        tool("env_show", "Merged variables of one environment with their source files; secrets masked.",
            json!({"name": s("Environment name")}), &["name"], true),
        tool("history", "Past executions of this workspace, newest first: id, time, source, file, name, method, url, status, total_ms, size_bytes. \
            `include` adds more per entry.",
            json!({"last": json!({"type": "integer", "description": "How many (default 20)"}),
                   "include": json!({"type": "array", "items": {"enum": ["headers", "bodies"]}, "description": "headers: request and response headers, cookies sent; bodies: request body"})}),
            &[], true),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_and_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.http"), "GET /a\n").unwrap();
        std::fs::write(root.join("respite-dev.private.json"), "{}").unwrap();
        let r = root.display().to_string();
        let handle = |m: &Value, root: &Path| handle(m, root, &|_| {});

        let init = handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}), &root).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), &root).is_none());
        assert_eq!(handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "nope"}), &root).unwrap()["error"]["code"], -32601);

        for t in tools().as_array().unwrap() {
            let a = json!({"file": "a.http", "request": "x", "path": ".", "name": "dev", "curl": "curl x"});
            if t["name"] == "add_request" {
                continue; // runs in-process, see `add_request_writes_checked_blocks`
            }
            let a = if t["name"] == "run" { json!({"file": "a.http"}) } else { a };
            assert!(argv(t["name"].as_str().unwrap(), &a, &root).is_ok(), "{t}");
        }
        let a = json!({"file": "a.http", "requests": ["login"], "env": "dev", "vars": {"id": "7"}, "dry_run": true, "include": ["body", "timing"]});
        assert_eq!(argv("run", &a, &root).unwrap().0.join(" "), format!("run {r}/a.http login --non-interactive --output json --env dev --var id=7 --dry-run --include body,timing"));
        let d = argv("run", &json!({"file": "a.http"}), &root).unwrap().0.join(" ");
        assert!(d.ends_with("--include body,timing"), "{d}");
        assert_eq!(argv("export_curl", &json!({"file": "a.http", "request": "#1"}), &root).unwrap().0.join(" "), format!("export-curl {r}/a.http #1 --raw"));
        assert!(argv("show", &json!({"file": "a.http"}), &root).is_err());

        // Secrets and files outside the workspace stay out of reach.
        for (tool, a) in [
            ("show", json!({"file": "respite-dev.private.json", "request": "#1"})),
            ("show", json!({"file": "../x.http", "request": "#1"})),
            ("show", json!({"file": "/etc/hosts", "request": "#1"})),
            ("list", json!({"path": "/"})),
            ("check", json!({"paths": ["respite-dev.private.json"]})),
            ("import", json!({"path": "a.http", "to": ".."})),
            ("import_curl", json!({"curl": "curl x", "to": "notes.txt"})),
        ] {
            assert!(argv(tool, &a, &root).is_err(), "{tool} {a}");
        }
        assert!(argv("import_curl", &json!({"curl": "curl x", "to": "new.http"}), &root).is_ok());
    }

    #[test]
    fn ad_hoc_and_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("safe.http"), "### A\nGET /a\n\n###\nHEAD /b\n").unwrap();
        std::fs::write(root.join("mixed.http"), "# @name a\nGET /a\n\n###\n# @name mk\nPOST /b\n").unwrap();
        let run = |a: Value| argv("run", &a, &root);

        // Whole file: safe methods run, anything else needs `all` or names; dry_run is never refused.
        assert!(run(json!({"file": "safe.http"})).is_ok());
        let e = run(json!({"file": "mixed.http"})).unwrap_err();
        assert!(e.contains("POST mk") && e.contains("all: true") && e.contains("exit code 4"), "{e}");
        assert!(run(json!({"file": "mixed.http", "all": true})).is_ok());
        assert!(run(json!({"file": "mixed.http", "dry_run": true})).is_ok());
        assert!(run(json!({"file": "mixed.http", "requests": ["mk"]})).is_ok());

        // Ad hoc: the block goes on stdin to `run -`; nothing is created in the workspace.
        let (v, input) = run(json!({"method": "POST", "url": "http://x/y", "headers": {"Accept": "a/b"}, "body": "{\"a\":1}", "jsonpath": "$.a"})).unwrap();
        assert!(v.join(" ").starts_with("run - --non-interactive --output json"), "{v:?}");
        assert!(v.contains(&"--jsonpath".to_string()));
        assert_eq!(input.unwrap(), "POST http://x/y\nAccept: a/b\n\n{\"a\":1}\n");
        let (v, input) = run(json!({"url": "http://x/", "dry_run": true})).unwrap();
        assert!((v.contains(&"--dry-run".to_string()), input.as_deref()) == (true, Some("GET http://x/\n")));
        let (_, input) = run(json!({"curl": "curl -X PUT http://x/z -H 'A: b' -d 'hi'"})).unwrap();
        let block = input.unwrap();
        assert!(block.contains("PUT http://x/z") && block.contains("A: b") && !block.contains("curl"), "{block}");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);

        // `timeout` raises the limit (`--timeout Ns`, and `# @timeout` in an ad-hoc block); it can't remove it.
        let (v, _) = run(json!({"file": "safe.http", "timeout": 120})).unwrap();
        assert!(v.join(" ").contains("--timeout 120s"), "{v:?}");
        let (_, input) = run(json!({"url": "http://x/", "timeout": 90})).unwrap();
        assert_eq!(input.unwrap(), "# @timeout 90 s\nGET http://x/\n");
        for t in [json!(0), json!(-1), json!(1.5), json!("2m")] {
            assert!(run(json!({"file": "safe.http", "timeout": t})).unwrap_err().contains("'timeout'"), "{t}");
        }

        // Exactly one way; no smuggled directives, files or disk writes.
        for a in [
            json!({}),
            json!({"file": "safe.http", "url": "http://x/"}),
            json!({"curl": "curl http://x/", "url": "http://x/"}),
            json!({"method": "GET"}),
            json!({"method": "get", "url": "http://x/"}),
            json!({"url": "http://x/\n# @no-log"}),
            json!({"url": "http://x/", "headers": {"A": "b\n# @no-log"}}),
            json!({"url": "http://x/", "body": "ok\n###\nGET /z"}),
            json!({"method": "POST", "url": "http://x/", "body": "< /etc/hosts"}),
            json!({"curl": "curl http://x/ -d @/etc/hosts"}),
            json!({"curl": "curl http://x/ -o out.bin"}),
            json!({"curl": "curl http://x/a\ncurl http://x/b"}),
            json!({"curl": "wget http://x/"}),
            json!({"url": "http://x/", "requests": ["a"]}),
        ] {
            assert!(run(a.clone()).is_err(), "{a}");
        }
    }

    #[test]
    fn results_are_shaped() {
        let root = Path::new("/ws");
        let entry = json!({"id": "i", "time": "t", "source": "mcp", "file": "", "name": null, "label": "l", "line": 1, "method": "GET", "url": "u",
            "request_headers": [["A", "b"]], "request_body": "{}", "status": 200, "reason": "OK", "http_version": "1.1",
            "response_headers": [["C", "d"]], "ttfb_ms": 1, "total_ms": 2, "size_bytes": 3, "dependencies": [], "response_url": "u", "redirects": [], "cookies_sent": []});
        let out = Value::Array(vec![entry]).to_string();
        let keys = |v: String| -> Vec<String> { serde_json::from_str::<Value>(&v).unwrap()[0].as_object().unwrap().keys().cloned().collect() };
        let compact = keys(shape("history", &json!({}), &out, root));
        assert_eq!(compact.len(), 10);
        assert!(["id", "time", "source", "file", "name", "method", "url", "status", "total_ms", "size_bytes"].iter().all(|k| compact.contains(&k.to_string())));
        let more = keys(shape("history", &json!({"include": ["headers", "bodies"]}), &out, root));
        assert!(["request_headers", "response_headers", "request_body"].iter().all(|k| more.contains(&k.to_string())) && !more.contains(&"label".to_string()));
        // Other tools' output passes through untouched.
        assert_eq!(shape("list", &json!({}), &out, root), out);
        for tool in ["list", "env_list", "history"] {
            let t = shape(tool, &json!({}), "[]", root);
            assert!(t.starts_with("[]\n\n") && t.len() > 20, "{tool}: {t}");
        }
        assert!(shape("list", &json!({}), "[]", root).contains("/ws") && shape("list", &json!({}), "[]", root).contains("add_request"));
        assert_eq!(shape("check", &json!({}), "[]", root), "[]");
    }

    #[test]
    fn add_request_writes_checked_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let add = |a: Value| add_request(&a, &root);
        let read = |f: &str| std::fs::read_to_string(root.join(f)).unwrap();

        let r = add(json!({"file": "n.http", "name": "list", "method": "GET", "url": "http://x/items", "asserts": ["status == 200", "# @assert body $.n exists"]})).unwrap();
        assert!(serde_json::from_str::<Value>(&r).unwrap()["line"] == 4, "{r}");
        assert_eq!(read("n.http"), "# @name list\n# @assert status == 200\n# @assert body $.n exists\nGET http://x/items\n");
        add(json!({"file": "n.http", "name": "mk", "title": "Create", "method": "POST", "url": "http://x/items",
            "headers": {"Content-Type": "application/json"}, "body": "{\"a\": 1}"})).unwrap();
        add(json!({"file": "n.http", "name": "other", "method": "DELETE", "url": "http://x/items/1"})).unwrap();
        let text = read("n.http");
        assert!(text.contains("\n\n### Create\n# @name mk\nPOST http://x/items\nContent-Type: application/json\n\n{\"a\": 1}\n\n###\n# @name other\nDELETE"), "{text}");
        let parsed = http_syntax::parse(&text);
        assert_eq!(parsed.requests.len(), 3);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);

        // Refused, and the file stays as it was: bad assert, bad JSON, duplicate name, bad input, bad path.
        let before = read("n.http");
        for a in [
            json!({"file": "n.http", "name": "x", "method": "GET", "url": "http://x/", "asserts": ["status ~ 1"]}),
            json!({"file": "n.http", "name": "x", "method": "POST", "url": "http://x/", "headers": {"Content-Type": "application/json"}, "body": "{,}"}),
            json!({"file": "n.http", "name": "list", "method": "GET", "url": "http://x/"}),
            json!({"file": "n.http", "name": "a b", "method": "GET", "url": "http://x/"}),
            json!({"file": "n.http", "name": "x", "method": "get", "url": "http://x/"}),
            json!({"file": "n.http", "name": "x", "method": "GET", "url": "http://x/\n# @note"}),
            json!({"file": "n.http", "name": "x", "method": "GET", "url": "http://x/", "asserts": ["status == 200\nGET /z"]}),
            json!({"file": "n.http", "name": "x", "method": "GET"}),
            json!({"file": "../x.http", "name": "x", "method": "GET", "url": "http://x/"}),
            json!({"file": "notes.txt", "name": "x", "method": "GET", "url": "http://x/"}),
            json!({"file": "gone/x.http", "name": "x", "method": "GET", "url": "http://x/"}),
        ] {
            assert!(add(a.clone()).is_err(), "{a}");
        }
        assert_eq!(read("n.http"), before);
        assert!(!root.join("notes.txt").exists());
    }
}
