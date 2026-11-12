//! What an addon is on disk: its manifest, its settings, and what the host keeps for it.
use std::path::Path;

use serde_json::{json, Map, Value as Json};

use crate::net::{self, Hosts};

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
    /// A `select`'s option values.
    pub options: Vec<String>,
}

/// How an addon signs in to the person's account.
#[derive(Clone, Debug, PartialEq)]
pub struct SignIn {
    /// What the app's button says.
    pub label: String,
    /// `code`: the addon's own `signIn()`, a code typed on another device. `spotify`: the host
    /// pairs with the Spotify app over the local network and keeps the session itself.
    pub kind: String,
}

impl SignIn {
    pub fn spotify(&self) -> bool {
        self.kind == "spotify"
    }
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
    /// When it needs the person's account on a website.
    pub sign_in: Option<SignIn>,
    /// Functions of its default export other addons may call (`addons.call`).
    pub exports: Vec<String>,
    /// Addons whose exports it calls.
    pub requires: Vec<String>,
    pub raw: Json,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn fits(field: &Field, value: &Json) -> bool {
    match field.kind.as_str() {
        "select" => value.as_str().is_some_and(|value| field.options.iter().any(|option| option == value)),
        "text" | "secret" => value.is_string(),
        "bool" => value.is_boolean(),
        "number" => value.is_number(),
        "list" => value.as_array().is_some_and(|items| items.iter().all(Json::is_string)),
        // Blank is no address: it grants nothing, and a required one is missing.
        "host" => value.as_str().is_some_and(|text| text.trim().is_empty() || net::typed_grant(text).is_ok()),
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
            if !matches!(kind, "text" | "secret" | "bool" | "number" | "list" | "select" | "host") {
                return Err(format!("manifest: config {key}: type must be text, secret, bool, number, list, select or host"));
            }
            let options = match (kind, entry.get("options")) {
                ("select", Some(Json::Array(options))) if !options.is_empty() => {
                    let mut values: Vec<String> = Vec::new();
                    for option in options {
                        let (Some(value), Some(_)) = (option.get("value").and_then(Json::as_str), option.get("label").and_then(Json::as_str)) else {
                            return Err(format!("manifest: config {key}: each option must be {{\"value\": text, \"label\": text}}"));
                        };
                        if values.iter().any(|v| v == value) {
                            return Err(format!("manifest: config {key}: the option {value:?} is listed twice"));
                        }
                        values.push(value.to_string());
                    }
                    values
                }
                ("select", _) => return Err(format!("manifest: config {key}: a select needs options, a list of {{\"value\", \"label\"}}")),
                (_, None | Some(Json::Null)) => Vec::new(),
                (_, Some(_)) => return Err(format!("manifest: config {key}: only a select has options")),
            };
            if entry.get("help").is_some_and(|help| !help.is_null() && !help.is_string()) {
                return Err(format!("manifest: config {key}: help must be text"));
            }
            let field = Field {
                key: key.into(),
                kind: kind.into(),
                default: entry.get("default").cloned().unwrap_or(Json::Null),
                required: entry.get("required").and_then(Json::as_bool).unwrap_or(false),
                label: entry.get("label").and_then(Json::as_str).unwrap_or(key).into(),
                secret: entry.get("secret").and_then(Json::as_bool).unwrap_or(false),
                options,
            };
            // Only what the person types is reached: an addon cannot grant itself a host.
            if kind == "host" && !field.default.is_null() {
                return Err(format!("manifest: config {key}: a host setting has no default: the person types it"));
            }
            if !field.default.is_null() && !fits(&field, &field.default) {
                return Err(match kind {
                    "select" => format!("manifest: config {key}: the default is not one of its options"),
                    _ => format!("manifest: config {key}: the default is no {kind}"),
                });
            }
            config.push(field);
        }
        if raw.get("config").is_some_and(|c| !c.is_null() && !c.is_array()) {
            return Err("manifest: config must be a list".into());
        }
        let refresh_minutes = match raw.get("refresh_minutes") {
            None | Some(Json::Null) => 360,
            Some(value) => value.as_u64().filter(|m| *m >= 5).ok_or("manifest: refresh_minutes must be a whole number, 5 or more")?,
        };
        let sign_in = match raw.get("sign_in") {
            None | Some(Json::Null) => None,
            Some(Json::Object(sign_in)) => Some(SignIn {
                label: match sign_in.get("label") {
                    None | Some(Json::Null) => format!("Sign in to {name}"),
                    Some(Json::String(label)) if !label.trim().is_empty() => label.clone(),
                    Some(_) => return Err("manifest: sign_in.label must be text".into()),
                },
                kind: match sign_in.get("kind") {
                    None | Some(Json::Null) => "code".into(),
                    Some(Json::String(kind)) if kind == "code" || kind == "spotify" => kind.clone(),
                    Some(_) => return Err("manifest: sign_in.kind must be \"code\" or \"spotify\"".into()),
                },
            }),
            Some(_) => return Err("manifest: sign_in must be an object, such as {\"label\": \"Sign in to Example\"}".into()),
        };
        let names = |key: &str, valid: fn(&str) -> bool, what: &str| -> Result<Vec<String>, String> {
            match raw.get(key) {
                None | Some(Json::Null) => Ok(Vec::new()),
                Some(Json::Array(list)) => list
                    .iter()
                    .map(|entry| entry.as_str().filter(|e| valid(e)).map(str::to_string).ok_or(format!("manifest: {key}: {entry} is no {what}")))
                    .collect(),
                Some(_) => Err(format!("manifest: {key} must be a list")),
            }
        };
        let exports = names("exports", |name| {
            name.len() <= 64 && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }, "function name")?;
        let requires = names("requires", valid_id, "addon id")?;
        if requires.contains(&id) {
            return Err("manifest: an addon cannot require itself".into());
        }
        // rex's own `directories` (docs/feed-api.md), passed on as they are when the addon registers.
        if raw.get("directories").is_some_and(|d| !d.is_null() && !d.is_array()) {
            return Err("manifest: directories must be a list, such as [{\"name\": \"youtube\", \"shows\": \"YouTube\"}]".into());
        }
        // An older rex-addons would install it and never sign it in.
        if sign_in.is_some() && host.as_deref().is_none_or(|host| newer("0.3", host)) {
            return Err("manifest: sign_in needs \"host\": \"0.3\" or newer".into());
        }
        // Fields an older rex-addons would install and ignore.
        let spotify = sign_in.as_ref().is_some_and(SignIn::spotify);
        if (spotify || !exports.is_empty() || !requires.is_empty()) && host.as_deref().is_none_or(|host| newer("0.5", host)) {
            return Err("manifest: sign_in kind spotify, exports and requires need \"host\": \"0.5\" or newer".into());
        }
        // An older rex-addons refuses the type with a message that doesn't say why.
        if config.iter().any(|field| field.kind == "select") && host.as_deref().is_none_or(|host| newer("0.6", host)) {
            return Err("manifest: a select setting needs \"host\": \"0.6\" or newer".into());
        }
        if config.iter().any(|field| field.kind == "host") && host.as_deref().is_none_or(|host| newer("0.7", host)) {
            return Err("manifest: a host setting needs \"host\": \"0.7\" or newer".into());
        }
        Ok(Manifest { id, name, version, description, hosts: Hosts::from(hosts), config, refresh_minutes, host, sign_in, exports, requires, raw: raw.clone() })
    }

