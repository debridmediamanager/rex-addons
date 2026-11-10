//! Addon repositories: public git repositories whose folders hold addons. Every folder with a
//! `manifest.json` and a `main.js`, up to MAX_DEPTH levels down and outside hidden folders, is
//! one; an optional `repository.json` at the top names the repository.
use std::path::Path;

use serde_json::{json, Value as Json};

use crate::addons::{read_json, Manifest};

/// How deep under the repository's top an addon's folder may be: `addons/<id>/` is 2.
const MAX_DEPTH: usize = 3;

#[derive(Clone, Debug)]
pub struct RepoAddon {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub hosts: Vec<String>,
    /// Its folder, relative to the repository's top.
    pub dir: String,
}

#[derive(Clone, Debug)]
pub struct Repo {
    pub id: String,
    pub url: String,
    pub name: String,
    pub description: String,
    pub commit: Option<String>,
    pub last_fetch: Option<u64>,
    pub error: Option<String>,
    pub addons: Vec<RepoAddon>,
}

/// A repository's id: its host and path as a slug (`github-com-someone-addons`).
pub fn repo_id(url: &str) -> Result<String, String> {
    let (parsed, _) = crate::git::split(url)?;
    let path = parsed.path().trim_end_matches(".git");
    let text = format!("{}{path}", parsed.host_str().unwrap_or(""));
    let slug: String = text.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let slug = slug.split('-').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-");
    Ok(slug.chars().take(100).collect())
}

/// The addons in a checkout, and why each folder that looked like one is not.
pub fn discover(root: &Path) -> (Vec<RepoAddon>, Vec<String>) {
    let (mut found, mut problems) = (Vec::new(), Vec::new());
    walk(root, root, 0, &mut found, &mut problems);
    found.sort_by(|a: &RepoAddon, b| a.dir.cmp(&b.dir));
    let mut seen = std::collections::BTreeSet::new();
    found.retain(|addon| {
        let first = seen.insert(addon.id.clone());
        if !first {
            problems.push(format!("{}: the id {} is taken by another folder", addon.dir, addon.id));
        }
        first
    });
    (found, problems)
}

fn walk(root: &Path, dir: &Path, depth: usize, found: &mut Vec<RepoAddon>, problems: &mut Vec<String>) {
    let relative = dir.strip_prefix(root).unwrap_or(dir).to_string_lossy().into_owned();
    if depth > 0 && dir.join("manifest.json").is_file() && dir.join("main.js").is_file() {
        match read_json(&dir.join("manifest.json")).ok_or("manifest.json is not JSON".to_string()).and_then(|raw| Manifest::parse(&raw)) {
            Ok(m) => found.push(RepoAddon {
                id: m.id,
                name: m.name,
                version: m.version,
                description: m.description,
                hosts: m.hosts.0,
                dir: relative,
            }),
            Err(error) => problems.push(format!("{relative}: {error}")),
        }
        return;
    }
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        walk(root, &entry.path(), depth + 1, found, problems);
    }
}

/// The repository's name and description: its `repository.json`, else its address's last part.
pub fn describe(root: &Path, url: &str) -> (String, String) {
    let declared = read_json(&root.join("repository.json")).unwrap_or(Json::Null);
    let text = |key: &str| declared[key].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    (text("name").unwrap_or_else(|| name_from_url(url)), text("description").unwrap_or_default())
}

pub fn name_from_url(url: &str) -> String {
    url.split('#').next().unwrap_or(url).trim_end_matches('/').rsplit('/').next().unwrap_or(url).trim_end_matches(".git").to_string()
}

impl RepoAddon {
    pub fn to_json(&self) -> Json {
        json!({"id": self.id, "name": self.name, "version": self.version, "description": self.description, "hosts": self.hosts, "dir": self.dir})
    }

    fn from_json(value: &Json) -> Option<RepoAddon> {
        let text = |key: &str| value[key].as_str().map(str::to_string);
        Some(RepoAddon {
            id: text("id")?,
            name: text("name")?,
            version: text("version")?,
            description: text("description").unwrap_or_default(),
            hosts: serde_json::from_value(value["hosts"].clone()).unwrap_or_default(),
            dir: text("dir")?,
        })
    }
}

impl Repo {
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id, "url": self.url, "name": self.name, "description": self.description,
            "commit": self.commit, "last_fetch": self.last_fetch, "error": self.error,
            "addons": self.addons.iter().map(RepoAddon::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(value: &Json) -> Option<Repo> {
        let text = |key: &str| value[key].as_str().map(str::to_string);
        Some(Repo {
            id: text("id")?,
            url: text("url")?,
            name: text("name").unwrap_or_default(),
            description: text("description").unwrap_or_default(),
            commit: text("commit"),
            last_fetch: value["last_fetch"].as_u64(),
            error: text("error"),
            addons: value["addons"].as_array().into_iter().flatten().filter_map(RepoAddon::from_json).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addon(root: &Path, dir: &str, id: &str) {
        let path = root.join(dir);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("manifest.json"), json!({"id": id, "name": id, "version": "1.0.0"}).to_string()).unwrap();
        std::fs::write(path.join("main.js"), "export default { items() { return []; }, open() {} };").unwrap();
    }

    #[test]
    fn layout() {
        let root = std::env::temp_dir().join(format!("rex-addons-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        addon(&root, "addons/films", "films");
        addon(&root, "music", "music");
        addon(&root, "a/b/deep", "deep"); // depth 3: found
        addon(&root, "a/b/c/deeper", "deeper"); // depth 4: too deep
        addon(&root, ".hidden/secret", "secret");
        addon(&root, "addons/films/nested", "nested"); // inside an addon: not looked at
        addon(&root, "copy", "films"); // the same id again
        std::fs::create_dir_all(root.join("broken")).unwrap();
        std::fs::write(root.join("broken/manifest.json"), "{\"id\": \"Bad Id\"}").unwrap();
        std::fs::write(root.join("broken/main.js"), "").unwrap();
        std::fs::create_dir_all(root.join("half")).unwrap();
        std::fs::write(root.join("half/manifest.json"), json!({"id": "half", "name": "h", "version": "1"}).to_string()).unwrap();
        // The top itself is the repository, never an addon.
        std::fs::write(root.join("manifest.json"), "{}").unwrap();
        std::fs::write(root.join("main.js"), "").unwrap();

        let (found, problems) = discover(&root);
        let dirs: Vec<(&str, &str)> = found.iter().map(|a| (a.dir.as_str(), a.id.as_str())).collect();
        assert_eq!(dirs, [("a/b/deep", "deep"), ("addons/films", "films"), ("music", "music")]);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.starts_with("broken: manifest: id")));
        assert!(problems.iter().any(|p| p.contains("the id films is taken")));

        assert_eq!(describe(&root, "https://codeberg.org/me/my-addons.git#dev"), ("my-addons".into(), String::new()));
        std::fs::write(root.join("repository.json"), json!({"name": "My addons", "description": "Mine."}).to_string()).unwrap();
        assert_eq!(describe(&root, "https://codeberg.org/me/my-addons"), ("My addons".into(), "Mine.".into()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ids() {
        assert_eq!(repo_id("https://github.com/debridmediamanager/rex-addons").unwrap(), "github-com-debridmediamanager-rex-addons");
        assert_eq!(repo_id("https://GitLab.com/Group/Sub/Addons.git#main").unwrap(), "gitlab-com-group-sub-addons");
        assert!(repo_id("http://github.com/a/b").is_err());
    }
}
