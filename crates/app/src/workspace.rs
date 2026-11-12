//! Projects for the sidebar: workspace roots Respite has seen (`~/.respite/projects.json`,
//! written by the history store) plus the current one, and their `.http` / `.rest` files.

use std::path::{Path, PathBuf};

pub struct Project {
    pub root: PathBuf,
    pub name: String,
    /// `None` until first expanded (other projects may be big; scan lazily).
    pub files: Option<Vec<PathBuf>>,
}

/// `current` first, then the other known roots that still exist, by name.
pub fn projects(base: Option<&Path>, current: &Path) -> Vec<Project> {
    let known: Vec<PathBuf> = base
        .and_then(|b| std::fs::read(b.join("projects.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes).ok())
        .map(|m| m.keys().map(PathBuf::from).collect())
        .unwrap_or_default();
    let mut others: Vec<PathBuf> = known.into_iter().filter(|r| r != current && r.is_dir()).collect();
    others.sort_by_key(|r| name_of(r).to_lowercase());
    std::iter::once(current.to_path_buf())
        .chain(others)
        .map(|root| Project { name: name_of(&root), files: None, root })
        .collect()
}

fn name_of(root: &Path) -> String {
    root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// `.http` / `.rest` files under `root`, respecting `.gitignore`.
pub fn scan(root: &Path) -> Vec<PathBuf> {
    // ponytail: depth cap guards against a root like $HOME; raise if real projects nest deeper.
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .max_depth(Some(8))
        .build()
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "http" || e == "rest"))
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_and_scan() {
        let base = tempfile::tempdir().unwrap();
        let (cur, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let map = serde_json::json!({
            other.path().to_string_lossy(): "o",
            cur.path().to_string_lossy(): "c",
            "/definitely/not/here": "gone",
        });
        std::fs::write(base.path().join("projects.json"), map.to_string()).unwrap();
        let ps = projects(Some(base.path()), cur.path());
        assert_eq!(ps.iter().map(|p| p.root.as_path()).collect::<Vec<_>>(), [cur.path(), other.path()]);

        let r = cur.path();
        std::fs::create_dir_all(r.join("api/v2")).unwrap();
        std::fs::create_dir_all(r.join("ignored")).unwrap();
        for f in ["a.http", "api/v2/b.rest", "api/notes.txt", "ignored/c.http"] {
            std::fs::write(r.join(f), "").unwrap();
        }
        std::fs::write(r.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(r.join(".git")).unwrap(); // .gitignore applies inside a git repo
        let rel: Vec<_> = scan(r).iter().map(|p| p.strip_prefix(r).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(rel, ["a.http", "api/v2/b.rest"]);
    }
}
