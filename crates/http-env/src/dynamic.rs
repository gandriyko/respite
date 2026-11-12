//! `{{$…}}` dynamic variables (VS Code REST Client set).

use std::path::Path;

use jiff::{Span, Timestamp, Zoned, tz::TimeZone};

use crate::ResolveError;

/// `expr` is everything after `$`, e.g. `timestamp -1 d`.
/// `var` resolves `%NAME` indirections through regular variables.
pub(crate) fn eval(
    expr: &str,
    dir: &Path,
    var: &mut dyn FnMut(&str) -> Result<String, ResolveError>,
) -> Result<String, ResolveError> {
    let (name, args) = expr.split_once(char::is_whitespace).map_or((expr, ""), |(n, a)| (n, a.trim()));
    let bad = || ResolveError::Dynamic(format!("${expr}"));
    let utc = || Zoned::now().with_time_zone(TimeZone::UTC);
    Ok(match name {
        "uuid" | "guid" => uuid::Uuid::new_v4().to_string(),
        "randomInt" => {
            let n: Vec<i64> = args.split_whitespace().map(str::parse).collect::<Result<_, _>>().map_err(|_| bad())?;
            let (lo, hi) = match n[..] {
                [] => (0, 1000),
                [lo, hi] if lo < hi => (lo, hi),
                _ => return Err(bad()),
            };
            fastrand::i64(lo..hi).to_string()
        }
        "timestamp" => shifted(utc(), args).ok_or_else(bad)?.timestamp().as_second().to_string(),
        "isoTimestamp" => Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        "datetime" | "localDatetime" => {
            let is_utc = name == "datetime";
            let (fmt, offset) = split_format(args).ok_or_else(bad)?;
            let t = shifted(if is_utc { utc() } else { Zoned::now() }, offset).ok_or_else(bad)?;
            format(&t, fmt, is_utc).ok_or_else(bad)?
        }
        "processEnv" => {
            let key = indirect(args, var)?;
            std::env::var(&key).map_err(|_| ResolveError::Undefined(format!("$processEnv {key}")))?
        }
        "dotenv" => {
            let key = indirect(args, var)?;
            dotenvy::from_path_iter(dir.join(".env"))
                .ok()
                .and_then(|it| it.flatten().find(|(k, _)| *k == key).map(|(_, v)| v))
                .ok_or_else(|| ResolveError::Undefined(format!("$dotenv {key}")))?
        }
        _ => return Err(bad()),
    })
}

fn indirect(arg: &str, var: &mut dyn FnMut(&str) -> Result<String, ResolveError>) -> Result<String, ResolveError> {
    match arg.strip_prefix('%') {
        Some(name) => var(name),
        None => Ok(arg.to_string()),
    }
}

/// `rfc1123 -1 d` / `"YYYY-MM-DD" 2 h` → (format incl. quotes, offset).
fn split_format(args: &str) -> Option<(&str, &str)> {
    match args.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let end = args[1..].find(q)? + 1;
            Some((&args[..=end], args[end + 1..].trim()))
        }
        _ => Some(args.split_once(char::is_whitespace).map_or((args, ""), |(f, o)| (f, o.trim()))),
    }
}

/// Apply `"<n> <unit>"` (units as in VS Code: y M w d h m s ms); empty offset = unchanged.
fn shifted(t: Zoned, offset: &str) -> Option<Zoned> {
    let mut it = offset.split_whitespace();
    let (Some(n), Some(unit), None) = (it.next(), it.next(), it.next()) else {
        return offset.is_empty().then_some(t);
    };
    let n: i64 = n.parse().ok()?;
    let span = match unit {
        "y" => Span::new().try_years(n),
        "M" => Span::new().try_months(n),
        "w" => Span::new().try_weeks(n),
        "d" => Span::new().try_days(n),
        "h" => Span::new().try_hours(n),
        "m" => Span::new().try_minutes(n),
        "s" => Span::new().try_seconds(n),
        "ms" => Span::new().try_milliseconds(n),
        _ => return None,
    }
    .ok()?;
    t.checked_add(span).ok()
}

