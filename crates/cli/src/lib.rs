//! `respite <command>`: the same core as the GUI, for terminals, CI and agents.
//! Exit codes: 0 ok · 1 failed checks (`# @assert`, `--fail-on-status`) · 2 network/timeout ·
//! 3 parse error / undefined variable / bad input · 4 refused (confirmation needed).

mod curl;
mod mcp;
mod run;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use http_env::{Environments, Variable};
use http_exec::Store;
pub use respite_settings::Settings;
use http_syntax::{HttpFile, Severity};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "respite", version, about = "Text-first REST client for .http files")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List requests: file, line, name, method, URL
    List {
        /// .http/.rest file or directory (default: current directory)
        path: Option<PathBuf>,
        #[arg(long, value_enum)]
        output: Option<Format>,
    },
    /// Print a request block; with --resolved, after variable substitution
    Show {
        file: PathBuf,
        /// Name, `:LINE` or `#N` (1-based)
        request: String,
        #[arg(long)]
        resolved: bool,
        #[command(flatten)]
        env: EnvArgs,
    },
    /// Run requests (all in file order when none are given)
    Run(run::RunArgs),
    /// cURL command(s) from stdin or the clipboard → request block(s)
    ImportCurl(curl::ImportArgs),
    /// Request → cURL command (variables substituted unless --raw)
    ExportCurl(curl::ExportArgs),
    /// Postman / Insomnia / OpenAPI / Swagger / Bruno / HAR → .http files in a new folder,
    /// environments into ~/.config/respite/env
    Import {
        /// Export file, spec, or Bruno collection folder
        path: PathBuf,
        /// Where the new folder goes (default: ~/.config/respite/collections)
        #[arg(long)]
        to: Option<PathBuf>,
        #[arg(long, value_enum)]
        output: Option<Format>,
    },
    /// Diagnostics without sending anything (pre-commit / CI)
    Check {
        /// Files or directories (default: current directory)
        paths: Vec<PathBuf>,
        #[arg(long, value_enum)]
        output: Option<Format>,
    },
    /// Environments from respite-<env>.json
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// OAuth 2.0 tokens of `Security.Auth` configs in the env files
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// Past executions of this workspace
    History {
        #[arg(long, default_value_t = 20)]
        last: usize,
        #[arg(long, value_enum)]
        output: Option<Format>,
    },
    /// MCP server on stdio for AI agents (tools: list, run, check, …)
    Mcp {
        /// Workspace the tools may touch; relative paths start here (default: current directory)
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum EnvCmd {
    /// Environment names (env files in ~/.config/respite/env)
    List {
        #[arg(long, value_enum)]
        output: Option<Format>,
    },
    /// Merged variables of one environment (secrets masked)
    Show {
        name: String,
        #[arg(long, value_enum)]
        output: Option<Format>,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Print a token: cached while fresh, else refreshed or acquired
    Token {
        /// Config id: `api` for `{{$auth.token("api")}}`
        id: String,
        /// Skip the cache and the refresh token: run the full grant
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        env: EnvArgs,
        #[arg(long, value_enum)]
        output: Option<Format>,
    },
    /// Forget the cached tokens of one environment
    Clear {
        /// Environment name; none = the tokens used without one
        #[arg(long)]
        env: Option<String>,
    },
}

#[derive(Args, Clone, Default)]
pub(crate) struct EnvArgs {
    /// Environment name (`dev` for respite-dev.json); default: the file's `# @env`
    #[arg(long)]
    pub env: Option<String>,
    /// Override a variable, any level (repeatable)
    #[arg(long = "var", value_name = "KEY=VALUE")]
    pub vars: Vec<String>,
    /// Extra env file over the central ones, named respite-<env>.json (repeatable)
    #[arg(long = "env-file", value_name = "PATH")]
    pub env_files: Vec<PathBuf>,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Human,
    Json,
    Jsonl,
}

impl Format {
    /// `human` on a terminal, `json` in a pipe.
    pub fn resolve(f: Option<Format>) -> Format {
        f.unwrap_or(if std::io::stdout().is_terminal() { Format::Human } else { Format::Json })
    }
}

/// The GUI binary calls this first; `None` means "not a CLI command, open the GUI".
pub fn main_if_cli() -> Option<ExitCode> {
    const COMMANDS: &[&str] = &[
        "list", "show", "run", "import", "import-curl", "export-curl", "check", "env", "auth", "history", "mcp", "help", "--help", "-h", "--version",
        "-V",
    ];
    let first = std::env::args().nth(1)?;
    if !COMMANDS.contains(&first.as_str()) {
        return None;
    }
    let cli = Cli::parse();
    let human = match &cli.cmd {
        Cmd::List { output, .. } | Cmd::Check { output, .. } | Cmd::History { output, .. } | Cmd::Import { output, .. } => {
            Format::resolve(*output)
        }
        Cmd::Env { cmd: EnvCmd::List { output, .. } | EnvCmd::Show { output, .. } } => Format::resolve(*output),
        Cmd::Auth { cmd: AuthCmd::Token { output, .. } } => Format::resolve(*output),
        Cmd::Run(a) => Format::resolve(a.output),
        Cmd::Show { .. } | Cmd::Mcp { .. } | Cmd::ImportCurl(_) | Cmd::ExportCurl(_) | Cmd::Auth { cmd: AuthCmd::Clear { .. } } => Format::Human,
    } == Format::Human;
    Some(match dispatch(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            e.print(human);
            ExitCode::from(e.exit)
        }
    })
}

fn dispatch(cli: Cli) -> Result<u8, CliError> {
    match cli.cmd {
        Cmd::List { path, output } => list(&path.unwrap_or_else(|| ".".into()), Format::resolve(output)),
        Cmd::Show { file, request, resolved, env } => show(&file, &request, resolved, &env),
        Cmd::Run(args) => run::run(args),
        Cmd::ImportCurl(args) => curl::import(args),
        Cmd::ExportCurl(args) => curl::export(args),
        Cmd::Import { path, to, output } => import(&path, to, Format::resolve(output)),
        Cmd::Check { paths, output } => check(&paths, Format::resolve(output)),
        Cmd::Env { cmd: EnvCmd::List { output } } => env_list(Format::resolve(output)),
        Cmd::Env { cmd: EnvCmd::Show { name, output } } => env_show(&name, Format::resolve(output)),
        Cmd::Auth { cmd: AuthCmd::Token { id, force, env, output } } => auth_token(&id, force, &env, Format::resolve(output)),
        Cmd::Auth { cmd: AuthCmd::Clear { env } } => {
            executor(env.as_deref()).clear_tokens();
            Ok(0)
        }
        Cmd::History { last, output } => history(last, Format::resolve(output)),
        Cmd::Mcp { root } => {
            let root = root.unwrap_or_else(|| ".".into());
            let root = root.canonicalize().map_err(|e| CliError::new("bad_argument", format!("--root: {e}"), 3).at(&root, None))?;
            Ok(mcp::serve(&root))
        }
    }
}

/// `respite auth token`: the token is printed in full, the user asked for it.
fn auth_token(id: &str, force: bool, args: &EnvArgs, format: Format) -> Result<u8, CliError> {
    let path = std::path::absolute("_.http").unwrap_or_default();
    let loaded = Loaded { path, file: Default::default(), src: String::new() };
    let (env, overrides, name) = environment(&loaded, args)?;
    let exec = executor(name.as_deref());
    let cx = http_exec::Context { path: &loaded.path, file: &loaded.file, env: &env, overrides: &overrides };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let t = rt.block_on(exec.auth_token(&cx, id, force)).map_err(|e| run::exec_error_plain(e, &loaded.path))?;
    let expires = t.expires_at.and_then(|s| jiff::Timestamp::from_second(s).ok()).map(|ts| ts.to_string());
    match format {
        Format::Human => {
            println!("{}", t.access_token);
            let meta = [expires.map(|e| format!("expires {e}")), t.scope.clone().map(|s| format!("scope {s}"))];
            let meta: Vec<String> = meta.into_iter().flatten().collect();
            if !meta.is_empty() {
                eprintln!("{}", paint(&meta.join(" · "), DIM, false));
            }
        }
        _ => println!(
            "{}",
            json!({"id": id, "access_token": t.access_token, "id_token": t.id_token, "expires_at": expires, "scope": t.scope, "has_refresh_token": t.refresh_token.is_some()})
        ),
    }
    Ok(0)
}

fn import(path: &Path, to: Option<PathBuf>, format: Format) -> Result<u8, CliError> {
    let c = http_import::import(path).map_err(|e| CliError::new("import_failed", e, 3))?;
    let no_home = || CliError::new("io_error", "no home directory", 3);
    let to = to.or_else(http_env::collections_dir).ok_or_else(no_home)?;
    let env_dir = http_env::env_dir().ok_or_else(no_home)?;
    let files = http_import::write(&c, &to, &env_dir).map_err(|e| CliError::new("io_error", e.to_string(), 3).at(&to, None))?;
    if format == Format::Human {
        println!("Imported {} '{}':", c.format, c.name);
        for f in &files {
            println!("  {}", f.display());
        }
        for w in &c.warnings {
            eprintln!("  warning: {w}");
        }
    } else {
        let files: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
        println!("{}", json!({ "format": c.format, "name": c.name, "files": files, "warnings": c.warnings }));
    }
    Ok(0)
}

/// `{"error":{"code","message","file","line","hint"}}` in JSON mode, `error: …` otherwise.
#[derive(Debug)]
pub(crate) struct CliError {
    pub code: &'static str,
    pub message: String,
    pub file: Option<PathBuf>,
    pub line: Option<usize>,
    pub hint: Option<String>,
    pub exit: u8,
}

impl CliError {
    pub fn new(code: &'static str, message: impl Into<String>, exit: u8) -> Self {
        Self { code, message: message.into(), file: None, line: None, hint: None, exit }
    }

    pub fn at(mut self, file: &Path, line: Option<usize>) -> Self {
        (self.file, self.line) = (Some(file.to_path_buf()), line);
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn json(&self) -> Value {
        json!({ "error": {
            "code": self.code, "message": self.message,
            "file": self.file.as_ref().map(|f| f.display().to_string()), "line": self.line, "hint": self.hint,
        }})
    }

    pub fn print(&self, human: bool) {
        if !human {
            println!("{}", self.json());
            return;
        }
        let at = match (&self.file, self.line) {
            (Some(f), Some(l)) => format!("{}:{l}: ", f.display()),
            (Some(f), None) => format!("{}: ", f.display()),
            _ => String::new(),
        };
        eprintln!("{} {at}{}", paint("error:", RED, true), self.message);
        if let Some(h) = &self.hint {
            eprintln!("  hint: {h}");
        }
    }
}

pub(crate) const RED: &str = "31";
pub(crate) const GREEN: &str = "32";
pub(crate) const YELLOW: &str = "33";
pub(crate) const DIM: &str = "2";
pub(crate) const BOLD: &str = "1";

/// ANSI color when writing to a terminal (stderr when `err`).
pub(crate) fn paint(s: &str, code: &str, err: bool) -> String {
    let tty = if err { std::io::stderr().is_terminal() } else { std::io::stdout().is_terminal() };
    if tty && std::env::var_os("NO_COLOR").is_none() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub(crate) struct Loaded {
    pub path: PathBuf,
    pub file: HttpFile,
    pub src: String,
}

pub(crate) fn load(path: &Path) -> Result<Loaded, CliError> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| CliError::new("file_not_found", e.to_string(), 3).at(path, None))?;
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    Ok(Loaded { file: http_syntax::parse(&src), path, src })
}

/// Request address: `name` (or `###` title), `:LINE`, `#N`.
pub(crate) fn find(loaded: &Loaded, addr: &str) -> Result<usize, CliError> {
    let reqs = &loaded.file.requests;
    let found = if let Some(line) = addr.strip_prefix(':') {
        line.parse().ok().and_then(|l: usize| reqs.iter().position(|r| r.lines.contains(&l)))
    } else if let Some(n) = addr.strip_prefix('#') {
        n.parse::<usize>().ok().filter(|n| (1..=reqs.len()).contains(n)).map(|n| n - 1)
    } else {
        reqs.iter()
            .position(|r| r.name.as_deref() == Some(addr))
            .or_else(|| reqs.iter().position(|r| r.title.as_deref() == Some(addr)))
    };
    found.ok_or_else(|| {
        let known: Vec<_> = reqs.iter().filter_map(|r| r.name.as_deref()).collect();
        CliError::new("request_not_found", format!("no request '{addr}'"), 3)
            .at(&loaded.path, None)
            .hint(format!("use a name ({}), :LINE or #N (1..={})", known.join(", "), reqs.len()))
    })
}

pub(crate) type Vars = HashMap<String, Variable>;
pub(crate) type Overrides = HashMap<String, String>;

/// Merged variables of the environment (`--env`, else the file's `# @env`), `--var` overrides,
/// and the environment's name. Env-file errors go to stderr.
pub(crate) fn environment(loaded: &Loaded, args: &EnvArgs) -> Result<(Vars, Overrides, Option<String>), CliError> {
    let envs = Environments::load(&args.env_files);
    for (p, e) in &envs.errors {
        eprintln!("{} {}: {e}", paint("warning:", YELLOW, true), p.display());
    }
    let name = args.env.clone().or_else(|| loaded.file.env.clone());
    if let Some(name) = &name
        && !envs.names().contains(name)
    {
        let mut e = CliError::new("unknown_environment", format!("no environment '{name}'"), 3)
            .hint(format!("available: {} (env files in {})", envs.names().join(", "), env_dir_shown()));
        if args.env.is_none() {
            e = e.at(&loaded.path, Some(1));
        }
        return Err(e);
    }
    let overrides = args
        .vars
        .iter()
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| CliError::new("bad_argument", format!("--var expects KEY=VALUE, got '{kv}'"), 3))
        })
        .collect::<Result<_, _>>()?;
    Ok((envs.variables(name.as_deref()), overrides, name))
}