    /// Installing refuses an addon that needs a newer rex-addons.
    pub fn check_host(&self) -> Result<(), String> {
        match &self.host {
            Some(host) if !host_fits(Some(host)) => Err(format!("this addon needs rex-addons {host} or newer")),
            _ => Ok(()),
        }
    }

    /// The settings an addon is called with: each field's default, then what was saved. A saved
    /// option a new version no longer offers gives the default.
    pub fn effective(&self, saved: &Map<String, Json>) -> Map<String, Json> {
        self.config
            .iter()
            .map(|field| {
                let saved = saved.get(&field.key).filter(|value| !matches!(field.kind.as_str(), "select" | "host") || fits(field, value));
                (field.key.clone(), saved.cloned().unwrap_or_else(|| field.default.clone()))
            })
            .collect()
    }

    /// The required settings that are empty, by their labels.
    pub fn missing(&self, saved: &Map<String, Json>) -> Vec<String> {
        let effective = self.effective(saved);
        self.config
            .iter()
            .filter(|field| {
                field.required
                    && match &effective[&field.key] {
                        Json::Null => true,
                        Json::String(text) => text.trim().is_empty(),
                        Json::Array(items) => items.is_empty(),
                        _ => false,
                    }
            })
            .map(|field| field.label.clone())
            .collect()
    }

