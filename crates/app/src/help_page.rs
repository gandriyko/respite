//! Help (F1): the `.http` syntax reference, opened as a read-only tab. A real file under
//! `~/.respite/help`, so tabs, Cmd+W, reopening on launch and running examples all just work.

use std::path::{Path, PathBuf};

use gpui::*;
use http_exec::Store;

use crate::Respite;

const HELP: &str = include_str!("../help.http");

fn path() -> Option<PathBuf> {
    Some(Store::default_base()?.join("help").join("Syntax Reference.http"))
}

/// Opened read-only (also when reopened as the last file on launch).
pub(crate) fn is_help(p: &Path) -> bool {
    path().is_some_and(|h| h == p)
}

impl Respite {
    pub(crate) fn show_help(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = path() else { return };
        // Rewritten every time: the text follows the app version. If writing fails the tab
        // still opens from memory; only reopening on the next launch is lost.
        let _ = std::fs::create_dir_all(path.parent().expect("has a parent")).and_then(|_| std::fs::write(&path, HELP));
        self.open(path, Some(HELP.into()), window, cx);
    }
}

#[cfg(test)]
mod tests {
    /// The reference must stay valid syntax: no parser complaints, directives land where shown.
    #[test]
    fn help_parses_cleanly() {
        let file = http_syntax::parse(super::HELP);
        assert_eq!(file.diagnostics, vec![]);
        let by_title = |t: &str| file.requests.iter().find(|r| r.title.as_deref() == Some(t)).unwrap_or_else(|| panic!("no request {t:?}"));
        assert_eq!(by_title("Log in").name.as_deref(), Some("login"));
        assert!(by_title("Self-signed certificate").directive("insecure").is_some());
        assert!(by_title("Long URLs").url.ends_with("?page=1&limit=20"));
        assert_eq!(by_title("GraphQL").method, "GRAPHQL");
        assert!(by_title("Directives").directive("no-redirect").is_some());
        assert!(by_title("Use login's response").headers.iter().any(|h| h.value.contains("login.response")));
    }
}