fn env_dir_shown() -> String {
    http_env::env_dir().map_or_else(|| "~/.config/respite/env".into(), |d| d.display().to_string())
}

/// Settings; problems in the file go to stderr.
pub(crate) fn settings() -> Settings {
    let loaded = respite_settings::Loaded::load();
    for e in &loaded.errors {
        eprintln!("{} {e}", paint("warning:", YELLOW, true));
    }
    loaded.settings
}

/// Request defaults from `request.*` settings.
pub fn exec_options(s: &Settings) -> http_exec::Options {
    http_exec::Options {
        timeout: (s.request.timeout_ms > 0).then(|| std::time::Duration::from_millis(s.request.timeout_ms)),
        follow_redirects: s.request.follow_redirects,
        max_redirects: s.request.max_redirects,
        insecure: s.request.insecure,
        proxy: Some(s.request.proxy.trim().to_string()).filter(|p| !p.is_empty()),
        no_proxy: s.request.no_proxy.clone(),
    }
}

/// Executor with history (limits from settings) and request defaults.
pub(crate) fn executor(env: Option<&str>) -> http_exec::Executor {
    let s = settings();
    let source = if std::env::var(mcp::SOURCE_ENV).as_deref() == Ok("mcp") { "mcp" } else { "cli" };
    let exec = http_exec::Executor::new(open_store(&s), env, source);
    exec.set_options(exec_options(&s));
    // Browser and device sign-ins: stdout stays the result, the instructions go to stderr.
    exec.set_auth_ui(http_exec::AuthUi {
        notice: std::sync::Arc::new(|m| eprintln!("{} {m}", paint("auth:", YELLOW, true))),
        ..Default::default()
    });
    exec
}

