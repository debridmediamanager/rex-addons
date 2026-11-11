//! What an addon is on disk: its manifest, its settings, and what the host keeps for it.
use std::path::Path;

use serde_json::{json, Map, Value as Json};

use crate::net::Hosts;

/// What a secret setting is answered as. Sent back unchanged, it keeps the stored value.
pub const MASK: &str = "********";

#[derive(Clone, Debug)]
pub struct Field {
    pub key: String,
    pub kind: String,
    pub default: Json,
    pub required: bool,
    pub label: String,
    /// A `list` answered redacted entry by entry (`"secret": true`).
    pub secret: bool,
}

#[derive(Clone, Debug)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub hosts: Hosts,
    pub config: Vec<Field>,
    pub refresh_minutes: u64,
    /// The lowest rex-addons it needs, if it says.
    pub host: Option<String>,
    pub raw: Json,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn fits(kind: &str, value: &Json) -> bool {
    match kind {
        "text" | "secret" => value.is_string(),
        "bool" => value.is_boolean(),
        "number" => value.is_number(),
        "list" => value.as_array().is_some_and(|items| items.iter().all(Json::is_string)),
        _ => false,
    }
}

fn dotted(version: &str) -> bool {
    !version.is_empty() && version.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// Whether this rex-addons is `host` or newer.
pub fn host_fits(host: Option<&str>) -> bool {
    host.is_none_or(|host| !newer(host, env!("CARGO_PKG_VERSION")))
}

impl Manifest {
    pub fn parse(raw: &Json) -> Result<Manifest, String> {
        let text = |key: &str| raw.get(key).and_then(Json::as_str).map(str::to_string);
        let id = text("id").filter(|id| valid_id(id)).ok_or("manifest: id must be lowercase letters, digits and -, at most 64")?;
        let name = text("name").filter(|name| !name.trim().is_empty()).ok_or("manifest: name is missing")?;
        let version = text("version").filter(|v| dotted(v)).ok_or("manifest: version must be numbers separated by dots, such as 1.2.0")?;
        let host = match raw.get("host") {
            None | Some(Json::Null) => None,
            Some(Json::String(host)) if dotted(host) => Some(host.clone()),
            Some(_) => return Err("manifest: host must be numbers separated by dots, such as 0.2".into()),
        };
        let description = match raw.get("description") {
            None | Some(Json::Null) => String::new(),
            Some(Json::String(text)) => text.clone(),
            Some(_) => return Err("manifest: description must be text".into()),
        };
        let hosts = match raw.get("hosts") {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Array(hosts)) => hosts
                .iter()
                .map(|host| {
                    host.as_str()
                        .map(str::to_ascii_lowercase)
                        .filter(|host| Hosts::valid_pattern(host))
                        .ok_or(format!("manifest: {host} is no host name, *.domain or *"))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("manifest: hosts must be a list".into()),
        };
        let mut config = Vec::new();
        for entry in raw.get("config").and_then(Json::as_array).into_iter().flatten() {
            let key = entry.get("key").and_then(Json::as_str).unwrap_or("");
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("manifest: config key {key:?} must be letters, digits and _"));
            }
            if config.iter().any(|field: &Field| field.key == key) {
                return Err(format!("manifest: config key {key} is listed twice"));
            }
            let kind = entry.get("type").and_then(Json::as_str).unwrap_or("");
            if !matches!(kind, "text" | "secret" | "bool" | "number" | "list") {
                return Err(format!("manifest: config {key}: type must be text, secret, bool, number or list"));
            }
            let default = entry.get("default").cloned().unwrap_or(Json::Null);
            if !default.is_null() && !fits(kind, &default) {
                return Err(format!("manifest: config {key}: the default is no {kind}"));
            }
            config.push(Field {
                key: key.into(),
                kind: kind.into(),
                default,
                required: entry.get("required").and_then(Json::as_bool).unwrap_or(false),
                label: entry.get("label").and_then(Json::as_str).unwrap_or(key).into(),
                secret: entry.get("secret").and_then(Json::as_bool).unwrap_or(false),
            });
        }
        if raw.get("config").is_some_and(|c| !c.is_null() && !c.is_array()) {
            return Err("manifest: config must be a list".into());
        }
        let refresh_minutes = match raw.get("refresh_minutes") {
            None | Some(Json::Null) => 360,
            Some(value) => value.as_u64().filter(|m| *m >= 5).ok_or("manifest: refresh_minutes must be a whole number, 5 or more")?,
        };
        Ok(Manifest { id, name, version, description, hosts: Hosts(hosts), config, refresh_minutes, host, raw: raw.clone() })
    }

    /// Installing refuses an addon that needs a newer rex-addons.
    pub fn check_host(&self) -> Result<(), String> {
        match &self.host {
            Some(host) if !host_fits(Some(host)) => Err(format!("this addon needs rex-addons {host} or newer")),
            _ => Ok(()),
        }
    }

    /// The settings an addon is called with: each field's default, then what was saved.
    pub fn effective(&self, saved: &Map<String, Json>) -> Map<String, Json> {
        self.config
            .iter()
            .map(|field| (field.key.clone(), saved.get(&field.key).cloned().unwrap_or_else(|| field.default.clone())))
            .collect()
    }

    /// The first required setting that is empty, by its label.
    pub fn missing(&self, saved: &Map<String, Json>) -> Option<String> {
        let effective = self.effective(saved);
        self.config
            .iter()
            .find(|field| {
                field.required
                    && match &effective[&field.key] {
                        Json::Null => true,
                        Json::String(text) => text.trim().is_empty(),
                        Json::Array(items) => items.is_empty(),
                        _ => false,
                    }
            })
            .map(|field| field.label.clone())
    }

    pub fn masked(&self, saved: &Map<String, Json>) -> Map<String, Json> {
        let mut effective = self.effective(saved);
        for field in self.config.iter().filter(|field| field.kind == "secret") {
            if effective[&field.key].as_str().is_some_and(|value| !value.is_empty()) {
                effective[&field.key] = json!(MASK);
            }
        }
        for field in self.config.iter().filter(|field| field.kind == "list" && field.secret) {
            if let Some(list) = effective[&field.key].as_array() {
                effective[&field.key] = list.iter().map(|entry| json!(redact(entry.as_str().unwrap_or("")))).collect();
            }
        }
        effective
    }

    /// The secret values set now, longest first, for masking in what is logged for the addon:
    /// each `secret` text, each entry of a secret list and, for a link, its part after the
    /// host (an addon may print only that). Shorter than 6 characters is too common to mask.
    pub fn secrets(&self, saved: &Map<String, Json>) -> Vec<String> {
        let effective = self.effective(saved);
        let mut found = Vec::new();
        for field in &self.config {
            match &effective[&field.key] {
                Json::String(value) if field.kind == "secret" => found.push(value.clone()),
                Json::Array(entries) if field.kind == "list" && field.secret => {
                    for entry in entries.iter().filter_map(Json::as_str) {
                        found.push(entry.to_string());
                        let rest = entry.split_once("://").map_or("", |(_, rest)| rest);
                        if let Some(at) = rest.find(['/', '?', '#']) {
                            found.push(rest[at..].to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        found.retain(|value| value.chars().count() >= 6);
        found.sort_by_key(|value| std::cmp::Reverse(value.len()));
        found.dedup();
        found
    }

    /// `saved` changed by a PUT: each key given replaces its value, `null` puts back the
    /// default, a secret sent as the mask keeps what is stored. Keys not given are kept.
    pub fn apply(&self, saved: &Map<String, Json>, patch: &Json) -> Result<Map<String, Json>, String> {
        let patch = patch.as_object().ok_or("settings must be a JSON object")?;
        let mut next = saved.clone();
        for (key, value) in patch {
            let field = self.config.iter().find(|field| &field.key == key).ok_or(format!("{key} is no setting of this addon"))?;
            if value.is_null() {
                next.remove(key);
            } else if field.kind == "secret" && value.as_str() == Some(MASK) {
                continue;
            } else if field.kind == "list" && field.secret && fits("list", value) {
                // An entry sent back as it was answered keeps the saved one it stands for.
                let effective = self.effective(saved);
                let mut unused: Vec<Option<&str>> = effective[key].as_array().into_iter().flatten().map(Json::as_str).collect();
                let entries = value.as_array().unwrap().iter().map(|entry| {
                    let entry = entry.as_str().unwrap();
                    let kept = unused.iter_mut().find(|s| s.is_some_and(|s| redact(s) == entry)).and_then(Option::take);
                    json!(kept.unwrap_or(entry))
                });
                next.insert(key.clone(), entries.collect());
            } else if fits(&field.kind, value) {
                next.insert(key.clone(), value.clone());
            } else {
                return Err(format!("{}: must be {}", field.label, match field.kind.as_str() {
                    "list" => "a list of text",
                    "bool" => "true or false",
                    "number" => "a number",
                    _ => "text",
                }));
            }
        }
        Ok(next)
    }
}

/// A secret list's entry as answered: an http(s) link keeps its scheme and host.
fn redact(entry: &str) -> String {
    match url::Url::parse(entry) {
        Ok(link) if matches!(link.scheme(), "http" | "https") && link.host_str().is_some() => {
            format!("{}://{}/{MASK}", link.scheme(), link.host_str().unwrap())
        }
        _ => MASK.to_string(),
    }
}

/// `1.10.0` is newer than `1.9.3`.
pub fn newer(candidate: &str, installed: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parts(candidate) > parts(installed)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

pub fn read_json(path: &Path) -> Option<Json> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest::parse(&json!({
            "id": "demo", "name": "Demo", "version": "1.2.0", "hosts": ["example.com", "*.cdn.example"],
            "config": [
                {"key": "urls", "label": "Links", "type": "list", "required": true},
                {"key": "token", "label": "Token", "type": "secret"},
                {"key": "hd", "label": "HD only", "type": "bool", "default": false},
                {"key": "limit", "label": "How many", "type": "number", "default": 50}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn manifest_refusals() {
        for (bad, says) in [
            (json!({"name": "x", "version": "1"}), "id"),
            (json!({"id": "Bad_Id", "name": "x", "version": "1"}), "id"),
            (json!({"id": "a", "version": "1"}), "name"),
            (json!({"id": "a", "name": "x", "version": "v1"}), "version"),
            (json!({"id": "a", "name": "x", "version": "1", "hosts": ["http://x.com/"]}), "host"),
            (json!({"id": "a", "name": "x", "version": "1", "config": [{"key": "k", "type": "color"}]}), "type"),
            (json!({"id": "a", "name": "x", "version": "1", "config": [{"key": "k", "type": "number", "default": "5"}]}), "default"),
            (json!({"id": "a", "name": "x", "version": "1", "refresh_minutes": 1}), "refresh_minutes"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "v0.2"}), "host"),
            (json!({"id": "a", "name": "x", "version": "1", "host": 2}), "host"),
        ] {
            let error = Manifest::parse(&bad).unwrap_err();
            assert!(error.contains(says), "{bad}: {error}");
        }
        assert_eq!(manifest().refresh_minutes, 360);
    }

    #[test]
    fn config_validation_defaults_and_secrets() {
        let m = manifest();
        assert_eq!(m.missing(&Map::new()).as_deref(), Some("Links"));
        let saved = m.apply(&Map::new(), &json!({"urls": ["https://a/manifest.json"], "token": "s3cret"})).unwrap();
        assert_eq!(m.missing(&saved), None);
        assert_eq!(m.effective(&saved)["limit"], json!(50));
        assert_eq!(m.masked(&saved)["token"], json!(MASK));
        // The app sends the masked value back: the secret stays.
        let again = m.apply(&saved, &json!({"token": MASK, "hd": true})).unwrap();
        assert_eq!(again["token"], json!("s3cret"));
        assert_eq!(again["hd"], json!(true));
        // null puts the default back.
        assert!(!m.apply(&again, &json!({"hd": null})).unwrap().contains_key("hd"));
        for (bad, says) in [(json!({"nope": 1}), "no setting"), (json!({"limit": "5"}), "a number"), (json!({"urls": [1]}), "a list"), (json!([1]), "object")] {
            let error = m.apply(&saved, &bad).unwrap_err();
            assert!(error.contains(says), "{bad}: {error}");
        }
    }

    #[test]
    fn secret_lists_are_redacted_entry_by_entry() {
        let m = Manifest::parse(&json!({"id": "s", "name": "S", "version": "1", "config": [
            {"key": "addons", "label": "Addons", "type": "list", "secret": true},
            {"key": "plain", "label": "Plain", "type": "list"}
        ]}))
        .unwrap();
        let typed = ["https://a.example/TOKEN1/manifest.json", "https://a.example/TOKEN2/manifest.json", "not a link", "http://b.example:8080/k"];
        let saved = m.apply(&Map::new(), &json!({"addons": typed, "plain": ["https://a.example/open"]})).unwrap();
        let shown = m.masked(&saved);
        assert_eq!(shown["addons"], json!(["https://a.example/********", "https://a.example/********", "********", "http://b.example/********"]));
        assert_eq!(shown["plain"], json!(["https://a.example/open"]));
        // Sent back as shown, with one removed, one moved and one added: the saved links stay.
        let again = m.apply(&saved, &json!({"addons": ["http://b.example/********", "https://a.example/********", "https://a.example/********", "https://c.example/new"]})).unwrap();
        assert_eq!(again["addons"], json!(["http://b.example:8080/k", typed[0], typed[1], "https://c.example/new"]));
        // A redacted entry nothing saved stands for is taken as typed.
        let third = m.apply(&saved, &json!({"addons": ["https://z.example/********"]})).unwrap();
        assert_eq!(third["addons"], json!(["https://z.example/********"]));
    }

    #[test]
    fn secrets_are_the_values_and_link_paths_worth_masking() {
        let m = Manifest::parse(&json!({"id": "s", "name": "S", "version": "1", "config": [
            {"key": "token", "label": "Token", "type": "secret"},
            {"key": "pin", "label": "PIN", "type": "secret"},
            {"key": "addons", "label": "Addons", "type": "list", "secret": true},
            {"key": "name", "label": "Name", "type": "text"},
            {"key": "plain", "label": "Plain", "type": "list"}
        ]}))
        .unwrap();
        let saved = m.apply(&Map::new(), &json!({
            "token": "tok-123456", "pin": "1234", "name": "visible-name",
            "addons": ["https://a.example/realdebrid=KEY123/manifest.json", "not-a-link-secret", "https://b.example"],
            "plain": ["https://c.example/open-path"]
        })).unwrap();
        assert_eq!(m.secrets(&saved), [
            "https://a.example/realdebrid=KEY123/manifest.json",
            "/realdebrid=KEY123/manifest.json",
            "not-a-link-secret",
            "https://b.example",
            "tok-123456",
        ]);
    }

    #[test]
    fn host_version() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.2.0");
        let needing = |host: &str| Manifest::parse(&json!({"id": "a", "name": "x", "version": "1", "host": host})).unwrap();
        for fits in ["0.2", "0.2.0", "0.1.9", "0"] {
            assert_eq!(needing(fits).check_host(), Ok(()), "{fits}");
        }
        for too_new in ["0.2.1", "0.3", "1"] {
            assert_eq!(needing(too_new).check_host().unwrap_err(), format!("this addon needs rex-addons {too_new} or newer"));
        }
        assert_eq!(manifest().host, None);
        assert_eq!(manifest().check_host(), Ok(()));
    }

    #[test]
    fn versions() {
        assert!(newer("1.10.0", "1.9.3"));
        assert!(newer("2", "1.9"));
        assert!(!newer("1.2.0", "1.2.0"));
        assert!(!newer("1.2", "1.2.0"));
    }
}
