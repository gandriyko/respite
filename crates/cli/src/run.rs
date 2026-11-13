//! `respite run`: execute requests, print human text or `respite.run/v1` JSON.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Args, ValueEnum};
use http_env::ResolveError;
use http_exec::{Context, Error, Exchange, SentRequest};
use http_syntax::Request;
use serde_json::{Map, Value, json};

use crate::{BOLD, CliError, DIM, EnvArgs, Format, GREEN, Loaded, RED, YELLOW, environment, find, load, paint, status_color};

const MASK: &str = "••••";
const SENSITIVE: [&str; 3] = ["authorization", "cookie", "x-api-key"];

#[derive(Args)]
pub(crate) struct RunArgs {
    file: PathBuf,
    /// Names, `:LINE` or `#N`; none = every request in file order
    requests: Vec<String>,
    #[command(flatten)]
    env: EnvArgs,
    #[arg(long, value_enum)]
    pub output: Option<Format>,
    /// Parts to print [default: body for human, all for JSON]
    #[arg(long, value_enum, value_delimiter = ',')]
    include: Vec<Part>,
    /// Cap the printed body, e.g. 64KB
    #[arg(long, value_parser = parse_size)]
    max_body: Option<usize>,
    /// Print only the JSONPath matches of a JSON body, e.g. '$.items[*].id'
    #[arg(long)]
    jsonpath: Option<String>,
    /// Stop at the first failure
    #[arg(long)]
    fail_fast: bool,
    /// Show the resolved request without sending it
    #[arg(long)]
    dry_run: bool,
    /// Never ask: missing @prompt values and @note confirmations become errors
    #[arg(long)]
    non_interactive: bool,
    /// Exit 1 when a response is 4xx/5xx
    #[arg(long)]
    fail_on_status: bool,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
enum Part {
    Headers,
    Body,
    Timing,
}

pub(crate) fn parse_size(s: &str) -> Result<usize, String> {
    let u = s.trim().to_ascii_uppercase();
    let (num, mult) = [("MB", 1 << 20), ("KB", 1 << 10), ("B", 1)]
        .iter()
        .find_map(|(suf, m)| u.strip_suffix(suf).map(|n| (n.to_string(), *m)))
        .unwrap_or((u.clone(), 1));
    num.trim().parse::<usize>().map(|n| n * mult).map_err(|_| format!("invalid size '{s}'"))
}

enum Outcome {
    Done(Box<Exchange>),
    Dry(SentRequest),
}

pub(crate) fn run(args: RunArgs) -> Result<u8, CliError> {
    let format = Format::resolve(args.output);
    let loaded = load(&args.file)?;
    let indices: Vec<usize> = if args.requests.is_empty() {
        (0..loaded.file.requests.len()).collect()
    } else {
        args.requests.iter().map(|a| find(&loaded, a)).collect::<Result<_, _>>()?
    };
    if indices.is_empty() {
        return Err(CliError::new("request_not_found", "no requests in file", 3).at(&args.file, None));
    }
    let include = match (&args.include[..], format) {
        ([], Format::Human) => vec![Part::Body],
        ([], _) => vec![Part::Headers, Part::Body, Part::Timing],
        (parts, _) => parts.to_vec(),
    };
    let (env, mut overrides) = environment(&loaded, &args.env)?;
    // ponytail: masks private-env and prompted values; $dotenv/$processEnv values are not tracked yet.
    let mut secrets: Vec<String> = env.values().filter(|v| v.secret && v.value.len() >= 4).map(|v| v.value.clone()).collect();
    let interactive = !args.non_interactive && std::io::stdin().is_terminal();
    let exec = crate::executor(&loaded.root, args.env.env.as_deref());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");

    let mut exit = 0;
    let mut items = vec![];
    for (n, &index) in indices.iter().enumerate() {
        let req = &loaded.file.requests[index];
        let outcome = ask(req, &loaded, &mut overrides, &mut secrets, interactive).and_then(|()| {
            let cx = Context { path: &loaded.path, file: &loaded.file, env: &env, overrides: &overrides };
            let result = if args.dry_run {
                exec.prepare(&cx, index).map(Outcome::Dry)
            } else {
                rt.block_on(exec.run(&cx, index)).map(|ex| Outcome::Done(Box::new(ex)))
            };
            result.map_err(|e| exec_error(e, &loaded, req))
        });
        let code = match &outcome {
            Ok(Outcome::Done(ex)) if args.fail_on_status && ex.response.status >= 400 => 1,
            Ok(Outcome::Done(ex)) if ex.tests.iter().any(|t| !t.passed) => 1,
            Ok(_) => 0,
            Err(e) => e.exit,
        };
        exit = exit.max(code);
        let warnings = warnings(&loaded, req);
        let out = Out { args: &args, include: &include, secrets: &secrets };
        match format {
            Format::Human => {
                if n > 0 {
                    println!();
                }
                out.human(req, &outcome, &warnings);
            }
            Format::Json | Format::Jsonl => {
                let item = out.json(req, &outcome, &warnings)?;
                if format == Format::Jsonl {
                    println!("{item}");
                } else {
                    items.push(item);
                }
            }
        }
        if code != 0 && args.fail_fast {
            break;
        }
    }
    if format == Format::Json {
        match <[Value; 1]>::try_from(items) {
            Ok([one]) => println!("{one}"),
            Err(many) => println!("{}", Value::Array(many)),
        }
    }
    Ok(exit)
}

/// `@prompt` values (hidden input for secret-looking names) and `@note` confirmation.
fn ask(
    req: &Request,
    loaded: &Loaded,
    overrides: &mut HashMap<String, String>,
    secrets: &mut Vec<String>,
    interactive: bool,
) -> Result<(), CliError> {
    let read_line = || {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).map(|_| s.trim_end_matches(['\r', '\n']).to_string())
    };
    for d in req.directives.iter().filter(|d| d.name == "prompt") {
        let value = d.value.as_deref().unwrap_or("");
        let (var, desc) = value.split_once(char::is_whitespace).map_or((value, ""), |(v, d)| (v, d.trim()));
        if var.is_empty() || overrides.contains_key(var) {
            continue;
        }
        if !interactive {
            return Err(CliError::new("undefined_variable", format!("@prompt '{var}' has no value"), 3)
                .at(&loaded.path, Some(d.line))
                .hint(format!("pass --var {var}=…")));
        }
        let label = if desc.is_empty() { format!("{var}: ") } else { format!("{var} ({desc}): ") };
        let lower = var.to_ascii_lowercase();
        let secret = ["pass", "secret", "token", "key"].iter().any(|s| lower.contains(s));
        let answer = if secret {
            rpassword::prompt_password(label)
        } else {
            eprint!("{label}");
            read_line()
        }
        .map_err(|e| CliError::new("prompt_failed", e.to_string(), 3))?;
        if secret && !answer.is_empty() {
            secrets.push(answer.clone());
        }
        overrides.insert(var.to_string(), answer);
    }
    if let Some(note) = req.directive("note") {
        let what = format!("{} {}", req.method, req.label());
        let refuse = |code, msg: &str| CliError::new(code, format!("{what}: {msg}"), 4).at(&loaded.path, Some(note.line));
        if !interactive {
            return Err(refuse("confirmation_required", "marked @note, needs confirmation").hint("run in a terminal to confirm"));
        }
        eprint!("{} {what}{}. Send? [y/N] ", paint("@note", YELLOW, true), note.value.as_deref().map(|v| format!(" — {v}")).unwrap_or_default());
        let yes = read_line().is_ok_and(|a| matches!(a.trim(), "y" | "Y" | "yes"));
        if !yes {
            return Err(refuse("cancelled", "not confirmed"));
        }
    }
    Ok(())
}