/// `.http` / `.rest` files under `path` (respects .gitignore), or `path` itself.
fn http_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut files: Vec<PathBuf> = ignore::Walk::new(path)
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p.extension().is_some_and(|e| e == "http" || e == "rest"))
        .collect();
    files.sort();
    files
}

fn list(path: &Path, format: Format) -> Result<u8, CliError> {
    let mut rows = vec![];
    for f in http_files(path) {
        let loaded = load(&f)?;
        for r in &loaded.file.requests {
            rows.push(json!({
                "file": f.display().to_string(), "line": r.request_line, "name": r.name,
                "title": r.title, "method": r.method, "url": r.url,
            }));
        }
    }
    match format {
        Format::Human => {
            for r in &rows {
                let loc = format!("{}:{}", r["file"].as_str().unwrap(), r["line"]);
                let label = r["name"].as_str().or(r["title"].as_str()).unwrap_or("");
                println!("{}  {:<7} {}  {}", paint(&loc, DIM, false), r["method"].as_str().unwrap(), paint(label, BOLD, false), r["url"].as_str().unwrap());
            }
        }
        Format::Json => println!("{}", Value::Array(rows)),
        Format::Jsonl => rows.iter().for_each(|r| println!("{r}")),
    }
    Ok(0)
}

fn show(path: &Path, addr: &str, resolved: bool, args: &EnvArgs) -> Result<u8, CliError> {
    let loaded = load(path)?;
    let index = find(&loaded, addr)?;
    let r = &loaded.file.requests[index];
    if !resolved {
        let lines: Vec<&str> = loaded.src.lines().collect();
        let mut block = &lines[r.lines.start() - 1..*r.lines.end()];
        while let [rest @ .., last] = block
            && last.trim().is_empty()
        {
            block = rest;
        }
        println!("{}", block.join("\n"));
        return Ok(0);
    }
    let (env, overrides, name) = environment(&loaded, args)?;
    let exec = executor(name.as_deref());
    let cx = http_exec::Context { path: &loaded.path, file: &loaded.file, env: &env, overrides: &overrides };
    let sent = exec.prepare(&cx, index).map_err(|e| run::exec_error(e, &loaded, r))?;
    println!("{} {}", sent.method, sent.url);
    for (n, v) in &sent.headers {
        println!("{n}: {v}");
    }
    if !sent.body.is_empty() {
        println!("\n{}", String::from_utf8_lossy(&sent.body));
    }
    Ok(0)
}

