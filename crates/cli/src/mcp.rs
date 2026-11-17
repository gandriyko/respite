//! `respite mcp`: a Model Context Protocol server on stdio (newline-delimited JSON-RPC).
//! Each tool runs this binary's CLI command with `--output json` and returns what it printed,
//! so tools behave exactly like the commands the skill documents.

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Set for the child commands: history entries are tagged `mcp` instead of `cli`.
pub(crate) const SOURCE_ENV: &str = "RESPITE_HISTORY_SOURCE";

pub(crate) fn serve() -> u8 {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg),
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
fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "respite", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Send HTTP requests and test APIs from .http files. Start with `list` and `env_list`; \
                check side-effecting requests with `show` (resolved) or `run` with dry_run first; \
                run `check` after editing a .http file. Requests marked `# @note` are refused: ask the user to run them.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call(params["name"].as_str().unwrap_or(""), &params["arguments"]),
        m => Err((-32601, format!("unknown method '{m}'"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

fn call(name: &str, args: &Value) -> Result<Value, (i64, String)> {
    let (argv, input) = argv(name, args).map_err(|e| (-32602, e))?;
    let mut child = Command::new(std::env::current_exe().map_err(|e| (-32603, e.to_string()))?)
        .args(&argv)
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

/// CLI arguments for a tool call, plus stdin for the child.
fn argv(name: &str, a: &Value) -> Result<(Vec<String>, Option<String>), String> {
    let str_of = |k: &str| a.get(k).and_then(Value::as_str);
    let need = |k: &str| str_of(k).map(String::from).ok_or_else(|| format!("'{k}' is required"));
    let list_of = |k: &str| -> Vec<String> {
        a.get(k).and_then(Value::as_array).map(|v| v.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
    };
    let mut v: Vec<String> = vec![];
    let opt = |v: &mut Vec<String>, k: &str, flag: &str| {
        if let Some(x) = a.get(k).filter(|x| !x.is_null()) {
            v.push(flag.into());
            v.push(x.as_str().map_or_else(|| x.to_string(), String::from));
        }
    };
    let flag = |v: &mut Vec<String>, k: &str, flag: &str| {
        if a.get(k).and_then(Value::as_bool) == Some(true) {
            v.push(flag.into());
        }
    };
    let env = |v: &mut Vec<String>| {
        opt(v, "env", "--env");
        if let Some(vars) = a.get("vars").and_then(Value::as_object) {
            for (k, x) in vars {
                v.extend(["--var".into(), format!("{k}={}", x.as_str().map_or_else(|| x.to_string(), String::from))]);
            }
        }
    };
    let json = ["--output".to_string(), "json".into()];
    let mut input = None;
    match name {
        "list" => {
            v.push("list".into());
            v.extend(str_of("path").map(String::from));
            v.extend(json);
        }
        "check" => {
            v.push("check".into());
            v.extend(list_of("paths"));
            v.extend(json);
        }
        "show" => {
            v.extend(["show".into(), need("file")?, need("request")?]);
            flag(&mut v, "resolved", "--resolved");
            env(&mut v);
        }
        "run" => {
            v.extend(["run".into(), need("file")?]);
            v.extend(list_of("requests"));
            v.push("--non-interactive".into());
            v.extend(json);
            env(&mut v);
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
        "export_curl" => {
            v.extend(["export-curl".into(), need("file")?, need("request")?]);
            flag(&mut v, "raw", "--raw");
            env(&mut v);
        }
        "import" => {
            v.extend(["import".into(), need("path")?]);
            opt(&mut v, "to", "--to");
            v.extend(json);
        }
        "import_curl" => {
            input = Some(need("curl")?);
            v.push("import-curl".into());
            opt(&mut v, "to", "--to");
            opt(&mut v, "env", "--env");
            flag(&mut v, "no_original", "--no-original");
        }
        "env_list" => {
            v.extend(["env".into(), "list".into()]);
            opt(&mut v, "dir", "--dir");
            v.extend(json);
        }
        "env_show" => {
            v.extend(["env".into(), "show".into(), need("name")?]);
            opt(&mut v, "dir", "--dir");
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
    let env = s("Environment name (`dev` reads respite-dev.json)");
    let vars = json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "Variable overrides, highest priority"});
    let tool = |name: &str, d: &str, props: Value, required: &[&str], read_only: bool| {
        json!({"name": name, "description": d, "inputSchema": {"type": "object", "properties": props, "required": required},
               "annotations": {"readOnlyHint": read_only}})
    };
    json!([
        tool("list", "Requests in a .http file or every .http/.rest file under a directory: file, line, name, method, URL.",
            json!({"path": s("File or directory (default: current directory)")}), &[], true),
        tool("check", "Diagnostics for .http files and their env files; sends nothing. Run after editing a .http file.",
            json!({"paths": strs("Files or directories (default: current directory)")}), &[], true),
        tool("show", "A request block as written, or with `resolved` the request after variable substitution (nothing is sent).",
            json!({"file": file, "request": request, "resolved": b("Substitute variables"), "env": env, "vars": vars}), &["file", "request"], true),
        tool("run", "Send requests (none named = all, in file order) and return respite.run/v1 JSON: request, response \
            (status, headers, body), timing_ms, tests (`# @assert` results), dependencies. Secrets are masked. \
            `# @note` requests are refused; `# @prompt` values must come in `vars`.",
            json!({"file": file, "requests": strs("Requests to run, each as in `show`"), "env": env, "vars": vars,
                   "dry_run": b("Resolve and return the request without sending it"),
                   "fail_fast": b("Stop at the first failure"), "fail_on_status": b("Treat 4xx/5xx as failure"),
                   "jsonpath": s("Return only these JSONPath matches of a JSON body, e.g. $.items[*].id"),
                   "max_body": s("Cap the returned body, e.g. 64KB"),
                   "include": json!({"type": "array", "items": {"enum": ["headers", "body", "timing"]}, "description": "Parts to return (default: all)"})}),
            &["file"], false),
        tool("export_curl", "A request as a cURL command (variables substituted unless `raw`).",
            json!({"file": file, "request": request, "raw": b("Keep {{variables}}"), "env": env, "vars": vars}), &["file", "request"], true),
        tool("import", "Postman / Insomnia / OpenAPI / Swagger / Bruno folder / HAR → .http files in a new folder.",
            json!({"path": s("Export file, spec or Bruno collection folder"), "to": s("Where the new folder goes (default: current directory)")}),
            &["path"], false),
        tool("import_curl", "cURL command(s) → request block(s): returned, or appended to `to`.",
            json!({"curl": s("One or more cURL commands"), "to": s("Append to this .http file instead of returning"),
                   "env": s("Replace this environment's values with {{name}}"), "no_original": b("Don't keep the command as a comment")}),
            &["curl"], false),
        tool("env_list", "Environment names visible from a directory.", json!({"dir": s("Directory (default: current)")}), &[], true),
        tool("env_show", "Merged variables of one environment with their source files; secrets masked.",
            json!({"name": s("Environment name"), "dir": s("Directory (default: current)")}), &["name"], true),
        tool("history", "Past executions of this workspace, newest first.",
            json!({"last": json!({"type": "integer", "description": "How many (default 20)"})}), &[], true),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_and_arguments() {
        let init = handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        assert_eq!(handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "nope"})).unwrap()["error"]["code"], -32601);

        let names: Vec<String> = tools().as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        for n in &names {
            let a = json!({"file": "a.http", "request": "x", "path": "p", "name": "dev", "curl": "curl x"});
            assert!(argv(n, &a).is_ok(), "{n}");
        }
        let (v, _) = argv("run", &json!({"file": "a.http", "requests": ["login"], "env": "dev", "vars": {"id": "7"}, "dry_run": true, "include": ["body", "timing"]})).unwrap();
        assert_eq!(v.join(" "), "run a.http login --non-interactive --output json --env dev --var id=7 --dry-run --include body,timing");
        assert_eq!(argv("history", &json!({"last": 5})).unwrap().0.join(" "), "history --last 5 --output json");
        assert!(argv("show", &json!({"file": "a.http"})).is_err());
    }
}