pub(crate) fn exec_error(e: Error, loaded: &Loaded, req: &Request) -> CliError {
    exec_error_at(e, &loaded.path, Some(req.request_line))
}

/// An executor error not tied to a request line (`respite auth`).
pub(crate) fn exec_error_plain(e: Error, path: &std::path::Path) -> CliError {
    exec_error_at(e, path, None)
}

fn exec_error_at(e: Error, path: &std::path::Path, line: Option<usize>) -> CliError {
    let at = |e: CliError| e.at(path, line);
    match e {
        Error::Auth { id, message } => {
            at(CliError::new("auth_failed", format!("auth '{id}': {message}"), 3).hint("see Security.Auth in the env files; `respite env show <env>`"))
        }
        Error::Resolve(r) => {
            let (code, hint) = match &r {
                ResolveError::Undefined(v) => ("undefined_variable", format!("define @{v} = … in the file, add it to an env file, or pass --var {v}=…")),
                ResolveError::Reference(v) => ("unresolved_reference", format!("run the request '{}' first", v.split('.').next().unwrap_or(v))),
                ResolveError::Cycle(_) => ("cyclic_variable", "break the cycle between file variables".into()),
                ResolveError::Dynamic(_) => ("invalid_dynamic_variable", "see `{{$…}}` syntax in the docs".into()),
            };
            at(CliError::new(code, r.to_string(), 3).hint(hint))
        }
        Error::File(p, io) => at(CliError::new("file_not_found", format!("{}: {io}", p.display()), 3)),
        Error::Http(h) if h.is_timeout() => at(CliError::new("timeout", h.to_string(), 2)),
        Error::Http(h) => at(CliError::new("network_error", error_chain(&h), 2)),
        Error::BadRequest(m) => at(CliError::new("bad_request", m, 3)),
        Error::DependencyCycle(n) => at(CliError::new("dependency_cycle", format!("request dependency cycle through '{n}'"), 3)),
    }
}