/// One history for everything: its root is the central folder, so `Entry::file` is relative
/// there and absolute for files elsewhere. The app opens the same one.
pub fn open_store(s: &Settings) -> Option<Store> {
    let mut store = Store::open(&Store::default_base()?, &http_env::collections_dir()?)
        .inspect_err(|e| eprintln!("{} history disabled: {e}", paint("warning:", YELLOW, true)))
        .ok()?;
    (store.per_request, store.total) = (s.history.per_request, s.history.total);
    Some(store)
}

fn check(paths: &[PathBuf], format: Format) -> Result<u8, CliError> {
    let paths = if paths.is_empty() { vec![PathBuf::from(".")] } else { paths.to_vec() };
    let mut rows = vec![];
    let envs = Environments::load(&[]);
    for f in paths.iter().flat_map(|p| http_files(p)) {
        let loaded = load(&f)?;
        let dir = loaded.path.parent().unwrap_or(Path::new("."));
        let mut diags = loaded.file.diagnostics.clone();
        diags.extend(http_env::check(&loaded.file, &envs, dir));
        diags.sort_by_key(|d| d.line);
        for d in diags {
            rows.push((f.display().to_string(), d.line, d.severity, d.code, d.message));
        }
    }
    for (p, e) in &envs.errors {
        rows.push((p.display().to_string(), 1, Severity::Error, "invalid_env_file", e.clone()));
    }
    let errors = rows.iter().filter(|r| r.2 == Severity::Error).count();
    let sev = |s: Severity| if s == Severity::Error { "error" } else { "warning" };
    match format {
        Format::Human => {
            for (file, line, s, code, msg) in &rows {
                let label = paint(&format!("{}[{code}]", sev(*s)), if *s == Severity::Error { RED } else { YELLOW }, false);
                println!("{file}:{line}: {label} {msg}");
            }
            let summary = format!("{errors} error(s), {} warning(s)", rows.len() - errors);
            println!("{}", paint(&summary, if errors > 0 { RED } else { GREEN }, false));
        }
        _ => {
            let items: Vec<Value> = rows
                .iter()
                .map(|(file, line, s, code, msg)| json!({"file": file, "line": line, "severity": sev(*s), "code": code, "message": msg}))
                .collect();
            match format {
                Format::Jsonl => items.iter().for_each(|i| println!("{i}")),
                _ => println!("{}", json!({"schema": "respite.check/v1", "errors": errors, "diagnostics": items})),
            }
        }
    }
    Ok(if errors > 0 { 3 } else { 0 })
}