    /// What the addon reaches with these settings: its manifest's hosts, and each host and port
    /// the person typed into its `host` settings.
    pub fn reach(&self, saved: &Map<String, Json>) -> Hosts {
        let effective = self.effective(saved);
        let mut granted: Vec<(String, u16)> = self
            .config
            .iter()
            .filter(|field| field.kind == "host")
            .filter_map(|field| net::typed_grant(effective[&field.key].as_str()?).ok())
            .flat_map(|(host, ports)| ports.into_iter().map(move |port| (host.clone(), port)))
            .collect();
        granted.sort();
        granted.dedup();
        Hosts { listed: self.hosts.listed.clone(), granted }
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
            } else if field.kind == "list" && field.secret && value.as_array().is_some_and(|items| items.iter().all(Json::is_string)) {
                // An entry sent back as it was answered keeps the saved one it stands for.
                let effective = self.effective(saved);
                let mut unused: Vec<Option<&str>> = effective[key].as_array().into_iter().flatten().map(Json::as_str).collect();
                let entries = value.as_array().unwrap().iter().map(|entry| {
                    let entry = entry.as_str().unwrap();
                    let kept = unused.iter_mut().find(|s| s.is_some_and(|s| redact(s) == entry)).and_then(Option::take);
                    json!(kept.unwrap_or(entry))
                });
                next.insert(key.clone(), entries.collect());
            } else if fits(field, value) {
                next.insert(key.clone(), value.clone());
            } else {
                if let Some(Err(why)) = value.as_str().filter(|_| field.kind == "host").map(net::typed_grant) {
                    return Err(format!("{}: {why}", field.label));
                }
                return Err(format!("{}: must be {}", field.label, match field.kind.as_str() {
                    "list" => "a list of text".to_string(),
                    "host" => "an address, as text".into(),
                    "bool" => "true or false".into(),
                    "number" => "a number".into(),
                    "select" => format!("one of {}", field.options.join(", ")),
                    _ => "text".into(),
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
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.6", "config": [{"key": "k", "type": "select"}]}), "a select needs options"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.6", "config": [{"key": "k", "type": "select", "options": []}]}), "a select needs options"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.6", "config": [{"key": "k", "type": "select", "options": [{"value": 1, "label": "One"}]}]}), "each option must be"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.6", "config": [{"key": "k", "type": "select", "options": [{"value": "1"}]}]}), "each option must be"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.6", "config": [{"key": "k", "type": "select", "options": [{"value": "1", "label": "a"}, {"value": "1", "label": "b"}]}]}), "\"1\" is listed twice"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.6", "config": [{"key": "k", "type": "select", "default": "2", "options": [{"value": "1", "label": "One"}]}]}), "the default is not one of its options"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.6", "config": [{"key": "k", "type": "text", "options": [{"value": "1", "label": "One"}]}]}), "only a select has options"),
            (json!({"id": "a", "name": "x", "version": "1", "config": [{"key": "k", "type": "select", "options": [{"value": "1", "label": "One"}]}]}), "\"host\": \"0.6\""),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.5", "config": [{"key": "k", "type": "select", "options": [{"value": "1", "label": "One"}]}]}), "\"host\": \"0.6\""),
            (json!({"id": "a", "name": "x", "version": "1", "config": [{"key": "k", "type": "text", "help": 5}]}), "help must be text"),
            (json!({"id": "a", "name": "x", "version": "1", "config": [{"key": "k", "type": "host"}]}), "a host setting needs \"host\": \"0.7\" or newer"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.6", "config": [{"key": "k", "type": "host"}]}), "a host setting needs \"host\": \"0.7\" or newer"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.7", "config": [{"key": "k", "type": "host", "default": "192.168.1.1"}]}), "a host setting has no default"),
            (json!({"id": "a", "name": "x", "version": "1", "config": [{"key": "k", "type": "number", "default": "5"}]}), "default"),
            (json!({"id": "a", "name": "x", "version": "1", "refresh_minutes": 1}), "refresh_minutes"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "v0.2"}), "host"),
            (json!({"id": "a", "name": "x", "version": "1", "host": 2}), "host"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.3", "sign_in": true}), "sign_in"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.3", "sign_in": {"label": 5}}), "sign_in.label"),
            (json!({"id": "a", "name": "x", "version": "1", "sign_in": {}}), "\"host\": \"0.3\""),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.2.9", "sign_in": {}}), "\"host\": \"0.3\""),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.5", "sign_in": {"kind": "password"}}), "sign_in.kind"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.4", "sign_in": {"kind": "spotify"}}), "\"host\": \"0.5\""),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.4", "exports": ["find"]}), "\"host\": \"0.5\""),
            (json!({"id": "a", "name": "x", "version": "1", "requires": ["youtube"]}), "\"host\": \"0.5\""),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.5", "exports": ["find me"]}), "function name"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.5", "exports": "find"}), "exports must be a list"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.5", "requires": ["YouTube"]}), "addon id"),
            (json!({"id": "a", "name": "x", "version": "1", "host": "0.5", "requires": ["a"]}), "require itself"),
        ] {
            let error = Manifest::parse(&bad).unwrap_err();
            assert!(error.contains(says), "{bad}: {error}");
        }
        assert_eq!(manifest().refresh_minutes, 360);
        let signing = |sign_in: Json| Manifest::parse(&json!({"id": "a", "name": "Example", "version": "1", "host": "0.5", "sign_in": sign_in})).unwrap().sign_in;
        let sign_in = |label: &str, kind: &str| Some(SignIn { label: label.into(), kind: kind.into() });
        assert_eq!(signing(json!({})), sign_in("Sign in to Example", "code"));
        assert_eq!(signing(json!({"label": "Use your Google account"})), sign_in("Use your Google account", "code"));
        assert_eq!(signing(json!({"kind": "spotify", "label": "Connect Spotify"})), sign_in("Connect Spotify", "spotify"));
        assert_eq!(signing(Json::Null), None);
        assert_eq!(manifest().sign_in, None);
        let both = Manifest::parse(&json!({"id": "a", "name": "x", "version": "1", "host": "0.5", "exports": ["find", "_open2"], "requires": ["youtube"]})).unwrap();
        assert_eq!((both.exports, both.requires), (vec!["find".to_string(), "_open2".into()], vec!["youtube".to_string()]));
        assert_eq!((manifest().exports.len(), manifest().requires.len()), (0, 0));
    }

    #[test]
    fn config_validation_defaults_and_secrets() {
        let m = manifest();
        assert_eq!(m.missing(&Map::new()), ["Links"]);
        let saved = m.apply(&Map::new(), &json!({"urls": ["https://a/manifest.json"], "token": "s3cret"})).unwrap();
        assert!(m.missing(&saved).is_empty());
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

    /// A select takes one of its options' values, on install and on a PUT; `help` is any field's
    /// text for the app, and needs no newer rex-addons.
    #[test]
    fn select_settings_take_only_their_options() {
        let m = Manifest::parse(&json!({"id": "s", "name": "S", "version": "1", "host": "0.6", "config": [
            {"key": "quality", "label": "Highest quality", "type": "select", "default": "1080", "help": "720 uses less data",
             "options": [{"value": "1080", "label": "1080p"}, {"value": "720", "label": "720p"}]},
            {"key": "lang", "label": "Language", "type": "select", "options": [{"value": "en", "label": "English"}]}
        ]}))
        .unwrap();
        assert_eq!(m.config[0].options, ["1080", "720"]);
        assert_eq!(m.effective(&Map::new()), json!({"quality": "1080", "lang": null}).as_object().unwrap().clone());
        let saved = m.apply(&Map::new(), &json!({"quality": "720"})).unwrap();
        assert_eq!(m.effective(&saved)["quality"], json!("720"));
        for bad in [json!({"quality": "480"}), json!({"quality": 720}), json!({"quality": ["720"]})] {
            assert_eq!(m.apply(&saved, &bad).unwrap_err(), "Highest quality: must be one of 1080, 720");
        }
        // A saved option a newer version dropped gives the default.
        let dropped = json!({"quality": "2160"}).as_object().unwrap().clone();
        assert_eq!(m.effective(&dropped)["quality"], json!("1080"));
        assert!(Manifest::parse(&json!({"id": "a", "name": "x", "version": "1", "config": [{"key": "k", "type": "text", "help": "Shown under it"}]})).is_ok());
    }

    /// A host setting takes an address the person types, kept as typed; the host and port in it,
    /// and only that, join what the addon reaches. Never a wildcard, a user, or this device.
    #[test]
    fn host_settings_take_an_address_and_grant_its_host_and_port() {
        let m = Manifest::parse(&json!({"id": "h", "name": "H", "version": "1", "host": "0.7", "hosts": ["example.com"], "config": [
            {"key": "server", "label": "Server", "type": "host", "required": true},
            {"key": "backup", "label": "Backup server", "type": "host"}
        ]}))
        .unwrap();
        let granted = |saved: &Map<String, Json>| m.reach(saved).granted;
        let grant = |host: &str, ports: &[u16]| ports.iter().map(|port| (host.to_string(), *port)).collect::<Vec<_>>();
        assert_eq!(m.missing(&Map::new()), ["Server"]);
        assert!(granted(&Map::new()).is_empty());
        for (typed, host, ports) in [
            ("192.168.1.20:8096", "192.168.1.20", &[8096][..]),
            (" http://NAS.Local:8096/jellyfin ", "nas.local", &[8096]),
            ("https://[fd00::5]:8920", "fd00::5", &[8920]),
            ("[fd00::5]:8920", "fd00::5", &[8920]),
            // A scheme's own port; a bare name either scheme's; a bare port typed, that one.
            ("http://nas", "nas", &[80]),
            ("https://nas/", "nas", &[443]),
            ("http://nas:443", "nas", &[443]),
            ("nas", "nas", &[80, 443]),
            ("[fd00::5]", "fd00::5", &[80, 443]),
            ("nas:80", "nas", &[80]),
            ("nas:443", "nas", &[443]),
            ("http://192.168.010.1/", "192.168.8.1", &[80]),
        ] {
            let saved = m.apply(&Map::new(), &json!({"server": typed})).unwrap();
            assert_eq!(m.effective(&saved)["server"], json!(typed), "kept as typed");
            assert_eq!(granted(&saved), grant(host, ports), "{typed}");
            let reach = m.reach(&saved);
            assert!(ports.iter().all(|port| reach.allows(host, *port)) && reach.allows("example.com", 8080), "{typed}");
            assert!(!reach.allows(host, 32400), "{typed}: another port of the same host");
        }
        // Two settings, each its own host and port; blank grants nothing and leaves a required one missing.
        let both = m.apply(&Map::new(), &json!({"server": "10.0.0.2:8096", "backup": "10.0.0.3"})).unwrap();
        assert_eq!(granted(&both), [("10.0.0.2".to_string(), 8096), ("10.0.0.3".into(), 80), ("10.0.0.3".into(), 443)]);
        assert_eq!(m.reach(&both).port_note("10.0.0.3", 32400), ": its settings allow 10.0.0.3 on port 80 and 443 only, not 32400");
        let blank = m.apply(&both, &json!({"server": "  ", "backup": null})).unwrap();
        assert!(granted(&blank).is_empty());
        assert_eq!(m.missing(&blank), ["Server"]);
        assert!(!m.reach(&both).allows("10.0.0.4", 8096));
        let shape = "Server: give an address such as 192.168.1.20:8096 or http://nas.local:8096";
        let device = "Server: that is this device, which addons never reach";
        for (typed, says) in [
            (json!("*"), shape), (json!("*.lan"), shape), (json!("nas lan"), shape), (json!("http://"), shape), (json!("nas_box"), shape),
            (json!("nas:0"), shape), (json!("nas:99999"), shape),
            (json!("ftp://nas"), "Server: give an http:// or https:// address"),
            (json!("user:pw@nas"), "Server: leave the user name and password out of the address"),
            (json!("http://user@nas:8096"), "Server: leave the user name and password out of the address"),
            (json!(5), "Server: must be an address, as text"), (json!(["nas"]), "Server: must be an address, as text"),
            (json!("127.0.0.1:32410"), device), (json!("http://127.0.0.2/"), device), (json!("127.1"), device), (json!("0x7f.0.0.1"), device),
            (json!("2130706433"), device), (json!("localhost:32400"), device), (json!("LOCALHOST"), device), (json!("rex.localhost"), device),
            (json!("[::1]:32410"), device), (json!("[::ffff:127.0.0.1]"), device), (json!("0.0.0.0:32400"), device), (json!("[::]"), device),
        ] {
            assert_eq!(m.apply(&both, &json!({"server": typed})).unwrap_err(), says, "{typed}");
        }
        // Saved some other way (an older file, a hand edit), a value that is no address grants nothing.
        let odd = json!({"server": "127.0.0.1", "backup": "*"}).as_object().unwrap().clone();
        assert!(granted(&odd).is_empty());
        assert_eq!(m.effective(&odd)["server"], Json::Null);
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
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.7.0");
        let needing = |host: &str| Manifest::parse(&json!({"id": "a", "name": "x", "version": "1", "host": host})).unwrap();
        for fits in ["0.7", "0.7.0", "0.6", "0.5", "0.4", "0.3", "0.2", "0.1.9", "0"] {
            assert_eq!(needing(fits).check_host(), Ok(()), "{fits}");
        }
        for too_new in ["0.7.1", "0.8", "1"] {
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
