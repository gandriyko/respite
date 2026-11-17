//! What the sidebar shows: the central folder (`~/.config/respite/collections`, with subfolders) and the
//! `.http` / `.rest` files added from elsewhere.
//!
//! The added files are listed in `~/.respite/files.json` (a JSON array of paths). Removing one
//! only drops it from that list: the file and its history stay.

use std::path::{Path, PathBuf};

/// `~/.config/respite/collections`; environments are next to it, in `~/.config/respite/env`.
pub fn collections_dir() -> PathBuf {
    http_env::collections_dir().unwrap_or_default()
}

pub fn env_dir() -> PathBuf {
    http_env::env_dir().unwrap_or_default()
}

pub fn is_request_file(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "http" || e == "rest")
}

/// Added files that still exist.
pub fn load_files(base: Option<&Path>) -> Vec<PathBuf> {
    let Some(base) = base else { return vec![] };
    let list: Vec<PathBuf> = std::fs::read(base.join("files.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    list.into_iter().filter(|p| p.is_file()).collect()
}

/// Persist the added files; failures are reported, not fatal.
pub fn save_files(base: Option<&Path>, files: &[PathBuf]) {
    let Some(base) = base else { return };
    let write = std::fs::create_dir_all(base)
        .and_then(|()| std::fs::write(base.join("files.json"), serde_json::to_vec_pretty(files).unwrap_or_default()));
    if let Err(e) = write {
        eprintln!("respite: file list not saved: {e}");
    }
}

fn read_state(base: &Path) -> serde_json::Value {
    std::fs::read(base.join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}))
}

/// File last selected and its cursor (0-based line, column), if the file still exists.
pub fn last_opened(base: Option<&Path>) -> Option<(PathBuf, u32, u32)> {
    let v = &read_state(base?)["last_file"];
    let path = PathBuf::from(v["path"].as_str()?);
    let pos = |k: &str| v[k].as_u64().unwrap_or(0) as u32;
    path.is_file().then(|| (path, pos("line"), pos("column")))
}

pub fn save_last_file(base: Option<&Path>, path: &Path, line: u32, column: u32) {
    let Some(base) = base else { return };
    let mut state = read_state(base);
    state["last_file"] = serde_json::json!({ "path": path, "line": line, "column": column });
    if let Err(e) = std::fs::create_dir_all(base).and_then(|()| std::fs::write(base.join("state.json"), state.to_string())) {
        eprintln!("respite: state not saved: {e}");
    }
}

pub use http_env::is_env_file;

/// Folders and `.http` / `.rest` files under the central folder `root`, sorted. Folders are
/// listed so that an empty new one shows up.
pub fn scan(root: &Path) -> Vec<PathBuf> {
    // ponytail: depth cap guards against a runaway tree; raise if real collections nest deeper.
    let mut found: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .max_depth(Some(8))
        .build()
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p != root && (p.is_dir() || (p.is_file() && is_request_file(p))))
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_state_and_scan() {
        let base = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let (a, gone) = (other.path().join("a.http"), other.path().join("gone.http"));
        std::fs::write(&a, "GET /").unwrap();
        save_files(Some(base.path()), &[a.clone(), gone]);
        assert_eq!(load_files(Some(base.path())), std::slice::from_ref(&a)); // a deleted file drops out

        assert_eq!(last_opened(Some(base.path())), None);
        save_last_file(Some(base.path()), &a, 12, 4);
        assert_eq!(last_opened(Some(base.path())), Some((a.clone(), 12, 4)));
        std::fs::remove_file(&a).unwrap();
        assert_eq!(last_opened(Some(base.path())), None); // deleted file is not reopened

        let r = other.path();
        for d in ["api/v2", "empty", ".hidden"] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        for f in ["b.http", "api/v2/c.rest", "api/notes.txt", ".hidden/d.http"] {
            std::fs::write(r.join(f), "").unwrap();
        }
        let rel: Vec<_> = scan(r).iter().map(|p| p.strip_prefix(r).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(rel, ["api", "api/v2", "api/v2/c.rest", "b.http", "empty"]);
    }
}