fn env_list(format: Format) -> Result<u8, CliError> {
    let names = Environments::load(&[]).names();
    match format {
        Format::Human => names.iter().for_each(|n| println!("{n}")),
        _ => println!("{}", json!(names)),
    }
    Ok(0)
}

fn env_show(name: &str, format: Format) -> Result<u8, CliError> {
    let envs = Environments::load(&[]);
    if !envs.names().iter().any(|n| n == name) {
        return Err(CliError::new("unknown_environment", format!("no environment '{name}'"), 3)
            .hint(format!("available: {} (env files in {})", envs.names().join(", "), env_dir_shown())));
    }
    let mut vars: Vec<_> = envs.variables(Some(name)).into_iter().collect();
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    let shown = |v: &Variable| if v.secret { "••••".to_string() } else { v.value.clone() };
    match format {
        Format::Human => {
            for (k, v) in &vars {
                println!("{k} = {}  {}", shown(v), paint(&v.source.display().to_string(), DIM, false));
            }
        }
        _ => {
            let obj: serde_json::Map<String, Value> = vars
                .iter()
                .map(|(k, v)| (k.clone(), json!({"value": shown(v), "secret": v.secret, "source": v.source.display().to_string()})))
                .collect();
            println!("{}", Value::Object(obj));
        }
    }
    Ok(0)
}

