//! Shell tokenizer for pasted curl commands.

/// Remove line continuations: `\⏎` (bash), `^⏎` (cmd), `` `⏎ `` (PowerShell); CRLF → LF.
/// For cmd-style input also drop `^` escapes (`^"` → `"`, `^^` → `^`).
pub fn join_continuations(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let cmd = text.contains("^\n") || text.contains("^\"");
    let mut out = String::with_capacity(text.len());
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match (c, it.peek()) {
            ('\\' | '`', Some('\n')) if c == '\\' || !cmd => {
                it.next();
                out.push(' ');
            }
            ('^', Some(_)) if cmd => {
                let next = it.next().unwrap();
                out.push(if next == '\n' { ' ' } else { next });
            }
            _ => out.push(c),
        }
    }
    out
}

/// POSIX-ish words: '…' literal, "…" with \" \\ \$ \` escapes, $'…' ANSI-C, bare \x.
pub fn tokenize(command: &str) -> Result<Vec<String>, String> {
    let text = join_continuations(command);
    let mut out = vec![];
    let mut cur = String::new();
    let mut started = false;
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match it.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err("unclosed ' quote".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match it.next() {
                        Some('"') => break,
                        Some('\\') => match it.next() {
                            Some(e @ ('"' | '\\' | '$' | '`')) => cur.push(e),
                            Some('\n') => {}
                            Some(o) => {
                                cur.push('\\');
                                cur.push(o);
                            }
                            None => return Err("unclosed \" quote".into()),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err("unclosed \" quote".into()),
                    }
                }
            }
            '$' if it.peek() == Some(&'\'') => {
                it.next();
                started = true;
                ansi_c(&mut it, &mut cur)?;
            }
            '\\' => {
                if let Some(ch) = it.next() {
                    started = true;
                    cur.push(ch);
                }
            }
            _ => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

/// Body of `$'…'` (Chrome uses it for bodies with escapes).
fn ansi_c(it: &mut std::iter::Peekable<std::str::Chars>, cur: &mut String) -> Result<(), String> {
    let hex = |it: &mut std::iter::Peekable<std::str::Chars>, max: usize| {
        let mut s = String::new();
        while s.len() < max && it.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
            s.push(it.next().unwrap());
        }
        u32::from_str_radix(&s, 16).ok().and_then(char::from_u32)
    };
    loop {
        match it.next() {
            Some('\'') => return Ok(()),
            Some('\\') => match it.next() {
                Some('n') => cur.push('\n'),
                Some('t') => cur.push('\t'),
                Some('r') => cur.push('\r'),
                Some('0') => cur.push('\0'),
                Some('x') => cur.extend(hex(it, 2)),
                Some('u') => cur.extend(hex(it, 4)),
                Some('U') => cur.extend(hex(it, 8)),
                Some(o) => cur.push(o), // \\ \' \" and the rest
                None => return Err("unclosed $' quote".into()),
            },
            Some(ch) => cur.push(ch),
            None => return Err("unclosed $' quote".into()),
        }
    }
}