/// reqwest's top-level message hides the cause ("error sending request"); include it.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s += &format!(": {c}");
        src = c.source();
    }
    s
}

fn warnings(loaded: &Loaded, req: &Request) -> Vec<&'static str> {
    let mut w: Vec<&'static str> =
        loaded.file.diagnostics.iter().filter(|d| req.lines.contains(&d.line) && d.code == "scripts_not_supported").map(|d| d.code).collect();
    w.dedup();
    w
}

struct Out<'a> {
    args: &'a RunArgs,
    include: &'a [Part],
    secrets: &'a [String],
}

impl Out<'_> {
    fn has(&self, p: Part) -> bool {
        self.include.contains(&p)
    }

    fn mask(&self, s: &str) -> String {
        self.secrets.iter().fold(s.to_string(), |acc, sec| acc.replace(sec.as_str(), MASK))
    }

    /// Body for output: JSONPath matches, else JSON value / text, capped by `--max-body`.
    fn body(&self, body: &[u8], masked: bool, pretty: bool) -> Result<(Value, bool), CliError> {
        let text = String::from_utf8_lossy(body);
        let text = if masked { self.mask(&text) } else { text.into_owned() };
        let parsed = serde_json::from_str::<Value>(&text).ok();
        if let Some(jp) = &self.args.jsonpath {
            let path = serde_json_path::JsonPath::parse(jp)
                .map_err(|e| CliError::new("bad_argument", format!("--jsonpath: {e}"), 3))?;
            let json = parsed.ok_or_else(|| CliError::new("bad_argument", "--jsonpath needs a JSON body", 3))?;
            return Ok((Value::Array(path.query(&json).all().into_iter().cloned().collect()), false));
        }
        let max = self.args.max_body.unwrap_or(usize::MAX);
        if text.len() > max {
            return Ok((Value::String(truncate(&text, max).to_string()), true));
        }
        Ok(match parsed {
            Some(v) if pretty => (Value::String(serde_json::to_string_pretty(&v).unwrap_or(text)), false),
            Some(v) => (v, false),
            None => (Value::String(text), false),
        })
    }

    fn human(&self, req: &Request, outcome: &Result<Outcome, CliError>, warnings: &[&str]) {
        for w in warnings {
            eprintln!("{} {w}: scripts are not run in this version", paint("warning:", YELLOW, true));
        }
        // Imported blocks are titled "METHOD /path" already.
        let label = req.label();
        let title = if label.starts_with(&format!("{} ", req.method)) { label } else { format!("{} {label}", req.method) };
        let title = paint(&title, BOLD, false);
        match outcome {
            Err(e) => {
                println!("{title}");
                e.print(true);
            }
            Ok(Outcome::Dry(sent)) => {
                println!("{title}  {}", paint("(dry run)", DIM, false));
                println!("{} {}", sent.method, sent.url);
                sent.headers.iter().for_each(|(n, v)| println!("{n}: {v}"));
                if !sent.body.is_empty() {
                    println!("\n{}", String::from_utf8_lossy(&sent.body));
                }
            }
            Ok(Outcome::Done(ex)) => {
                let r = &ex.response;
                if !ex.dependencies.is_empty() {
                    println!("{}", paint(&format!("↳ ran first: {}", ex.dependencies.join(", ")), DIM, false));
                }
                let status = format!("{} {} · {} ms · {}", r.status, r.reason, r.total.as_millis(), size(r.body.len()));
                println!("{title}  {}", paint(&status, status_color(r.status), false));
                for t in &ex.tests {
                    match t.passed {
                        true => println!("{} {}", paint("✓", GREEN, false), t.text),
                        false => println!("{} {}  {}", paint("✗", RED, true), t.text, paint(&t.detail, DIM, false)),
                    }
                }
                if self.has(Part::Headers) {
                    r.headers.iter().for_each(|(n, v)| println!("{}", paint(&format!("{n}: {v}"), DIM, false)));
                }
                if self.has(Part::Timing) {
                    println!("{}", paint(&format!("ttfb {} ms · total {} ms · {}", r.ttfb.as_millis(), r.total.as_millis(), r.http_version), DIM, false));
                }
                if self.has(Part::Body) && !r.body.is_empty() {
                    match self.body(&r.body, false, true) {
                        Ok((Value::String(s), cut)) => {
                            println!("{s}");
                            if cut {
                                println!("{}", paint(&format!("… truncated ({} total)", size(r.body.len())), DIM, false));
                            }
                        }
                        Ok((v, _)) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
                        Err(e) => e.print(true),
                    }
                }
            }
        }
    }

    fn json(&self, req: &Request, outcome: &Result<Outcome, CliError>, warnings: &[&str]) -> Result<Value, CliError> {
        let mut item = json!({
            "schema": "respite.run/v1",
            "file": self.args.file.display().to_string(),
            "request": { "name": req.name, "line": req.request_line, "method": req.method },
            "warnings": warnings,
        });
        match outcome {
            Err(e) => {
                item["error"] = e.json()["error"].clone();
            }
            Ok(Outcome::Dry(sent)) => {
                self.request_json(&mut item, sent);
                item["request"]["body"] = Value::String(self.mask(&String::from_utf8_lossy(&sent.body)));
                item["dry_run"] = Value::Bool(true);
            }
            Ok(Outcome::Done(ex)) => {
                self.request_json(&mut item, &ex.request);
                let r = &ex.response;
                let mut res = json!({ "status": r.status, "reason": r.reason, "http_version": r.http_version, "size_bytes": r.body.len() });
                if self.has(Part::Headers) {
                    res["headers"] = Value::Object(self.headers(&r.headers, false));
                }
                if self.has(Part::Body) {
                    let (body, cut) = self.body(&r.body, true, false)?;
                    (res["body"], res["body_truncated"]) = (body, Value::Bool(cut));
                }
                item["response"] = res;
                if self.has(Part::Timing) {
                    item["timing_ms"] = json!({ "ttfb": r.ttfb.as_millis() as u64, "total": r.total.as_millis() as u64 });
                }
                item["tests"] = ex
                    .tests
                    .iter()
                    .map(|t| json!({ "line": t.line, "assert": t.text, "passed": t.passed, "detail": t.detail }))
                    .collect();
                item["dependencies"] = json!(ex.dependencies);
                item["history_id"] = json!(ex.history_id);
            }
        }
        Ok(item)
    }

    fn request_json(&self, item: &mut Value, sent: &SentRequest) {
        item["request"]["method"] = json!(sent.method);
        item["request"]["url"] = json!(self.mask(&sent.url));
        item["request"]["headers"] = Value::Object(self.headers(&sent.headers, true));
    }

    /// Repeated names joined with ", "; sensitive request headers masked.
    fn headers(&self, headers: &[(String, String)], request: bool) -> Map<String, Value> {
        let mut map = Map::new();
        for (n, v) in headers {
            let v = if request && SENSITIVE.iter().any(|s| n.eq_ignore_ascii_case(s)) { MASK.to_string() } else { self.mask(v) };
            match map.get_mut(n) {
                Some(Value::String(prev)) => *prev = format!("{prev}, {v}"),
                _ => {
                    map.insert(n.clone(), Value::String(v));
                }
            }
        }
        map
    }
}

fn size(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1 << 20 => format!("{:.1} KB", n as f64 / 1024.),
        n => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    let end = (0..=max.min(s.len())).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    &s[..end]
}