fn format(t: &Zoned, fmt: &str, utc: bool) -> Option<String> {
    let strf = match fmt {
        "rfc1123" if utc => "%a, %d %b %Y %H:%M:%S GMT".to_string(),
        "rfc1123" => "%a, %d %b %Y %H:%M:%S %z".to_string(),
        "iso8601" if utc => "%Y-%m-%dT%H:%M:%S%.3fZ".to_string(),
        "iso8601" => "%Y-%m-%dT%H:%M:%S%.3f%:z".to_string(),
        quoted if quoted.len() >= 2 && quoted.starts_with(['"', '\'']) => {
            dayjs_to_strftime(&quoted[1..quoted.len() - 1])
        }
        _ => return None,
    };
    Some(t.strftime(&strf).to_string())
}

/// Custom formats use day.js tokens (as VS Code does).
// ponytail: common tokens only; `[escaped]` text and locale tokens are not supported.
fn dayjs_to_strftime(fmt: &str) -> String {
    const TOKENS: &[(&str, &str)] = &[
        ("YYYY", "%Y"), ("YY", "%y"), ("MMMM", "%B"), ("MMM", "%b"), ("MM", "%m"), ("M", "%-m"),
        ("DD", "%d"), ("D", "%-d"), ("dddd", "%A"), ("ddd", "%a"), ("HH", "%H"), ("H", "%-H"),
        ("hh", "%I"), ("h", "%-I"), ("mm", "%M"), ("m", "%-M"), ("ss", "%S"), ("s", "%-S"),
        ("SSS", "%3f"), ("A", "%p"), ("a", "%P"), ("ZZ", "%z"), ("Z", "%:z"),
    ];
    let mut out = String::new();
    let mut rest = fmt;
    'outer: while let Some(c) = rest.chars().next() {
        for (tok, rep) in TOKENS {
            if let Some(r) = rest.strip_prefix(tok) {
                out.push_str(rep);
                rest = r;
                continue 'outer;
            }
        }
        if c == '%' {
            out.push('%');
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(expr: &str) -> Result<String, ResolveError> {
        eval(expr, Path::new("."), &mut |n| if n == "KEYNAME" { Ok("RESPITE_TEST_VAR".into()) } else { Err(ResolveError::Undefined(n.into())) })
    }

    #[test]
    fn dynamic_variables() {
        assert_eq!(ev("uuid").unwrap().len(), 36);
        let n: i64 = ev("randomInt 5 7").unwrap().parse().unwrap();
        assert!((5..7).contains(&n));
        assert!(ev("randomInt 7 5").is_err());
        let now: i64 = ev("timestamp").unwrap().parse().unwrap();
        let day_ago: i64 = ev("timestamp -1 d").unwrap().parse().unwrap();
        assert!((now - day_ago - 86_400).abs() <= 1);
        assert!(ev("isoTimestamp").unwrap().ends_with('Z'));
        assert!(ev("datetime rfc1123").unwrap().ends_with(" GMT"));
        assert_eq!(ev("datetime iso8601").unwrap().len(), 24);
        let d = ev("datetime \"YYYY-MM-DD HH:mm\" 1 y").unwrap();
        assert_eq!(d.len(), 16, "{d}");
        assert!(ev("datetime 'DD.MM' -2 w").is_ok());
        assert!(ev("datetime bogus").is_err());
        unsafe { std::env::set_var("RESPITE_TEST_VAR", "42") };
        assert_eq!(ev("processEnv RESPITE_TEST_VAR").unwrap(), "42");
        assert_eq!(ev("processEnv %KEYNAME").unwrap(), "42");
        assert!(matches!(ev("nope"), Err(ResolveError::Dynamic(_))));
    }

    #[test]
    fn dotenv_lookup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "API_KEY=abc\n").unwrap();
        let r = eval("dotenv API_KEY", dir.path(), &mut |n| Err(ResolveError::Undefined(n.into())));
        assert_eq!(r.unwrap(), "abc");
    }
}
