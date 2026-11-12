//! `respite import-curl` / `respite export-curl`.

use std::io::Read;
use std::path::{Path, PathBuf};

use clap::Args;
use http_env::Environments;

use crate::{CliError, DIM, EnvArgs, environment, find, load, paint};

#[derive(Args)]
pub(crate) struct ImportArgs {
    /// Read the command from the clipboard instead of stdin
    #[arg(long)]
    clipboard: bool,
    /// Append the block(s) to this .http file instead of printing
    #[arg(long, value_name = "FILE")]
    to: Option<PathBuf>,
    /// Replace values of this environment's variables with `{{name}}`
    #[arg(long)]
    env: Option<String>,
    /// Do not keep the original command as a comment
    #[arg(long)]
    no_original: bool,
}

#[derive(Args)]
pub(crate) struct ExportArgs {
    file: PathBuf,
    /// Name, `:LINE` or `#N`
    request: String,
    /// Keep `{{variables}}` instead of substituting them
    #[arg(long)]
    raw: bool,
    #[command(flatten)]
    env: EnvArgs,
}

pub(crate) fn import(args: ImportArgs) -> Result<u8, CliError> {
    let text = if args.clipboard {
        arboard::Clipboard::new()
            .and_then(|mut c| c.get_text())
            .map_err(|e| CliError::new("clipboard_unavailable", e.to_string(), 3))?
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| CliError::new("bad_input", e.to_string(), 3))?;
        s
    };
    let vars = substitution_vars(args.to.as_deref(), args.env.as_deref());
    let cwd = std::path::absolute(".").unwrap_or_default();
    let s = crate::settings(&http_env::workspace_root(&cwd.join("_.http")));
    let strip: Vec<&str> = s.curl.strip_headers.iter().map(String::as_str).collect();
    let opts = http_curl::Options {
        keep_original: s.curl.keep_original_as_comment && !args.no_original,
        strip_headers: &strip,
        vars: &vars,
        ..Default::default()
    };
    let blocks = http_curl::import_all(&text, &opts).map_err(|e| CliError::new("invalid_curl", e, 3))?;
    let joined = blocks.join("\n");
    let Some(to) = args.to else {
        print!("{joined}");
        return Ok(0);
    };
    let existing = std::fs::read_to_string(&to).unwrap_or_default();
    let sep = match existing.as_str() {
        "" => "",
        s if s.ends_with("\n\n") => "",
        s if s.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    std::fs::write(&to, format!("{existing}{sep}{joined}"))
        .map_err(|e| CliError::new("write_failed", e.to_string(), 3).at(&to, None))?;
    eprintln!("{}", paint(&format!("appended {} block(s) to {}", blocks.len(), to.display()), DIM, true));
    Ok(0)
}

/// Values that become `{{name}}`: env variables (`env` or just `$shared`) and the target
/// file's own `@variables`, resolved.
fn substitution_vars(target: Option<&Path>, env: Option<&str>) -> Vec<(String, String)> {
    let probe = target.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("_.http"));
    let path = std::path::absolute(&probe).unwrap_or(probe);
    let root = http_env::workspace_root(&path);
    let env_vars = Environments::load(&path, &root, &[]).variables(env);
    let file = http_syntax::parse(&std::fs::read_to_string(&path).unwrap_or_default());
    http_env::known_values(&file, &env_vars, path.parent().unwrap_or(Path::new(".")))
}

pub(crate) fn export(args: ExportArgs) -> Result<u8, CliError> {
    let loaded = load(&args.file)?;
    let index = find(&loaded, &args.request)?;
    let req = &loaded.file.requests[index];
    if args.raw {
        println!("{}", http_curl::export_raw(req));
        return Ok(0);
    }
    let (env, overrides) = environment(&loaded, &args.env)?;
    let exec = crate::executor(&loaded.root, args.env.env.as_deref());
    let cx = http_exec::Context { path: &loaded.path, file: &loaded.file, env: &env, overrides: &overrides };
    let sent = exec.prepare(&cx, index).map_err(|e| crate::run::exec_error(e, &loaded, req))?;
    println!("{}", http_curl::export(&sent, req));
    Ok(0)
}
