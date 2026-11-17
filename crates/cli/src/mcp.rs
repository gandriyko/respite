//! `respite mcp`: a Model Context Protocol server on stdio (newline-delimited JSON-RPC).
//! Each tool runs this binary's CLI command with `--output json` and returns what it printed,
//! so tools behave exactly like the commands the skill documents.
//!
//! Secrets stay out of the results: every path must lie inside the workspace root (`.http` /
//! `.rest` where a request file is expected), and nothing that prints substituted values
//! unmasked is exposed (`show --resolved`, `export-curl` without `--raw`); `run` masks them.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Set for the child commands: history entries are tagged `mcp` instead of `cli`.
pub(crate) const SOURCE_ENV: &str = "RESPITE_HISTORY_SOURCE";

/// `root` is canonical.
pub(crate) fn serve(root: &Path) -> u8 {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg, root),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        if let Some(r) = reply
            && (writeln!(out, "{r}").is_err() || out.flush().is_err())
        {
            break;
        }
    }
    0
}

/// The reply to one message; `None` for notifications.
fn handle(msg: &Value, root: &Path) -> Option<Value> {
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
                Requests marked `# @note` are refused: ask the user to run them.", root.display()),
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call(params["name"].as_str().unwrap_or(""), &params["arguments"], root),
        m => Err((-32601, format!("unknown method '{m}'"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

fn call(name: &str, args: &Value, root: &Path) -> Result<Value, (i64, String)> {
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
    if code != 0 {
        text += &format!("\n\nexit code {code} (1 assert failed or 4xx/5xx · 2 network/timeout · 3 bad input · 4 needs confirmation)");
    }
    Ok(json!({"content": [{"type": "text", "text": text}], "isError": code != 0}))
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
            v.extend(["run".into(), need_path("file", true)?]);
            v.extend(list_of("requests"));
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
            let include = list_of("include");
            if !include.is_empty() {
                v.extend(["--include".into(), include.join(",")]);
            }
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
        tool("list", "Requests in a .http file or every .http/.rest file under a directory: file, line, name, method, URL.",
            json!({"path": s("File or directory (default: the workspace)")}), &[], true),
        tool("check", "Diagnostics for .http files and their env files; sends nothing. Run after editing a .http file.",
            json!({"paths": strs("Files or directories (default: the workspace)")}), &[], true),
        tool("show", "A request block as written. To see it with variables substituted (secrets masked), use `run` with dry_run.",
            json!({"file": file, "request": request}), &["file", "request"], true),
        tool("run", "Send requests (none named = all, in file order) and return respite.run/v1 JSON: request, response \
            (status, headers, body), timing_ms, tests (`# @assert` results), dependencies. Secrets are masked. \
            `# @note` requests are refused; `# @prompt` values must come in `vars`.",
            json!({"file": file, "requests": strs("Requests to run, each as in `show`"),
                   "env": s("Environment name (`dev` reads respite-dev.json); default: the file's `# @env`"),
                   "vars": json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "Variable overrides, highest priority"}),
                   "dry_run": b("Resolve and return the request without sending it"),
                   "fail_fast": b("Stop at the first failure"), "fail_on_status": b("Treat 4xx/5xx as failure"),
                   "jsonpath": s("Return only these JSONPath matches of a JSON body, e.g. $.items[*].id"),
                   "max_body": s("Cap the returned body, e.g. 64KB"),
                   "include": json!({"type": "array", "items": {"enum": ["headers", "body", "timing"]}, "description": "Parts to return (default: all)"})}),
            &["file"], false),
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
        tool("history", "Past executions of this workspace, newest first.",
            json!({"last": json!({"type": "integer", "description": "How many (default 20)"})}), &[], true),
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

        let init = handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}), &root).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), &root).is_none());
        assert_eq!(handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "nope"}), &root).unwrap()["error"]["code"], -32601);

        for t in tools().as_array().unwrap() {
            let a = json!({"file": "a.http", "request": "x", "path": ".", "name": "dev", "curl": "curl x"});
            assert!(argv(t["name"].as_str().unwrap(), &a, &root).is_ok(), "{t}");
        }
        let a = json!({"file": "a.http", "requests": ["login"], "env": "dev", "vars": {"id": "7"}, "dry_run": true, "include": ["body", "timing"]});
        assert_eq!(argv("run", &a, &root).unwrap().0.join(" "), format!("run {r}/a.http login --non-interactive --output json --env dev --var id=7 --dry-run --include body,timing"));
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
}
