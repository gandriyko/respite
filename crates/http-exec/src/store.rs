//! Per-project history in `<base>/history/<project>/` (base is `~/.respite`):
//! `<id>.json` (request + response metadata) and `<id>.body` (raw response body),
//! plus `cookies.<env>.json`. The newest entry of a named request doubles as its
//! cross-run response cache.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Exchange, Response, SentRequest};

/// Request headers whose values never reach disk.
const SENSITIVE: [&str; 3] = ["authorization", "cookie", "x-api-key"];
const MASK: &str = "••••";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    /// RFC 3339, UTC.
    pub time: String,
    /// `gui` / `cli` / `mcp`.
    pub source: String,
    /// Relative to the workspace root.
    pub file: String,
    pub name: Option<String>,
    pub label: String,
    pub line: usize,
    pub method: String,
    pub url: String,
    pub request_headers: Vec<(String, String)>,
    pub request_body: String,
    pub status: u16,
    pub reason: String,
    pub http_version: String,
    pub response_headers: Vec<(String, String)>,
    pub ttfb_ms: u64,
    pub total_ms: u64,
    pub size_bytes: usize,
    pub dependencies: Vec<String>,
}

pub struct Store {
    root: PathBuf,
    dir: PathBuf,
    pub per_request: usize,
    pub total: usize,
}

impl Store {
    /// `~/.respite`
    pub fn default_base() -> Option<PathBuf> {
        Some(std::env::home_dir()?.join(".respite"))
    }

    /// `base` is `~/.respite`; `root` is the workspace root (its dir name names the project).
    pub fn open(base: &Path, root: &Path) -> io::Result<Self> {
        let name = project_name(base, root)?;
        let dir = base.join("history").join(name);
        std::fs::create_dir_all(&dir)?;
        Ok(Self { root: root.to_path_buf(), dir, per_request: 50, total: 500 })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn cookies_path(&self, env: Option<&str>) -> PathBuf {
        self.dir.join(format!("cookies.{}.json", env.unwrap_or("_")))
    }

    /// Save an execution; returns its id. Sensitive request headers and every `secrets` value
    /// in the request (URL, headers, body) are masked. The response body is kept as is:
    /// it is the `{{name.response…}}` cache.
    pub fn record(
        &self,
        source: &str,
        file: &Path,
        req: &http_syntax::Request,
        ex: &Exchange,
        secrets: &[&str],
    ) -> io::Result<String> {
        let mask = |s: &str| secrets.iter().fold(s.to_string(), |acc, sec| acc.replace(sec, MASK));
        let now = jiff::Timestamp::now();
        let label = req.name.clone().unwrap_or_else(|| format!("line{}", req.request_line));
        // Nanoseconds keep ids (= file names) in execution order when sorted.
        let mut id = format!("{}-{}", now.strftime("%Y-%m-%dT%H-%M-%S%.9fZ"), sanitize(&label));
        while self.dir.join(format!("{id}.json")).exists() {
            id.push('_');
        }
        let (rq, rs) = (&ex.request, &ex.response);
        let entry = Entry {
            id: id.clone(),
            time: now.to_string(),
            source: source.into(),
            file: self.relative(file),
            name: req.name.clone(),
            label: req.label(),
            line: req.request_line,
            method: rq.method.clone(),
            url: mask(&rq.url),
            request_headers: rq
                .headers
                .iter()
                .map(|(n, v)| {
                    let masked = SENSITIVE.iter().any(|s| n.eq_ignore_ascii_case(s));
                    (n.clone(), if masked { MASK.into() } else { mask(v) })
                })
                .collect(),
            request_body: mask(&String::from_utf8_lossy(&rq.body)),
            status: rs.status,
            reason: rs.reason.clone(),
            http_version: rs.http_version.clone(),
            response_headers: rs.headers.clone(),
            ttfb_ms: rs.ttfb.as_millis() as u64,
            total_ms: rs.total.as_millis() as u64,
            size_bytes: rs.body.len(),
            dependencies: ex.dependencies.clone(),
        };
        std::fs::write(self.dir.join(format!("{id}.body")), &rs.body)?;
        std::fs::write(self.dir.join(format!("{id}.json")), serde_json::to_vec_pretty(&entry).map_err(io::Error::other)?)?;
        self.prune(&entry)?;
        Ok(id)
    }

    /// Newest first.
    // ponytail: reads every entry's JSON (≤ `total` small files); add an index file if it shows up in profiles.
    pub fn list(&self) -> Vec<Entry> {
        let mut ids: Vec<String> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(str::to_string))
            .filter(|id| !id.starts_with("cookies."))
            .collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.iter().filter_map(|id| self.entry(id)).collect()
    }

    pub fn entry(&self, id: &str) -> Option<Entry> {
        serde_json::from_slice(&std::fs::read(self.dir.join(format!("{id}.json"))).ok()?).ok()
    }

    pub fn body(&self, id: &str) -> Option<Vec<u8>> {
        std::fs::read(self.dir.join(format!("{id}.body"))).ok()
    }

