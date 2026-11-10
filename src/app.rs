//! The host's state and work: installed addons, their syncs to rex, and what they open.
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value as Json};

use crate::addons::{newer, read_json, write_atomic, Manifest};
use crate::net;
use crate::sandbox::{self, Env, Storage};

/// How long an `open()` answer is reused for the byte reads of one item.
const OPEN_TTL: Duration = Duration::from_secs(10 * 60);
/// How soon a sync that failed is tried again.
const RETRY: Duration = Duration::from_secs(5 * 60);
/// Items rex is sent per request, as feed-api.md asks.
const CHUNK: usize = 200;
/// Probed items go to rex in batches this big, so they show before the whole sync is done.
const READY_BATCH: usize = 20;
const PROBE_WORKERS: usize = 4;

pub fn log(tag: &str, message: &str) {
    println!("[{tag}] {message}");
}

pub fn warn(tag: &str, message: &str) {
    eprintln!("WARN [{tag}] {message}");
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[derive(Clone)]
pub struct Opened {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

pub struct Addon {
    pub manifest: Arc<Manifest>,
    pub source: Arc<String>,
    pub config: Map<String, Json>,
    pub storage: Arc<Storage>,
    pub enabled: bool,
    pub last_sync: Option<u64>,
    pub items: usize,
    pub held: usize,
    pub error: Option<String>,
    /// Ids a person deleted in a Plex app: never pushed again.
    pub deleted: BTreeSet<String>,
    /// Each pushed path (and each id) to its item id: what `/bytes` reads by.
    pub paths: HashMap<String, String>,
    pub probes: Map<String, Json>,
    pub syncing: bool,
    /// The next sync may remove more than half of the addon's items.
    pub force: bool,
    /// Asked for while one was running (new settings): run again when it ends.
    pub again: bool,
    pub attempted: Option<Instant>,
    pub opened: HashMap<String, (Opened, Instant)>,
}

impl Addon {
    pub fn status(&self) -> Json {
        let m = &self.manifest;
        json!({
            "id": m.id, "name": m.name, "version": m.version, "description": m.description,
            "enabled": self.enabled,
            "state": if self.syncing { "syncing" } else if self.error.is_some() { "error" } else { "idle" },
            "items": self.items, "held": self.held, "last_sync": self.last_sync, "error": self.error,
        })
    }
}

pub struct App {
    pub data: PathBuf,
    pub rex: String,
    pub listen: SocketAddr,
    pub ffprobe: Option<String>,
    pub addons: Mutex<BTreeMap<String, Addon>>,
}

fn dir(app: &App, id: &str) -> PathBuf {
    app.data.join("addons").join(id)
}

pub fn source_name(id: &str) -> String {
    format!("addon-{id}")
}

impl App {
    pub fn new(data: PathBuf, rex: String, listen: SocketAddr, ffprobe: Option<String>, bundled: Option<&Path>) -> Arc<App> {
        std::fs::create_dir_all(data.join("addons")).expect("cannot create the data directory");
        let app = Arc::new(App { data, rex: rex.trim_end_matches('/').to_string(), listen, ffprobe, addons: Mutex::default() });
        let entries = std::fs::read_dir(app.data.join("addons")).into_iter().flatten().flatten();
        for entry in entries {
            match load(&entry.path()) {
                Ok(addon) => {
                    app.addons.lock().unwrap().insert(addon.manifest.id.clone(), addon);
                }
                Err(error) => warn("host", &format!("{}: {error}", entry.path().display())),
            }
        }
        if let Some(bundled) = bundled {
            app.install_bundled(bundled);
        }
        app
    }

    fn removed_path(&self) -> PathBuf {
        self.data.join("removed.json")
    }

    fn removed(&self) -> BTreeSet<String> {
        read_json(&self.removed_path()).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    fn set_removed(&self, id: &str, removed: bool) {
        let mut ids = self.removed();
        if if removed { ids.insert(id.into()) } else { ids.remove(id) } {
            let _ = write_atomic(&self.removed_path(), json!(ids).to_string().as_bytes());
        }
    }

    /// Bundled addons are installed on the first start and updated when the bundled version is
    /// newer, unless the person removed one.
    fn install_bundled(&self, bundled: &Path) {
        let removed = self.removed();
        for entry in std::fs::read_dir(bundled).into_iter().flatten().flatten() {
            let path = entry.path();
            let (Some(raw), Ok(source)) = (read_json(&path.join("manifest.json")), std::fs::read_to_string(path.join("main.js"))) else {
                continue;
            };
            let manifest = match Manifest::parse(&raw) {
                Ok(m) => m,
                Err(error) => {
                    warn("host", &format!("bundled {}: {error}", path.display()));
                    continue;
                }
            };
            if removed.contains(&manifest.id) {
                continue;
            }
            let installed = self.addons.lock().unwrap().get(&manifest.id).map(|a| a.manifest.version.clone());
            if installed.as_ref().is_none_or(|version| newer(&manifest.version, version)) {
                match self.install(&raw, &source) {
                    Ok(id) => log("host", &format!("installed bundled {id} {}", manifest.version)),
                    Err(error) => warn("host", &format!("bundled {}: {error}", manifest.id)),
                }
            }
        }
    }

    /// Writes an addon's files and loads it; an installed one keeps its settings and storage.
    pub fn install(&self, raw: &Json, source: &str) -> Result<String, String> {
        let manifest = Manifest::parse(raw)?;
        let storage = Arc::new(Storage::open(PathBuf::from("/dev/null")));
        let env = Env { addon: &manifest.id, source, hosts: &manifest.hosts, storage };
        sandbox::call(&env, "", &[], sandbox::OPEN).map_err(|error| format!("main.js: {error}"))?;
        let path = dir(self, &manifest.id);
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        write_atomic(&path.join("main.js"), source.as_bytes()).map_err(|e| e.to_string())?;
        write_atomic(&path.join("manifest.json"), serde_json::to_string_pretty(raw).unwrap().as_bytes()).map_err(|e| e.to_string())?;
        let mut addons = self.addons.lock().unwrap();
        let id = manifest.id.clone();
        match addons.get_mut(&id) {
            Some(addon) => {
                addon.manifest = Arc::new(manifest);
                addon.source = Arc::new(source.to_string());
                addon.opened.clear();
                addon.last_sync = None;
            }
            None => {
                let addon = load(&path)?;
                save_state(self, &addon);
                addons.insert(id.clone(), addon);
            }
        }
        drop(addons);
        self.set_removed(&id, false);
        Ok(id)
    }

    pub fn install_url(&self, url: &str) -> Result<String, String> {
        let manifest_url = url::Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
        if !matches!(manifest_url.scheme(), "http" | "https") {
            return Err("the link must be http or https".into());
        }
        let agent = net::plain_agent(Duration::from_secs(30));
        let get = |url: &str| -> Result<String, String> {
            agent.get(url).call().map_err(|e| format!("{url}: {e}"))?.into_string().map_err(|e| format!("{url}: {e}"))
        };
        let raw: Json = serde_json::from_str(&get(manifest_url.as_str())?).map_err(|e| format!("manifest.json: {e}"))?;
        let main = manifest_url.join("main.js").map_err(|e| e.to_string())?;
        let id = self.install(&raw, &get(main.as_str())?)?;
        log(&id, &format!("installed from {}", manifest_url.as_str()));
        Ok(id)
    }

    /// Removes an addon and, through rex's two-step delete, its source and everything it pushed.
    pub fn remove(&self, id: &str) -> Result<Json, String> {
        // Out of the map first: a sync still running sees it gone before its next call to rex.
        if self.addons.lock().unwrap().remove(id).is_none() {
            return Err("no such addon".into());
        }
        let rex = self.delete_source(id);
        std::fs::remove_dir_all(dir(self, id)).map_err(|e| e.to_string())?;
        self.set_removed(id, true);
        log(id, "removed");
        Ok(match rex {
            Ok(removed) => json!({"removed": true, "rex_removed": removed}),
            Err(error) => {
                warn(id, &format!("rex still holds its items: {error}"));
                json!({"removed": true, "rex_error": error})
            }
        })
    }

    /// A sync's call to rex, unless the addon was removed meanwhile.
    fn feed_call(&self, id: &str, method: &str, path: &str, body: &Json) -> Result<(u16, Json), String> {
        if !self.addons.lock().unwrap().contains_key(id) {
            return Err("removed".into());
        }
        self.rex_call(method, path, Some(body))
    }

    fn rex_call(&self, method: &str, path: &str, body: Option<&Json>) -> Result<(u16, Json), String> {
        let agent = net::plain_agent(Duration::from_secs(600));
        let request = agent.request(method, &format!("{}{path}", self.rex)).set("Content-Type", "application/json");
        let sent = match body {
            Some(body) => request.send_string(&body.to_string()),
            None => request.call(),
        };
        let response = match sent {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(error) => return Err(format!("rex at {} is not reachable: {error}", self.rex)),
        };
        let status = response.status();
        let text = response.into_string().unwrap_or_default();
        Ok((status, serde_json::from_str(&text).unwrap_or(json!({"error": text}))))
    }

    pub fn rex_reachable(&self) -> bool {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(3)).build();
        agent.get(&format!("{}/identity", self.rex)).call().is_ok()
    }

    fn delete_source(&self, id: &str) -> Result<u64, String> {
        let name = source_name(id);
        let (status, answer) = self.rex_call("DELETE", &format!("/rex/sources/{name}"), None)?;
        match (status, answer["confirm"].as_str()) {
            (404, _) => Ok(0),
            (200, Some(confirm)) => {
                let (status, answer) = self.rex_call("DELETE", confirm, None)?;
                if status != 200 {
                    return Err(format!("rex answered {status} to the confirmation: {answer}"));
                }
                Ok(answer["removed"].as_u64().unwrap_or(0))
            }
            _ => Err(format!("rex answered {status}: {answer}")),
        }
    }

    pub fn byte_base(&self, id: &str) -> String {
        format!("http://{}/bytes/{id}", net::reachable(self.listen))
    }

    /// What an item plays from now: the addon's `open()`, reused for OPEN_TTL.
    pub fn open(&self, id: &str, item: &str) -> Result<Opened, String> {
        let (manifest, source, config, storage) = {
            let addons = self.addons.lock().unwrap();
            let addon = addons.get(id).ok_or("no such addon")?;
            if let Some((opened, at)) = addon.opened.get(item) {
                if at.elapsed() < OPEN_TTL {
                    return Ok(opened.clone());
                }
            }
            (addon.manifest.clone(), addon.source.clone(), addon.config.clone(), addon.storage.clone())
        };
        let env = Env { addon: id, source: &source, hosts: &manifest.hosts, storage };
        let ctx = json!({"config": manifest.effective(&config)});
        let answer = sandbox::call(&env, "open", &[json!(item), ctx], sandbox::OPEN)?;
        let url = answer["url"].as_str().ok_or("open() returned no url")?;
        let parsed = url::Url::parse(url).map_err(|e| format!("open() returned {url}: {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https") || !manifest.hosts.allows(parsed.host_str().unwrap_or("")) {
            return Err(format!("open() returned {url}, which is not on the addon's hosts"));
        }
        let headers = answer["headers"]
            .as_object()
            .map(|h| h.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default();
        let opened = Opened { url: url.to_string(), headers };
        if let Some(addon) = self.addons.lock().unwrap().get_mut(id) {
            addon.opened.insert(item.to_string(), (opened.clone(), Instant::now()));
        }
        Ok(opened)
    }

    pub fn forget_open(&self, id: &str, item: &str) {
        if let Some(addon) = self.addons.lock().unwrap().get_mut(id) {
            addon.opened.remove(item);
        }
    }

    /// A person deleted the item in a Plex app: it is not pushed again.
    pub fn delete_item(&self, id: &str, item: &str) {
        let mut addons = self.addons.lock().unwrap();
        if let Some(addon) = addons.get_mut(id) {
            addon.deleted.insert(item.to_string());
            addon.paths.retain(|_, value| value != item);
            save_state(self, addon);
            write_index(self, addon);
        }
        log(id, &format!("{item} deleted in a Plex app: no longer pushed"));
    }

    /// Starts a sync on its own thread; one asked for while another runs follows it.
    pub fn start_sync(self: &Arc<Self>, id: &str, force: bool) -> bool {
        {
            let mut addons = self.addons.lock().unwrap();
            let Some(addon) = addons.get_mut(id) else { return false };
            addon.force |= force;
            if addon.syncing {
                addon.again = true;
                return false;
            }
            addon.syncing = true;
        }
        let (app, id) = (self.clone(), id.to_string());
        std::thread::spawn(move || app.sync(&id));
        true
    }

    /// Every enabled addon whose refresh is due, or whose last sync failed RETRY ago.
    pub fn due(&self) -> Vec<String> {
        let addons = self.addons.lock().unwrap();
        addons
            .values()
            .filter(|a| a.enabled && !a.syncing)
            .filter(|a| match (a.error.is_some(), a.attempted, a.last_sync) {
                (true, Some(at), _) => at.elapsed() >= RETRY,
                // A failure from before a restart is tried again at once.
                (true, None, _) | (false, None, None) => true,
                (false, _, Some(last)) => now() >= last + a.manifest.refresh_minutes * 60,
                (false, Some(at), None) => at.elapsed() >= RETRY,
            })
            .map(|a| a.manifest.id.clone())
            .collect()
    }

    fn sync(self: &Arc<Self>, id: &str) {
        let started = Instant::now();
        let force = match self.addons.lock().unwrap().get_mut(id) {
            Some(addon) => std::mem::take(&mut addon.force),
            None => return,
        };
        let result = self.run_sync(id, force);
        let mut addons = self.addons.lock().unwrap();
        let Some(addon) = addons.get_mut(id) else { return };
        addon.syncing = false;
        addon.attempted = Some(Instant::now());
        match result {
            Ok((items, held)) => {
                addon.last_sync = Some(now());
                addon.items = items;
                addon.held = held;
                addon.error = None;
                log(id, &format!("synced in {:.1} s: {items} items in rex, {held} held", started.elapsed().as_secs_f64()));
            }
            Err(error) => {
                warn(id, &format!("sync failed: {error}"));
                addon.error = Some(error);
                addon.force |= force;
            }
        }
        save_state(self, addon);
        if std::mem::take(&mut addon.again) {
            drop(addons);
            self.start_sync(id, false);
        }
    }

    fn run_sync(&self, id: &str, force: bool) -> Result<(usize, usize), String> {
        let (manifest, source, config, storage, deleted) = {
            let addons = self.addons.lock().unwrap();
            let a = addons.get(id).ok_or("removed")?;
            (a.manifest.clone(), a.source.clone(), a.config.clone(), a.storage.clone(), a.deleted.clone())
        };
        if let Some(label) = manifest.missing(&config) {
            return Err(format!("set {label} first"));
        }
        let env = Env { addon: id, source: &source, hosts: &manifest.hosts, storage };
        let returned = sandbox::call(&env, "items", &[json!({"config": manifest.effective(&config)})], sandbox::ITEMS)?;
        let mut items = checked_items(id, returned, &deleted)?;
        let name = source_name(id);
        let (status, answer) = self.feed_call(id, "PUT", &format!("/rex/sources/{name}"), &json!({"url": self.byte_base(id)}))?;
        if status != 200 {
            return Err(format!("rex refused the registration ({status}): {}", answer["error"]));
        }

        // Probes already taken go with their items; the rest are probed below.
        let mut todo = Vec::new();
        {
            let mut addons = self.addons.lock().unwrap();
            let addon = addons.get_mut(id).ok_or("removed")?;
            addon.paths = items.iter().flat_map(|i| [(path_of(i), id_of(i)), (id_of(i), id_of(i))]).collect();
            addon.probes.retain(|key, _| items.iter().any(|i| id_of(i) == *key));
            for (at, item) in items.iter_mut().enumerate() {
                if item.get("media_info").is_some_and(Json::is_object) {
                    continue;
                }
                match addon.probes.get(&id_of(item)) {
                    Some(probe) => {
                        attach(item, probe.clone());
                        addon.paths.insert(path_of(item), id_of(item));
                    }
                    None => todo.push(at),
                }
            }
            write_index(self, addon);
        }

        if !todo.is_empty() {
            match &self.ffprobe {
                None => warn(id, &format!("{} items are not probed (no --ffprobe): rex holds them and shows none of them", todo.len())),
                Some(ffprobe) => self.probe_all(id, ffprobe, &mut items, todo)?,
            }
        }

        let held = items.iter().filter(|i| !i.get("media_info").is_some_and(Json::is_object)).count();
        if items.is_empty() {
            return Ok((0, 0));
        }
        self.snapshot(id, &items, force)?;
        Ok((items.len() - held, held))
    }

    /// Probes items PROBE_WORKERS at a time through this host's own byte path, so the probe
    /// describes exactly what rex reads; each one reaches rex as soon as a batch is ready.
    fn probe_all(&self, id: &str, ffprobe: &str, items: &mut [Map<String, Json>], todo: Vec<usize>) -> Result<(), String> {
        let urls: Vec<(usize, String)> = todo
            .iter()
            .map(|&at| (at, format!("{}/{}", self.byte_base(id), encode_path(&path_of(&items[at])))))
            .collect();
        let queue = Mutex::new(VecDeque::from(urls));
        let (sender, results) = mpsc::channel();
        let mut batch = Vec::new();
        let mut failed = 0;
        std::thread::scope(|scope| -> Result<(), String> {
            for _ in 0..PROBE_WORKERS.min(todo.len()) {
                let (queue, sender) = (&queue, sender.clone());
                scope.spawn(move || {
                    while let Some((at, url)) = { let next = queue.lock().unwrap().pop_front(); next } {
                        let _ = sender.send((at, probe(ffprobe, &url)));
                    }
                });
            }
            drop(sender);
            for (at, result) in results {
                let item = &mut items[at];
                match result {
                    Ok(probe) => {
                        attach(item, probe.clone());
                        if let Some(addon) = self.addons.lock().unwrap().get_mut(id) {
                            addon.probes.insert(id_of(item), probe);
                            addon.paths.insert(path_of(item), id_of(item));
                        }
                        batch.push(Json::Object(item.clone()));
                    }
                    Err(error) => {
                        failed += 1;
                        warn(id, &format!("{}: probe failed: {error}", id_of(item)));
                    }
                }
                if batch.len() >= READY_BATCH {
                    self.push(id, std::mem::take(&mut batch))?;
                }
            }
            Ok(())
        })?;
        if !batch.is_empty() {
            self.push(id, batch)?;
        }
        if let Some(addon) = self.addons.lock().unwrap().get(id) {
            let _ = write_atomic(&dir(self, id).join("probes.json"), Json::Object(addon.probes.clone()).to_string().as_bytes());
            write_index(self, addon);
        }
        if failed > 0 {
            warn(id, &format!("{failed} of {} probes failed; rex holds those until a later sync probes them", todo.len()));
        }
        Ok(())
    }

    fn push(&self, id: &str, items: Vec<Json>) -> Result<(), String> {
        let (status, answer) = self.feed_call(id, "POST", &format!("/rex/feed/{}/items", source_name(id)), &json!({"items": items}))?;
        if status != 200 {
            return Err(format!("rex refused items ({status}): {}", answer["error"]));
        }
        log_rejected(id, &answer);
        Ok(())
    }

    /// The whole set, in chunks of one session. rex refuses a snapshot that would remove more
    /// than half of the source's items; that is the addon's error to show, never forced, unless
    /// the person changed its settings or asked for it (`force`).
    fn snapshot(&self, id: &str, items: &[Map<String, Json>], force: bool) -> Result<(), String> {
        let path = format!("/rex/feed/{}/snapshot", source_name(id));
        let session = format!("{}-{}", now(), std::process::id());
        let chunks: Vec<&[Map<String, Json>]> = items.chunks(CHUNK).collect();
        for (part, chunk) in chunks.iter().enumerate() {
            let complete = part + 1 == chunks.len();
            let mut body = json!({"session": session, "part": part, "complete": complete, "items": chunk});
            if complete && force {
                body["force"] = json!(true);
            }
            let (status, answer) = self.feed_call(id, "PUT", &path, &body)?;
            log_rejected(id, &answer);
            match status {
                200 => {}
                409 if answer.get("would_remove").is_some() => {
                    return Err(format!(
                        "rex kept everything: this sync would remove {} of the {} items it has from this addon, more than half. \
                         If that is right, sync with force",
                        answer["would_remove"], answer["of"]
                    ))
                }
                _ => return Err(format!("rex refused the snapshot ({status}): {}", answer["error"])),
            }
        }
        Ok(())
    }

    pub fn set_config(self: &Arc<Self>, id: &str, patch: &Json) -> Result<Json, String> {
        let mut addons = self.addons.lock().unwrap();
        let addon = addons.get_mut(id).ok_or("no such addon")?;
        addon.config = addon.manifest.apply(&addon.config, patch)?;
        write_atomic(&dir(self, id).join("config.json"), Json::Object(addon.config.clone()).to_string().as_bytes())
            .map_err(|e| e.to_string())?;
        addon.opened.clear();
        let masked = addon.manifest.masked(&addon.config);
        let enabled = addon.enabled;
        drop(addons);
        if enabled {
            // New settings can take more than half of the items away on purpose.
            self.start_sync(id, true);
        }
        Ok(Json::Object(masked))
    }

    pub fn set_enabled(self: &Arc<Self>, id: &str, enabled: bool) -> Result<(), String> {
        let mut addons = self.addons.lock().unwrap();
        let addon = addons.get_mut(id).ok_or("no such addon")?;
        addon.enabled = enabled;
        save_state(self, addon);
        drop(addons);
        if enabled {
            self.start_sync(id, false);
        }
        Ok(())
    }
}

fn id_of(item: &Map<String, Json>) -> String {
    item["id"].as_str().unwrap_or_default().to_string()
}

fn path_of(item: &Map<String, Json>) -> String {
    item["path"].as_str().unwrap_or_default().to_string()
}

fn log_rejected(id: &str, answer: &Json) {
    for rejected in answer["rejected"].as_array().into_iter().flatten() {
        warn(id, &format!("rex rejected {}: {}", rejected["id"], rejected["reason"]));
    }
}

/// What `items()` returned, kept to what rex can take: objects with an `id` and a relative
/// `path`, each id and path once, none a person deleted.
fn checked_items(id: &str, returned: Json, deleted: &BTreeSet<String>) -> Result<Vec<Map<String, Json>>, String> {
    let Json::Array(list) = returned else {
        return Err("items() must return a list".into());
    };
    let (mut ids, mut paths, mut kept) = (BTreeSet::new(), BTreeSet::new(), Vec::new());
    for entry in list {
        let Json::Object(item) = entry else {
            warn(id, "items(): an entry is not an object; left out");
            continue;
        };
        let (Some(item_id), Some(path)) = (item.get("id").and_then(Json::as_str), item.get("path").and_then(Json::as_str)) else {
            warn(id, "items(): an entry has no id or path; left out");
            continue;
        };
        if item_id.is_empty() || path.starts_with('/') || path.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
            warn(id, &format!("items(): {item_id:?} has the path {path:?}, which is not relative; left out"));
            continue;
        }
        if !ids.insert(item_id.to_string()) || !paths.insert(path.to_string()) {
            warn(id, &format!("items(): {item_id} repeats an id or a path; left out"));
            continue;
        }
        if !deleted.contains(item_id) {
            kept.push(item);
        }
    }
    Ok(kept)
}

/// ffprobe's container to the extension rex takes the file by.
fn extension(format_name: &str) -> Option<&'static str> {
    Some(match format_name {
        "hls" => "m3u8",
        "matroska,webm" => "mkv",
        "mov,mp4,m4a,3gp,3g2,mj2" => "mp4",
        "mpegts" => "ts",
        "avi" => "avi",
        "mpeg" => "mpg",
        "ogg" => "ogv",
        "flv" => "flv",
        "asf" => "asf",
        _ => return None,
    })
}

/// The probe goes with its item, and the item's path takes the extension of what was probed:
/// an addon cannot know whether a link is Matroska or MP4 before it is read.
fn attach(item: &mut Map<String, Json>, probe: Json) {
    if let Some(ext) = probe["format"]["format_name"].as_str().and_then(extension) {
        let path = path_of(item);
        let (folder, file) = path.rsplit_once('/').map_or(("", path.as_str()), |(d, f)| (d, f));
        let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
        let renamed = if folder.is_empty() { format!("{stem}.{ext}") } else { format!("{folder}/{stem}.{ext}") };
        item.insert("path".into(), json!(renamed));
    }
    item.insert("media_info".into(), probe);
}

pub fn encode_path(path: &str) -> String {
    const SEGMENT: &percent_encoding::AsciiSet =
        &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
    path.split('/').map(|s| percent_encoding::utf8_percent_encode(s, SEGMENT).to_string()).collect::<Vec<_>>().join("/")
}

fn probe(ffprobe: &str, url: &str) -> Result<Json, String> {
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-rw_timeout", "60000000", "-print_format", "json"])
        .args(["-show_format", "-show_streams", "-show_chapters", url])
        .output()
        .map_err(|e| format!("{ffprobe}: {e}"))?;
    if !out.status.success() {
        let error = String::from_utf8_lossy(&out.stderr);
        let error = error.trim();
        return Err(error[error.len().saturating_sub(300)..].to_string());
    }
    let mut probe: Json = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    if !probe["format"].is_object() || probe["streams"].as_array().is_none_or(Vec::is_empty) {
        return Err("ffprobe found no streams".into());
    }
    // It names this host's own URL, which means nothing to rex.
    probe["format"].as_object_mut().unwrap().remove("filename");
    Ok(probe)
}

fn load(path: &Path) -> Result<Addon, String> {
    let raw = read_json(&path.join("manifest.json")).ok_or("no manifest.json")?;
    let manifest = Manifest::parse(&raw)?;
    let source = std::fs::read_to_string(path.join("main.js")).map_err(|e| format!("main.js: {e}"))?;
    let state = read_json(&path.join("state.json")).unwrap_or(json!({"enabled": true}));
    let object = |name: &str| match read_json(&path.join(name)) {
        Some(Json::Object(map)) => map,
        _ => Map::new(),
    };
    let paths = object("index.json").into_iter().filter_map(|(k, v)| Some((k, v.as_str()?.to_string()))).collect();
    Ok(Addon {
        manifest: Arc::new(manifest),
        source: Arc::new(source),
        config: object("config.json"),
        storage: Arc::new(Storage::open(path.join("storage.json"))),
        enabled: state["enabled"].as_bool().unwrap_or(true),
        last_sync: state["last_sync"].as_u64(),
        items: state["items"].as_u64().unwrap_or(0) as usize,
        held: state["held"].as_u64().unwrap_or(0) as usize,
        error: state["error"].as_str().map(str::to_string),
        deleted: serde_json::from_value(state["deleted"].clone()).unwrap_or_default(),
        paths,
        probes: object("probes.json"),
        syncing: false,
        force: false,
        again: false,
        attempted: None,
        opened: HashMap::new(),
    })
}

fn save_state(app: &App, addon: &Addon) {
    let state = json!({
        "enabled": addon.enabled, "last_sync": addon.last_sync, "items": addon.items, "held": addon.held,
        "error": addon.error, "deleted": addon.deleted,
    });
    let _ = write_atomic(&dir(app, &addon.manifest.id).join("state.json"), state.to_string().as_bytes());
}

fn write_index(app: &App, addon: &Addon) {
    let _ = write_atomic(&dir(app, &addon.manifest.id).join("index.json"), json!(addon.paths).to_string().as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_checked() {
        let deleted = BTreeSet::from(["gone".to_string()]);
        let kept = checked_items(
            "t",
            json!([
                {"id": "a", "path": "movies/a.mp4"},
                {"id": "a", "path": "movies/a2.mp4"},
                {"id": "b", "path": "movies/a.mp4"},
                {"id": "c", "path": "/etc/passwd"},
                {"id": "d", "path": "movies/../x.mp4"},
                {"id": "gone", "path": "movies/gone.mp4"},
                {"path": "movies/e.mp4"},
                "f",
                {"id": "g", "path": "movies/g.mkv", "kind": "movie"}
            ]),
            &deleted,
        )
        .unwrap();
        assert_eq!(kept.iter().map(id_of).collect::<Vec<_>>(), ["a", "g"]);
        assert!(checked_items("t", json!({"id": "a"}), &deleted).is_err());
    }

    #[test]
    fn probe_sets_the_extension() {
        let mut item = json!({"id": "x", "path": "films/Some Film (1950).mp4"}).as_object().unwrap().clone();
        attach(&mut item, json!({"format": {"format_name": "matroska,webm"}, "streams": [{}]}));
        assert_eq!(item["path"], "films/Some Film (1950).mkv");
        let mut item = json!({"id": "y", "path": "live/channel"}).as_object().unwrap().clone();
        attach(&mut item, json!({"format": {"format_name": "hls"}, "streams": [{}]}));
        assert_eq!(item["path"], "live/channel.m3u8");
        assert_eq!(encode_path("films/Some Film (1950).mkv"), "films/Some%20Film%20%281950%29.mkv");
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    /// A rex that takes registrations and refuses every snapshot unforced as one that would remove
    /// more than half, recording each snapshot body it was sent.
    fn stub_rex() -> (String, Arc<Mutex<Vec<Json>>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        let seen: Arc<Mutex<Vec<Json>>> = Arc::default();
        let recorded = seen.clone();
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let body: Json = serde_json::from_str(&body).unwrap_or(Json::Null);
                let (status, answer) = if request.url().ends_with("/snapshot") {
                    recorded.lock().unwrap().push(body.clone());
                    if body["force"] == json!(true) {
                        (200, json!({"created": 0}))
                    } else {
                        (409, json!({"error": "the snapshot would remove more than half of this source's items", "would_remove": 3, "of": 4}))
                    }
                } else {
                    (200, json!({}))
                };
                let _ = request.respond(tiny_http::Response::from_string(answer.to_string()).with_status_code(status));
            }
        });
        (url, seen)
    }

    fn wait_idle(app: &App, id: &str) -> Json {
        for _ in 0..200 {
            let status = app.addons.lock().unwrap()[id].status();
            if status["state"] != "syncing" {
                return status;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("the sync never ended");
    }

    #[test]
    fn a_refused_snapshot_is_the_addons_error_until_forced() {
        let (rex, seen) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        // No ffprobe: items go to rex unprobed, which rex holds.
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let source = "export default { items() { return [{id: 'a', path: 'films/a.mp4'}]; }, open() {} };";
        app.install(&json!({"id": "demo", "name": "Demo", "version": "1.0.0"}), source).unwrap();

        assert!(app.start_sync("demo", false));
        let status = wait_idle(&app, "demo");
        assert_eq!(status["state"], "error");
        let error = status["error"].as_str().unwrap();
        assert!(error.contains("would remove 3 of the 4"), "{error}");
        assert_eq!(status["last_sync"], Json::Null);
        assert_eq!(seen.lock().unwrap()[0]["items"][0], json!({"id": "a", "path": "films/a.mp4"}));
        assert!(seen.lock().unwrap()[0].get("force").is_none());

        assert!(app.start_sync("demo", true));
        let status = wait_idle(&app, "demo");
        assert_eq!((status["state"].as_str(), status["items"].as_u64(), status["held"].as_u64()), (Some("idle"), Some(0), Some(1)));
        assert_eq!(seen.lock().unwrap()[1]["force"], json!(true));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn new_settings_during_a_sync_sync_again_forced() {
        let (rex, seen) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-again-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        // items() takes a moment, so the settings change lands while it runs.
        let source = "export default { items(ctx) { const end = Date.now() + 300; while (Date.now() < end); return [{id: 'a' + ctx.config.n, path: 'films/a.mp4'}]; }, open() {} };";
        let manifest = json!({"id": "slow", "name": "Slow", "version": "1.0.0", "config": [{"key": "n", "label": "N", "type": "number", "default": 1}]});
        app.install(&manifest, source).unwrap();
        assert!(app.start_sync("slow", false));
        std::thread::sleep(Duration::from_millis(100));
        app.set_config("slow", &json!({"n": 2})).unwrap();
        for _ in 0..200 {
            if seen.lock().unwrap().len() == 2 && wait_idle(&app, "slow")["state"] == "idle" {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!((seen[0]["items"][0]["id"].as_str(), seen[0].get("force")), (Some("a1"), None));
        assert_eq!((seen[1]["items"][0]["id"].as_str(), &seen[1]["force"]), (Some("a2"), &json!(true)));
        let _ = std::fs::remove_dir_all(&data);
    }
}
