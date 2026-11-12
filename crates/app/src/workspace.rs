//! Projects for the sidebar and their `.http` / `.rest` files.
//!
//! The list lives in `~/.respite/workspaces.json` (a JSON array of roots). Removing a
//! project only drops it from that list: files and history stay. Until the file exists it
//! is seeded from the roots the history store has seen (`~/.respite/projects.json`).

use std::path::{Path, PathBuf};

pub struct Project {
    pub root: PathBuf,
    pub name: String,
    /// `None` until first expanded (other projects may be big; scan lazily).
    pub files: Option<Vec<PathBuf>>,
}

impl Project {
    pub fn new(root: PathBuf) -> Self {
        Self { name: name_of(&root), files: None, root }
    }
}

/// `current` first (added to the saved list if new), then the other listed roots that still exist, by name.
pub fn projects(base: Option<&Path>, current: &Path) -> Vec<Project> {
    let mut roots = load_list(base);
    if !roots.iter().any(|r| r == current) {
        roots.push(current.to_path_buf());
        save_list(base, &roots);
    }
    let mut others: Vec<PathBuf> = roots.into_iter().filter(|r| r != current && r.is_dir()).collect();
    others.sort_by_key(|r| name_of(r).to_lowercase());
    std::iter::once(current.to_path_buf()).chain(others).map(Project::new).collect()
}

fn load_list(base: Option<&Path>) -> Vec<PathBuf> {
    let Some(base) = base else { return vec![] };
    if let Some(list) = std::fs::read(base.join("workspaces.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        return list;
    }
    std::fs::read(base.join("projects.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes).ok())
        .map(|m| m.keys().map(PathBuf::from).collect())
        .unwrap_or_default()
}

/// Persist the sidebar's project roots; failures are reported, not fatal.
pub fn save_list(base: Option<&Path>, roots: &[PathBuf]) {
    let Some(base) = base else { return };
    let write = std::fs::create_dir_all(base)
        .and_then(|()| std::fs::write(base.join("workspaces.json"), serde_json::to_vec_pretty(roots).unwrap_or_default()));
    if let Err(e) = write {
        eprintln!("respite: project list not saved: {e}");
    }
}

fn name_of(root: &Path) -> String {
    root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Environment last chosen for `root` (`~/.respite/state.json` → `{"env": {root: name}}`).
pub fn load_env(base: Option<&Path>, root: &Path) -> Option<String> {
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(base?.join("state.json")).ok()?).ok()?;
    state["env"][root.to_string_lossy().as_ref()].as_str().map(str::to_string)
}

/// Remember the environment for `root`; `None` = "No environment".
pub fn save_env(base: Option<&Path>, root: &Path, env: Option<&str>) {
    let Some(base) = base else { return };
    let path = base.join("state.json");
    let mut state: serde_json::Value =
        std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}));
    if !state["env"].is_object() {
        state["env"] = serde_json::json!({});
    }
    state["env"][root.to_string_lossy().as_ref()] = env.map_or(serde_json::Value::Null, Into::into);
    if let Err(e) = std::fs::create_dir_all(base).and_then(|()| std::fs::write(&path, state.to_string())) {
        eprintln!("respite: environment choice not saved: {e}");
    }
}

pub use http_env::is_env_file;

/// `.http` / `.rest` and env files (`*.env.json`, `respite*.json`) under `root`, respecting
/// `.gitignore`. Private env files are usually git-ignored, so they are added back.
pub fn scan(root: &Path) -> Vec<PathBuf> {
    // ponytail: depth cap guards against a root like $HOME; raise if real projects nest deeper.
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .max_depth(Some(8))
        .build()
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p.is_file() && (p.extension().is_some_and(|e| e == "http" || e == "rest") || is_env_file(p)))
        .collect();
    // Git-ignored private env files next to the ones found (or at the root).
    let dirs: std::collections::BTreeSet<PathBuf> =
        files.iter().filter_map(|f| f.parent().map(Path::to_path_buf)).chain([root.to_path_buf()]).collect();
    for d in dirs {
        for p in std::fs::read_dir(&d).into_iter().flatten().flatten().map(|e| e.path()) {
            let private = p.file_name().is_some_and(|n| n.to_string_lossy().contains(".private."));
            if private && is_env_file(&p) && p.is_file() && !files.contains(&p) {
                files.push(p);
            }
        }
    }
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

        // Seeded list is saved; a removed root stays removed, a new current one is appended.
        let mut list = load_list(Some(base.path()));
        list.retain(|r| r != other.path());
        save_list(Some(base.path()), &list);
        let fresh = tempfile::tempdir().unwrap();
        let ps = projects(Some(base.path()), fresh.path());
        assert_eq!(ps.iter().map(|p| p.root.as_path()).collect::<Vec<_>>(), [fresh.path(), cur.path()]);

        assert_eq!(load_env(Some(base.path()), cur.path()), None);
        save_env(Some(base.path()), cur.path(), Some("dev"));
        save_env(Some(base.path()), other.path(), Some("prod"));
        assert_eq!(load_env(Some(base.path()), cur.path()).as_deref(), Some("dev"));
        save_env(Some(base.path()), cur.path(), None);
        assert_eq!(load_env(Some(base.path()), cur.path()), None);
        assert_eq!(load_env(Some(base.path()), other.path()).as_deref(), Some("prod"));

        let r = cur.path();
        std::fs::create_dir_all(r.join("api/v2")).unwrap();
        std::fs::create_dir_all(r.join("ignored")).unwrap();
        for f in [
            "a.http", "api/v2/b.rest", "api/notes.txt", "ignored/c.http", "package.json",
            "http-client.env.json", "http-client.private.env.json", "respite.dev.json", "respite.dev.private.json",
        ] {
            std::fs::write(r.join(f), "").unwrap();
        }
        std::fs::write(r.join(".gitignore"), "ignored/\n*.private.*\n").unwrap();
        std::fs::create_dir(r.join(".git")).unwrap(); // .gitignore applies inside a git repo
        let rel: Vec<_> = scan(r).iter().map(|p| p.strip_prefix(r).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(rel, [
            "a.http", "api/v2/b.rest", "http-client.env.json", "http-client.private.env.json",
            "respite.dev.json", "respite.dev.private.json",
        ]);
    }
}