    /// Newest recorded exchange of request `name` in `file` (cross-run `{{name.response…}}` cache).
    pub fn latest(&self, file: &Path, name: &str) -> Option<Exchange> {
        let rel = self.relative(file);
        self.exchange(self.list().into_iter().find(|e| e.file == rel && e.name.as_deref() == Some(name))?)
    }

    /// Workspace root; `Entry::file` is relative to it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A recorded entry as an exchange (body read from disk). Request secrets stay masked.
    pub fn exchange(&self, entry: Entry) -> Option<Exchange> {
        let body = self.body(&entry.id)?;
        Some(Exchange {
            request: SentRequest {
                method: entry.method,
                url: entry.url,
                headers: entry.request_headers,
                body: entry.request_body.into_bytes(),
            },
            response: Response {
                status: entry.status,
                reason: entry.reason,
                http_version: entry.http_version,
                headers: entry.response_headers,
                body,
                ttfb: Duration::from_millis(entry.ttfb_ms),
                total: Duration::from_millis(entry.total_ms),
            },
            dependencies: entry.dependencies,
            history_id: Some(entry.id),
        })
    }

    fn prune(&self, newest: &Entry) -> io::Result<()> {
        let mut same = 0;
        for (i, e) in self.list().iter().enumerate() {
            let same_request = e.file == newest.file && e.label == newest.label;
            same += same_request as usize;
            if i >= self.total || (same_request && same > self.per_request) {
                for ext in ["json", "body"] {
                    std::fs::remove_file(self.dir.join(format!("{}.{ext}", e.id)))?;
                }
            }
        }
        Ok(())
    }

    fn relative(&self, file: &Path) -> String {
        file.strip_prefix(&self.root).unwrap_or(file).to_string_lossy().into_owned()
    }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// Root dir name, or `name-<4 hex>` if another root already took it; kept in `projects.json`.
fn project_name(base: &Path, root: &Path) -> io::Result<String> {
    std::fs::create_dir_all(base)?;
    let map_path = base.join("projects.json");
    let mut map: HashMap<String, String> =
        std::fs::read(&map_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let key = root.to_string_lossy().into_owned();
    if let Some(name) = map.get(&key) {
        return Ok(name.clone());
    }
    let plain = sanitize(&root.file_name().map_or("root".into(), |n| n.to_string_lossy().into_owned()));
    let name = if map.values().any(|v| *v == plain) {
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        format!("{plain}-{:04x}", h.finish() & 0xffff)
    } else {
        plain
    };
    map.insert(key, name.clone());
    std::fs::write(&map_path, serde_json::to_vec_pretty(&map).map_err(io::Error::other)?)?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(status: u16) -> Exchange {
        Exchange {
            request: SentRequest {
                method: "GET".into(),
                url: "http://x/".into(),
                headers: vec![("Authorization".into(), "Bearer T".into()), ("Accept".into(), "*/*".into())],
                body: vec![],
            },
            response: Response {
                status,
                reason: "OK".into(),
                http_version: "HTTP/1.1".into(),
                headers: vec![],
                body: b"{\"a\":1}".to_vec(),
                ttfb: Duration::from_millis(5),
                total: Duration::from_millis(9),
            },
            dependencies: vec![],
            history_id: None,
        }
    }

    #[test]
    fn project_names_record_latest_prune() {
        let base = tempfile::tempdir().unwrap();
        let (a, b) = (Path::new("/w1/api"), Path::new("/w2/api"));
        let sa = Store::open(base.path(), a).unwrap();
        let sb = Store::open(base.path(), b).unwrap();
        assert!(sa.dir().ends_with("history/api"));
        let other = sb.dir().file_name().unwrap().to_string_lossy().into_owned();
        assert!(other.starts_with("api-") && other.len() == 8, "{other}");
        assert_eq!(Store::open(base.path(), b).unwrap().dir(), sb.dir());

        let mut s = Store::open(base.path(), a).unwrap();
        (s.per_request, s.total) = (2, 3);
        let file = a.join("x/users.http");
        let parsed = http_syntax::parse("# @name login\nGET http://x/\n###\nGET http://y/\n");
        for status in [200, 201, 202] {
            s.record("cli", &file, &parsed.requests[0], &exchange(status), &[]).unwrap();
        }
        s.record("gui", &file, &parsed.requests[1], &exchange(204), &[]).unwrap();
        s.record("gui", &file, &parsed.requests[1], &exchange(205), &["http://x"]).unwrap();

        let list = s.list();
        assert_eq!(list.iter().map(|e| e.status).collect::<Vec<_>>(), [205, 204, 202]);
        assert_eq!(list[2].file, "x/users.http");
        assert_eq!((list[0].url.as_str(), list[1].url.as_str()), ("••••/", "http://x/"));
        assert_eq!(list[2].request_headers[0].1, MASK);

        let cached = s.latest(&file, "login").unwrap();
        assert_eq!((cached.response.status, cached.response.body.as_slice()), (202, b"{\"a\":1}".as_slice()));
        assert!(s.latest(&file, "nope").is_none());
    }
}
