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

fn read_state(base: &Path) -> serde_json::Value {
    std::fs::read(base.join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}))
}

fn write_state(base: &Path, state: &serde_json::Value) {
    if let Err(e) = std::fs::create_dir_all(base).and_then(|()| std::fs::write(base.join("state.json"), state.to_string())) {
        eprintln!("respite: state not saved: {e}");
    }
}

/// Project opened last (`state.json` → `last_root`), if it still exists.
pub fn last_root(base: Option<&Path>) -> Option<PathBuf> {
    let root = PathBuf::from(read_state(base?)["last_root"].as_str()?);
    root.is_dir().then_some(root)
}

pub fn save_last_root(base: Option<&Path>, root: &Path) {
    let Some(base) = base else { return };
    let mut state = read_state(base);
    state["last_root"] = root.to_string_lossy().into_owned().into();
    write_state(base, &state);
}

/// File last selected in `root` and its cursor (0-based line, column), if the file still exists.
pub fn last_file(base: Option<&Path>, root: &Path) -> Option<(PathBuf, u32, u32)> {
    let v = &read_state(base?)["files"][root.to_string_lossy().as_ref()];
    let path = PathBuf::from(v["path"].as_str()?);
    let pos = |k: &str| v[k].as_u64().unwrap_or(0) as u32;
    path.is_file().then(|| (path, pos("line"), pos("column")))
}

pub fn save_last_file(base: Option<&Path>, root: &Path, path: &Path, line: u32, column: u32) {
    let Some(base) = base else { return };
    let mut state = read_state(base);
    if !state["files"].is_object() {
        state["files"] = serde_json::json!({});
    }
    state["files"][root.to_string_lossy().as_ref()] = serde_json::json!({ "path": path, "line": line, "column": column });
    write_state(base, &state);
}

/// Environment last chosen for `root` (`~/.respite/state.json` → `{"env": {root: name}}`).
pub fn load_env(base: Option<&Path>, root: &Path) -> Option<String> {
    read_state(base?)["env"][root.to_string_lossy().as_ref()].as_str().map(str::to_string)
}

/// Remember the environment for `root`; `None` = "No environment".
pub fn save_env(base: Option<&Path>, root: &Path, env: Option<&str>) {
    let Some(base) = base else { return };
    let mut state = read_state(base);
    if !state["env"].is_object() {
        state["env"] = serde_json::json!({});
    }
    state["env"][root.to_string_lossy().as_ref()] = env.map_or(serde_json::Value::Null, Into::into);
    write_state(base, &state);
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
        assert_eq!(last_root(Some(base.path())), None);
        save_last_root(Some(base.path()), other.path());
        assert_eq!(last_root(Some(base.path())).as_deref(), Some(other.path()));
        assert_eq!(load_env(Some(base.path()), other.path()).as_deref(), Some("prod")); // same file, other keys kept
        let f = other.path().join("x.http");
        std::fs::write(&f, "GET /").unwrap();
        save_last_file(Some(base.path()), other.path(), &f, 12, 4);
        assert_eq!(last_file(Some(base.path()), other.path()), Some((f.clone(), 12, 4)));
        std::fs::remove_file(&f).unwrap();
        assert_eq!(last_file(Some(base.path()), other.path()), None); // deleted file is not reopened

        let r = cur.path();
        std::fs::create_dir_all(r.join("api/v2")).unwrap();
        std::fs::create_dir_all(r.join("ignored")).unwrap();
        for f in [
            "a.http", "api/v2/b.rest", "api/notes.txt", "ignored/c.http", "package.json",
            "http-client.env.json", "http-client.private.env.json", "respite-dev.json", "respite-dev.private.json",
        ] {
            std::fs::write(r.join(f), "").unwrap();
        }
        std::fs::write(r.join(".gitignore"), "ignored/\n*.private.*\n").unwrap();
        std::fs::create_dir(r.join(".git")).unwrap(); // .gitignore applies inside a git repo
        let rel: Vec<_> = scan(r).iter().map(|p| p.strip_prefix(r).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(rel, [
            "a.http", "api/v2/b.rest", "http-client.env.json", "http-client.private.env.json",
            "respite-dev.json", "respite-dev.private.json",
        ]);
    }
}