pub(crate) fn status_color(status: u16) -> &'static str {
    match status {
        200..=299 => GREEN,
        300..=499 => YELLOW,
        _ => RED,
    }
}

fn history(last: usize, format: Format) -> Result<u8, CliError> {
    let entries: Vec<_> = open_store(&settings()).map(|s| s.list()).unwrap_or_default().into_iter().take(last).collect();
    match format {
        Format::Human => {
            for e in &entries {
                let status = paint(&e.status.to_string(), status_color(e.status), false);
                println!("{}  {status}  {} {}  {}  {}", e.time, e.method, e.url, paint(&format!("{}:{}", e.file, e.line), DIM, false), paint(&e.id, DIM, false));
            }
        }
        Format::Json => println!("{}", serde_json::to_value(&entries).unwrap_or_default()),
        Format::Jsonl => entries.iter().for_each(|e| println!("{}", serde_json::to_value(e).unwrap_or_default())),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The agent skill documents every command, long flag and directive; extend it when this fails.
    #[test]
    fn skill_covers_cli() {
        use clap::CommandFactory as _;
        let skill = include_str!("../../../skills/respite/SKILL.md");
        fn walk(cmd: &clap::Command, path: &str, skill: &str, missing: &mut Vec<String>) {
            for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
                let path = format!("{path} {}", sub.get_name());
                if !skill.contains(&path) {
                    missing.push(path.clone());
                }
                let flags = sub.get_arguments().filter_map(|a| a.get_long()).filter(|l| *l != "help");
                missing.extend(flags.filter(|l| !skill.contains(&format!("--{l}"))).map(|l| format!("{path} --{l}")));
                walk(sub, &path, skill, missing);
            }
        }
        let mut missing = vec![];
        walk(&Cli::command(), "respite", skill, &mut missing);
        missing.extend(http_syntax::DIRECTIVES.iter().filter(|d| !skill.contains(&format!("@{d}"))).map(|d| format!("@{d}")));
        missing.extend(
            http_syntax::ASSERT_BINARY.iter().chain(http_syntax::ASSERT_UNARY).filter(|o| !skill.contains(&format!("`{o}`"))).map(|o| format!("@assert {o}")),
        );
        assert!(missing.is_empty(), "skills/respite/SKILL.md misses: {missing:?}");
    }

    #[test]
    fn request_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.http");
        std::fs::write(&path, "### First\n# @name one\nGET /1\n\n### Second\nGET /2\nAccept: */*\n").unwrap();
        let loaded = load(&path).unwrap();
        let at = |a: &str| find(&loaded, a).ok();
        assert_eq!([at("one"), at("Second"), at("#2"), at(":7"), at(":1")], [Some(0), Some(1), Some(1), Some(1), Some(0)]);
        assert_eq!([at("#0"), at("#3"), at(":99"), at("nope")], [None; 4]);
        assert!(find(&loaded, "nope").unwrap_err().hint.unwrap().contains("one"));
    }

    #[test]
    fn sizes() {
        assert_eq!(run::parse_size("64KB"), Ok(65536));
        assert_eq!(run::parse_size("2mb"), Ok(2 << 20));
        assert_eq!(run::parse_size("10"), Ok(10));
        assert!(run::parse_size("lots").is_err());
    }
}
