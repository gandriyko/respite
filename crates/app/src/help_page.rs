//! Help (F1): the `.http` syntax reference, opened as a read-only tab. The text lives in the
//! binary; the tab's path is virtual (nothing is written to disk), so the text always matches
//! the app version. Its examples still run like any other request.

use std::path::{Path, PathBuf};

use gpui::*;

use crate::Respite;

const HELP: &str = include_str!("../help.http");

/// A path no file has: the tab's identity and its breadcrumbs (`Respite Help › Syntax Reference.http`).
fn path() -> PathBuf {
    PathBuf::from("/Respite Help/Syntax Reference.http")
}

/// Opened read-only, never saved, not reopened on launch.
pub(crate) fn is_help(p: &Path) -> bool {
    p == path()
}

impl Respite {
    pub(crate) fn show_help(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open(path(), Some(HELP.into()), window, cx);
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
        assert!(!super::path().exists());
    }
}
