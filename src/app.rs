//! The host's state and work: installed addons, their syncs to rex, and what they open.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value as Json};

use crate::addons::{newer, read_json, write_atomic, Manifest, MASK};
use crate::net;
use crate::probe::{self, Prober};
use crate::repos::{self, Repo};
use crate::sandbox::{self, Bridge, Env, Storage};
use crate::spotify::{self, Pairing, Spotify};

/// How long an `open()` answer is reused for the byte reads of one item.
const OPEN_TTL: Duration = Duration::from_secs(10 * 60);
/// How soon a sync that failed is tried again...
const RETRY: Duration = Duration::from_secs(5 * 60);
/// ...unless `items()` threw `RETRY_AFTER <seconds>`, which is kept within these.
const RETRY_AFTER: (u64, u64) = (60, 24 * 3600);
/// Items rex is sent per request, as feed-api.md asks.
const CHUNK: usize = 200;
/// What one sync's `items()` may return over all its pages: pages, titles, bytes of the ids and
/// paths the host keeps for them (index, checks), and time spent in the calls, waiting included.
/// Measured on zen (2026-11-12), 100,000 titles in pages of 5,000: rex peaked at 1.5 GB, this
/// host at 196 to 344 MB, most of it one page's JSON; the index alone is about 40 MB.
const MAX_PAGES: usize = 10_000;
const MAX_TITLES: usize = 200_000;
const MAX_KEPT: usize = 64 << 20;
const PAGES_TIME: Duration = Duration::from_secs(3600);
/// A sync asks for every title at least this often, so rex converges even after it lost what it had.
const FULL_EVERY: u64 = 24 * 3600;
/// How many of the last sync's left-out entries the status quotes; it counts them all.
const WARNINGS_SHOWN: usize = 5;
/// Probed items go to rex in batches this big, so they show before the whole sync is done.
const READY_BATCH: usize = 20;
const PROBE_WORKERS: usize = 4;
/// The longest an item whose probe keeps failing waits before it is probed again.
const PROBE_BACKOFF_MAX: u64 = 7 * 86400;
/// A repository is fetched again a day after its last fetch that worked...
const REPO_REFRESH: u64 = 24 * 3600;
/// ...and, while fetches fail, no more often than this.
const REPO_RETRY: u64 = 3600;
/// How often a pending sign-in is polled, at least and at most, whatever the addon asks...
const SIGN_IN_INTERVAL: (u64, u64) = (2, 60);
/// ...and how long its code may live.
const SIGN_IN_LIFETIME: u64 = 3600;
/// How long a Spotify pairing keeps this device discoverable on the local network.
const PAIRING_LIFETIME: u64 = 600;
/// Where a Spotify addon's credentials are kept, in its folder: the host's alone.
const SPOTIFY_CREDENTIALS: &str = "spotify-credentials.json";
/// Each sign-in started gets the next number: a poll whose sign-in was replaced or cancelled stops.
static SIGN_INS: AtomicU64 = AtomicU64::new(0);

/// Each addon's secret setting values: every line logged for it, its own console output
/// included, has them masked, as does the error its status keeps.
static SECRETS: Mutex<BTreeMap<String, Vec<String>>> = Mutex::new(BTreeMap::new());

fn remember_secrets(addon: &Addon) {
    let mut secrets = addon.manifest.secrets(&addon.config);
    // The code a person types to sign in is theirs to see, not the log's.
    secrets.extend(addon.pending.as_ref().map(|pending| pending.code.clone()).filter(|code| !code.is_empty()));
    mask_secrets(&addon.manifest.id, secrets);
}

/// Every line logged for the addon `id` from now on has these masked.
pub fn mask_secrets(id: &str, mut secrets: Vec<String>) {
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    SECRETS.lock().unwrap().insert(id.to_string(), secrets);
}

/// `text` with the secret settings of the addon `tag` masked.
pub fn masked(tag: &str, text: &str) -> String {
    let secrets = SECRETS.lock().unwrap();
    secrets.get(tag).into_iter().flatten().fold(text.to_string(), |text, secret| text.replace(secret.as_str(), MASK))
}

/// How many of each addon's last log lines `GET /api/addons/{id}/log` can show...
pub const LOG_LINES: usize = 300;
/// ...each cut to this many bytes: an addon can print megabytes in one line.
const LOG_LINE_BYTES: usize = 4096;
/// Each tag's last LOG_LINES lines as they were logged, with when (Unix seconds).
static RECENT: Mutex<BTreeMap<String, VecDeque<(u64, String)>>> = Mutex::new(BTreeMap::new());

/// The last `lines` lines logged about the addon `id`, oldest first.
pub fn recent(id: &str, lines: usize) -> Json {
    let recent = RECENT.lock().unwrap();
    let kept = recent.get(id).into_iter().flatten();
    let skip = kept.clone().count().saturating_sub(lines);
    kept.skip(skip).map(|(at, line)| json!({"at": at, "line": line})).collect()
}

/// Writes a log line, already masked, about `tag` (an addon, a repository or `host`): stdout, or
/// stderr for a warning or an error.
pub fn emit(tag: &str, line: &str, problem: bool) {
    #[cfg(test)]
    LOGGED.lock().unwrap().push(line.to_string());
    if tag != "host" {
        let mut cut = line.len().min(LOG_LINE_BYTES);
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut recent = RECENT.lock().unwrap();
        let kept = recent.entry(tag.to_string()).or_default();
        if kept.len() == LOG_LINES {
            kept.pop_front();
        }
        kept.push_back((now(), line[..cut].to_string()));
    }
    if problem {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

#[cfg(test)]
pub static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn log(tag: &str, message: &str) {
    emit(tag, &format!("[{tag}] {}", masked(tag, message)), false);
}

pub fn warn(tag: &str, message: &str) {
    emit(tag, &format!("WARN [{tag}] {}", masked(tag, message)), true);
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[derive(Clone)]
pub struct Opened {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Playlists the addon wrote, instead of a link.
    pub playlists: Option<Arc<probe::Playlists>>,
}

/// A sign-in waiting for the person to enter its code, or to pick this device in the Spotify
/// app. Never saved.
pub struct Pending {
    /// Empty while pairing with Spotify.
    pub code: String,
    pub url: String,
    /// The name this device shows in the Spotify app while pairing.
    pub device: Option<String>,
    /// Dropped with the pending sign-in, which ends the pairing.
    _pairing: Option<Pairing>,
    pub expires_at: u64,
    expires: Instant,
    interval: Duration,
    /// What the addon needs to finish it: it never leaves the host and is never logged.
    state: Json,
    number: u64,
}

pub struct Addon {
    pub manifest: Arc<Manifest>,
    pub source: Arc<String>,
    pub config: Map<String, Json>,
    pub storage: Arc<Storage>,
    pub enabled: bool,
    /// The repository it was installed from; None for a bundled one.
    pub repo: Option<String>,
    pub last_sync: Option<u64>,
    pub items: usize,
    pub held: usize,
    pub error: Option<String>,
    /// `would_remove` when rex refused the last sync's snapshot as removing more than half.
    pub error_kind: Option<String>,
    /// Entries the last sync left out (items() returned them unfit, or rex rejected them), and
    /// the first WARNINGS_SHOWN of their messages.
    pub warned: usize,
    pub warnings: Vec<String>,
    /// Ids a person deleted in a Plex app: never pushed again.
    pub deleted: BTreeSet<String>,
    /// Each pushed path (and each id) to its item id: what `/bytes` reads by.
    pub paths: HashMap<String, String>,
    /// Ids pushed without a probe, which rex holds.
    pub held_ids: BTreeSet<String>,
    /// The next sync asks for every title (`ctx.full`): new settings, a sign-in, a restore, or
    /// a sync that failed once `items()` had answered.
    pub full_due: bool,
    /// A sync's `items()` answered and its push hasn't ended: saved, so a crash means `full_due`.
    pub unfinished: bool,
    /// When the last sync that worked began (`ctx.last_sync`), and the last that sent every title.
    pub since: Option<u64>,
    pub last_full: u64,
    pub probes: Map<String, Json>,
    /// Items whose probe failed: `{"count", "at"}`, so they wait before the next try.
    pub probe_failures: Map<String, Json>,
    pub syncing: bool,
    /// The next sync may remove more than half of the addon's items.
    pub force: bool,
    /// Asked for while one was running (new settings): run again when it ends, for this reason.
    pub again: Option<&'static str>,
    pub attempted: Option<Instant>,
    /// How long after a failed sync the next one runs: RETRY, or what the addon asked for.
    pub retry: Duration,
    pub opened: HashMap<String, (Opened, Instant)>,
    /// The `open()` running for each item: callers for the same item wait for it and share it.
    pub opening: HashMap<String, Arc<OnceLock<Result<Opened, String>>>>,
    /// The account it is signed in with, for an addon whose manifest has `sign_in`.
    pub account: Option<String>,
    /// Why the last sign-in failed, or why the website signed it out.
    pub sign_in_error: Option<String>,
    pub pending: Option<Pending>,
}

impl Addon {
    pub fn status(&self) -> Json {
        let m = &self.manifest;
        json!({
            "id": m.id, "name": m.name, "version": m.version, "description": m.description,
            "enabled": self.enabled, "repo": self.repo, "hosts": m.hosts.listed, "granted_hosts": m.reach(&self.config).granted.iter().map(|(host, port)| net::shown(host, *port)).collect::<Vec<_>>(),
            "state": if self.syncing { "syncing" } else if self.error.is_some() { "error" } else { "idle" },
            "items": self.items, "held": self.held, "last_sync": self.last_sync, "error": self.error,
            "error_kind": self.error_kind,
            "warnings": {"count": self.warned, "first": self.warnings},
            "sign_in": m.sign_in.as_ref().map(|sign_in| json!({
                "label": sign_in.label, "kind": sign_in.kind, "account": self.account, "error": self.sign_in_error,
                "pending": self.pending.as_ref().map(|p| match &p.device {
                    Some(device) => json!({"device": device, "expires_at": p.expires_at}),
                    None => json!({"code": p.code, "url": p.url, "expires_at": p.expires_at}),
                }),
            })),
        })
    }

    /// Whether it may sync and play: it needs no account, or is signed in.
    fn signed_in(&self) -> bool {
        self.manifest.sign_in.is_none() || self.account.is_some()
    }
}

pub struct App {
    pub data: PathBuf,
    pub rex: String,
    pub listen: SocketAddr,
    pub prober: Arc<Prober>,
    /// The name this device pairs under in the Spotify app (`--device-name`).
    pub device_name: String,
    pub spotify: Arc<dyn Spotify>,
    pub addons: Mutex<BTreeMap<String, Addon>>,
    pub repos: Mutex<Repos>,
}

/// The repositories added, and every `--default-repo` ever added, so one a person removed
/// is never added again.
#[derive(Default)]
pub struct Repos {
    pub list: Vec<Repo>,
    pub defaults: BTreeSet<String>,
    /// When each repository's last fetch started (Unix seconds), worked or not.
    pub attempted: HashMap<String, u64>,
}

/// An installed addon as a repository's listing compares it: version, repository, hosts.
type Installed = (String, Option<String>, Vec<String>);

fn dir(app: &App, id: &str) -> PathBuf {
    app.data.join("addons").join(id)
}

pub fn source_name(id: &str) -> String {
    format!("addon-{id}")
}

impl App {
    #[cfg(test)]
    pub fn new(data: PathBuf, rex: String, listen: SocketAddr, ffprobe: Option<String>, bundled: Option<&Path>) -> Arc<App> {
        App::with_spotify(data, rex, listen, ffprobe, bundled, "rex".into(), Arc::new(spotify::Librespot::default()))
    }

    pub fn with_spotify(
        data: PathBuf, rex: String, listen: SocketAddr, ffprobe: Option<String>, bundled: Option<&Path>, device_name: String, spotify: Arc<dyn Spotify>,
    ) -> Arc<App> {
        std::fs::create_dir_all(data.join("addons")).expect("cannot create the data directory");
        let saved = read_json(&data.join("repos.json")).unwrap_or(Json::Null);
        let repos = Repos {
            list: saved["repos"].as_array().into_iter().flatten().filter_map(Repo::from_json).collect(),
            defaults: serde_json::from_value(saved["defaults"].clone()).unwrap_or_default(),
            attempted: HashMap::new(),
        };
        let app = Arc::new(App {
            prober: Arc::new(Prober::new(ffprobe, listen)),
            device_name,
            spotify,
            data,
            rex: rex.trim_end_matches('/').to_string(),
            listen,
            addons: Mutex::default(),
            repos: Mutex::new(repos),
        });
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
                match self.install(&raw, &source, None) {
                    Ok(id) => log("host", &format!("installed bundled {id} {}", manifest.version)),
                    Err(error) => warn("host", &format!("bundled {}: {error}", manifest.id)),
                }
            }
        }
    }

    /// `--dev`: each addon folder in `dir` is installed now, whatever its version, and installed
    /// again whenever its manifest.json or main.js changes on disk, then synced. Checked every second.
    pub fn watch_dev(self: &Arc<Self>, dir: PathBuf) {
        let app = self.clone();
        std::thread::spawn(move || {
            let mut seen: HashMap<PathBuf, (SystemTime, SystemTime)> = HashMap::new();
            let mut first = true;
            loop {
                for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let path = entry.path();
                    let modified = |name: &str| std::fs::metadata(path.join(name)).and_then(|m| m.modified()).ok();
                    let (Some(manifest), Some(main)) = (modified("manifest.json"), modified("main.js")) else { continue };
                    if seen.insert(path.clone(), (manifest, main)) == Some((manifest, main)) {
                        continue;
                    }
                    let source = std::fs::read_to_string(path.join("main.js")).map_err(|e| format!("main.js: {e}"));
                    let raw = read_json(&path.join("manifest.json")).ok_or_else(|| "manifest.json is not JSON".to_string());
                    match raw.and_then(|raw| app.install(&raw, &source?, None)) {
                        Ok(id) => {
                            log("host", &format!("dev: installed {id} from {}", path.display()));
                            // At the start rex may not answer yet: the schedule syncs it once it does.
                            if !first {
                                app.start_sync(&id, false, "install");
                            }
                        }
                        Err(error) => warn("host", &format!("dev {}: {error}", path.display())),
                    }
                }
                first = false;
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    /// Writes an addon's files and loads it; an installed one keeps its settings and storage.
    /// `repo` is where it came from; None (bundled) keeps what an installed one came from.
    pub fn install(&self, raw: &Json, source: &str, repo: Option<&str>) -> Result<String, String> {
        let manifest = Manifest::parse(raw)?;
        manifest.check_host()?;
        self.loads(&manifest, source)?;
        let path = dir(self, &manifest.id);
        std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        write_atomic(&path.join("main.js"), source.as_bytes()).map_err(|e| e.to_string())?;
        write_atomic(&path.join("manifest.json"), serde_json::to_string_pretty(raw).unwrap().as_bytes()).map_err(|e| e.to_string())?;
        let mut addons = self.addons.lock().unwrap();
        let id = manifest.id.clone();
        match addons.get_mut(&id) {
            Some(addon) => {
                // A value saved in a setting that only now becomes a host setting was never typed
                // as a host to reach: the person types it again.
                let was_host = |key: &str| addon.manifest.config.iter().any(|f| f.key == key && f.kind == "host");
                let new_hosts: Vec<String> = manifest.config.iter().filter(|f| f.kind == "host" && !was_host(&f.key)).map(|f| f.key.clone()).collect();
                if new_hosts.iter().filter(|key| addon.config.remove(*key).is_some()).count() > 0 {
                    write_atomic(&path.join("config.json"), Json::Object(addon.config.clone()).to_string().as_bytes()).map_err(|e| e.to_string())?;
                }
                addon.manifest = Arc::new(manifest);
                addon.source = Arc::new(source.to_string());
                addon.opened.clear();
                addon.opening.clear();
                addon.last_sync = None;
                clear_probe_failures(self, addon);
                if repo.is_some() {
                    addon.repo = repo.map(str::to_string);
                }
                remember_secrets(addon);
                save_state(self, addon);
            }
            None => {
                let mut addon = load(&path)?;
                addon.repo = repo.map(str::to_string);
                save_state(self, &addon);
                addons.insert(id.clone(), addon);
            }
        }
        drop(addons);
        self.set_removed(&id, false);
        Ok(id)
    }

    /// What installing checks of `main.js`: it loads, and exports `items` and `open`, `signIn` and
    /// `signInPoll` with a code sign-in, and each of `exports`. The names checked, or why not.
    pub fn loads(&self, manifest: &Manifest, source: &str) -> Result<Vec<String>, String> {
        let storage = Arc::new(Storage::open(PathBuf::from("/dev/null")));
        let env = Env { addon: &manifest.id, source, hosts: manifest.hosts.clone(), storage, prober: self.prober.clone(), spotify: None, calls: None };
        let mut needed = vec!["items".to_string(), "open".into()];
        if manifest.sign_in.as_ref().is_some_and(|s| !s.spotify()) {
            needed.extend(["signIn".to_string(), "signInPoll".into()]);
        }
        needed.extend(manifest.exports.iter().cloned());
        let names: Vec<Json> = needed.iter().map(|name| json!(name)).collect();
        sandbox::call(&env, "", &names, sandbox::OPEN).map_err(|error| format!("main.js: {error}"))?;
        Ok(needed)
    }

    /// Removes an addon and, through rex's two-step delete, its source and everything it pushed.
    pub fn remove(&self, id: &str) -> Result<Json, String> {
        // Out of the map first: a sync still running sees it gone before its next call to rex.
        if self.addons.lock().unwrap().remove(id).is_none() {
            return Err("no such addon".into());
        }
        self.spotify.forget(id);
        RECENT.lock().unwrap().remove(id);
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

    /// What an item plays from now: the addon's `open()`, reused for OPEN_TTL. Concurrent
    /// first reads of one item (a player opens several) share a single call, and its error.
    pub fn open(self: &Arc<Self>, id: &str, item: &str) -> Result<Opened, String> {
        let (slot, manifest, source, config, storage) = {
            let mut addons = self.addons.lock().unwrap();
            let addon = addons.get_mut(id).ok_or("no such addon")?;
            if !addon.signed_in() {
                return Err("sign in first".into());
            }
            if let Some((opened, at)) = addon.opened.get(item) {
                if at.elapsed() < OPEN_TTL {
                    return Ok(opened.clone());
                }
            }
            let slot = addon.opening.entry(item.to_string()).or_default().clone();
            (slot, addon.manifest.clone(), addon.source.clone(), addon.config.clone(), addon.storage.clone())
        };
        let result = slot.get_or_init(|| self.call_open(id, item, &manifest, &source, &config, storage)).clone();
        if let Some(addon) = self.addons.lock().unwrap().get_mut(id) {
            if addon.opening.get(item).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
                addon.opening.remove(item);
                match &result {
                    Ok(opened) => {
                        // Playlists can be megabytes: what expired goes.
                        addon.opened.retain(|_, (_, at)| at.elapsed() < OPEN_TTL);
                        addon.opened.insert(item.to_string(), (opened.clone(), Instant::now()));
                    }
                    Err(error) => {
                        if let Some(reason) = sign_in_required(addon, error) {
                            signed_out(self, addon, &reason);
                        }
                    }
                }
            }
        }
        // Answered to rex and logged: no secret setting in it.
        result.map_err(|error| masked(id, &error))
    }

    fn call_open(self: &Arc<Self>, id: &str, item: &str, manifest: &Manifest, source: &str, config: &Map<String, Json>, storage: Arc<Storage>) -> Result<Opened, String> {
        let env = self.env(id, manifest, config, source, storage, true);
        let answer = sandbox::call(&env, "open", &[json!(item), call_ctx(manifest, config)], sandbox::OPEN)?;
        opened_from(&answer, manifest, &env.hosts)
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
            addon.held_ids.remove(item);
            save_state(self, addon);
            write_index(self, addon);
        }
        log(id, &format!("{item} deleted in a Plex app: no longer pushed"));
    }

    /// Starts a sync on its own thread; one asked for while another runs follows it. `reason` is
    /// what `items()` gets as `ctx.reason`: `schedule`, `retry`, `manual`, `settings`, `install`
    /// or `sign_in`.
    pub fn start_sync(self: &Arc<Self>, id: &str, force: bool, reason: &'static str) -> bool {
        {
            let mut addons = self.addons.lock().unwrap();
            let Some(addon) = addons.get_mut(id) else { return false };
            addon.force |= force;
            if addon.syncing {
                addon.again = Some(reason);
                return false;
            }
            addon.syncing = true;
        }
        let (app, id) = (self.clone(), id.to_string());
        std::thread::spawn(move || app.sync(&id, reason));
        true
    }

    /// Every enabled addon whose refresh is due, or whose last sync failed RETRY ago, with why.
    pub fn due(&self) -> Vec<(String, &'static str)> {
        let addons = self.addons.lock().unwrap();
        addons
            .values()
            // One waiting to be signed in is synced by the sign-in.
            .filter(|a| a.enabled && !a.syncing && a.signed_in())
            .filter_map(|a| {
                let reason = match (a.error.is_some(), a.attempted, a.last_sync) {
                    (true, Some(at), _) => (at.elapsed() >= a.retry).then_some("retry"),
                    // A failure from before a restart is tried again at once.
                    (true, None, _) => Some("retry"),
                    // Installed, or updated, and not synced since.
                    (false, None, None) => Some("install"),
                    (false, _, Some(last)) => (now() >= last + a.manifest.refresh_minutes * 60).then_some("schedule"),
                    (false, Some(at), None) => (at.elapsed() >= RETRY).then_some("retry"),
                };
                Some((a.manifest.id.clone(), reason?))
            })
            .collect()
    }

    fn sync(self: &Arc<Self>, id: &str, reason: &'static str) {
        let (started, began) = (Instant::now(), now());
        // Force is for this attempt only: the retries of one that fails are not forced.
        let force = match self.addons.lock().unwrap().get_mut(id) {
            Some(addon) => {
                addon.error_kind = None;
                std::mem::take(&mut addon.force)
            }
            None => return,
        };
        let mut run = Run::default();
        let result = self.run_sync(id, force, reason, &mut run);
        let mut addons = self.addons.lock().unwrap();
        let Some(addon) = addons.get_mut(id) else { return };
        addon.syncing = false;
        addon.attempted = Some(Instant::now());
        match result {
            Ok((items, held, every)) => {
                addon.last_sync = Some(now());
                addon.since = Some(began);
                if every {
                    addon.last_full = began;
                }
                addon.items = items;
                addon.held = held;
                addon.error = None;
                let what = if every { "every title" } else { "changes only" };
                let pages = if run.pages > 1 { format!(", {} pages", run.pages) } else { String::new() };
                log(id, &format!("synced in {:.1} s ({what}{pages}): {items} items in rex, {held} held", started.elapsed().as_secs_f64()));
            }
            Err(error) => {
                // What reached rex is unknown once items() answered, and a full sync that failed is still owed.
                addon.full_due |= run.full || run.answered;
                warn(id, &format!("sync failed: {error}"));
                let (retry, shown) = retry_after(&error).unwrap_or((RETRY, error));
                if retry != RETRY {
                    log(id, &format!("the addon asked to be tried again in {} s", retry.as_secs()));
                }
                addon.retry = retry;
                // The log keeps the stack; the app shows the reason.
                addon.error = Some(first_line(&masked(id, &shown)));
            }
        }
        if run.answered {
            addon.unfinished = false;
            save_bookkeeping(self, addon);
        }
        save_state(self, addon);
        if let Some(reason) = addon.again.take() {
            drop(addons);
            self.start_sync(id, false, reason);
        }
    }

    /// One sync: `items()` page by page, each page probed and sent as it comes, every title in one
    /// snapshot or, for a partial answer, upserts and then removals. The items and held titles
    /// rex has afterwards, and whether every title was sent.
    fn run_sync(self: &Arc<Self>, id: &str, force: bool, reason: &str, run: &mut Run) -> Result<(usize, usize, bool), String> {
        let (manifest, source, config, storage, deleted, last_sync) = {
            let mut addons = self.addons.lock().unwrap();
            let a = addons.get_mut(id).ok_or("removed")?;
            if !a.signed_in() {
                a.error_kind = Some("sign_in".into());
                return Err("sign in first".into());
            }
            let interval = a.manifest.refresh_minutes * 60;
            // A held title whose probe is due again is probed only when items() sends it.
            let probes_due = self.prober.bin.is_some() && a.held_ids.iter().any(|h| !probe_waits(a.probe_failures.get(h), interval, now()));
            let due = std::mem::take(&mut a.full_due);
            run.full = force || due || a.last_sync.is_none() || now() >= a.last_full.saturating_add(FULL_EVERY) || probes_due;
            (a.manifest.clone(), a.source.clone(), a.config.clone(), a.storage.clone(), a.deleted.clone(), a.last_sync.and(a.since))
        };
        if let Some(label) = manifest.missing(&config).first() {
            return Err(format!("set {label} first"));
        }
        let env = self.env(id, &manifest, &config, &source, storage, true);
        let mut ctx = call_ctx(&manifest, &config);
        ctx["reason"] = json!(reason);
        ctx["full"] = json!(run.full);
        ctx["last_sync"] = json!(last_sync);
        let session = format!("{}-{}", now(), std::process::id());
        let (mut part, mut fresh, mut held, mut removed) = (0, HashMap::new(), BTreeSet::new(), Vec::new());
        let (full, mut seen) = (run.full, Seen::default());
        let paged = pages(&env, ctx, &manifest, full, &deleted, &mut seen, |page, last| {
            let Page { mut items, left_out, partial, removed: gone, .. } = page;
            if run.pages == 0 {
                if let Some(addon) = self.addons.lock().unwrap().get_mut(id) {
                    (addon.warned, addon.warnings) = (0, Vec::new());
                    addon.unfinished = true;
                    save_state(self, addon);
                }
                self.register(id, &manifest)?;
            }
            run.pages += 1;
            removed.extend(gone);
            let left_out = [left_out, self.prepare(id, &mut items, partial)?].concat();
            for why in &left_out {
                warn(id, why);
            }
            if let Some(addon) = self.addons.lock().unwrap().get_mut(id) {
                warned(addon, left_out);
            }
            if partial {
                return self.upsert(id, &items);
            }
            fresh.extend(index_of(&items));
            held.extend(items.iter().filter(|i| !probed(i)).map(id_of));
            self.snapshot(id, &session, &mut part, &items, last, force)
        });
        // Once items() answered, the addon may count what it returned as sent: if it isn't, the next sync is full.
        run.answered = seen.pages > 0;
        paged.map_err(|error| {
            let mut addons = self.addons.lock().unwrap();
            match addons.get_mut(id).and_then(|a| Some((sign_in_required(a, &error)?, a))) {
                Some((reason, addon)) => {
                    signed_out(self, addon, &reason);
                    addon.error_kind = Some("sign_in".into());
                    format!("sign in again: {reason}")
                }
                None => error,
            }
        })?;
        let every = seen.partial != Some(true);
        if !every {
            self.remove_titles(id, removed, &seen.ids)?;
        }
        let mut addons = self.addons.lock().unwrap();
        let addon = addons.get_mut(id).ok_or("removed")?;
        // An empty set changes nothing in rex, nor here.
        if every && part > 0 {
            addon.paths = fresh;
            addon.held_ids = held;
            addon.probes.retain(|key, _| seen.ids.contains(key));
            addon.probe_failures.retain(|key, _| seen.ids.contains(key));
        }
        let ids = addon.paths.iter().filter(|(path, item)| path == item).count();
        let held = addon.held_ids.iter().filter(|h| addon.paths.get(*h) == Some(h)).count();
        Ok((ids - held, held, every))
    }

    /// Registers the addon as rex's source `addon-<id>`, with the libraries its manifest declares.
    fn register(&self, id: &str, manifest: &Manifest) -> Result<(), String> {
        let mut registration = json!({"url": self.byte_base(id)});
        if let Some(directories) = manifest.raw.get("directories").filter(|d| d.is_array()) {
            registration["directories"] = directories.clone();
        }
        let (status, answer) = self.feed_call(id, "PUT", &format!("/rex/sources/{}", source_name(id)), &registration)?;
        if status != 200 {
            return Err(format!("rex refused the registration ({status}): {}", answer["error"]));
        }
        Ok(())
    }

    /// Gives each item its probe: its own, one this host made before, or one made now, and adds it
    /// to the index `/bytes` reads by. In a partial sync an item whose path another title has is
    /// left out, as rex would refuse it: why each was.
    fn prepare(&self, id: &str, items: &mut Vec<Map<String, Json>>, partial: bool) -> Result<Vec<String>, String> {
        let (mut todo, mut waiting, mut left_out) = (Vec::new(), 0, Vec::new());
        {
            let mut addons = self.addons.lock().unwrap();
            let addon = addons.get_mut(id).ok_or("removed")?;
            for item in items.iter_mut() {
                if let Some(probe) = item.get("media_info").filter(|m| m.is_object()).or_else(|| addon.probes.get(&id_of(item))).cloned() {
                    attach(item, probe);
                }
            }
            if partial {
                items.retain(|item| match addon.paths.get(&path_of(item)) {
                    Some(other) if *other != id_of(item) => {
                        left_out.push(format!("items(): {} has the path {:?}, which {other} has; left out", id_of(item), path_of(item)));
                        false
                    }
                    _ => true,
                });
            }
            // Added to, not replaced: rex reads the old paths until it takes the new ones.
            addon.paths.extend(index_of(items));
            let interval = addon.manifest.refresh_minutes * 60;
            for (at, item) in items.iter().enumerate().filter(|(_, item)| !probed(item)) {
                match probe_waits(addon.probe_failures.get(&id_of(item)), interval, now()) {
                    true => waiting += 1,
                    false => todo.push(at),
                }
            }
        }
        if waiting > 0 {
            log(id, &format!("{waiting} items whose probe failed wait before they are probed again; rex holds them"));
        }
        if !todo.is_empty() {
            match &self.prober.bin {
                None => warn(id, &format!("{} items are not probed (no --ffprobe): rex holds them and shows none of them", todo.len())),
                Some(ffprobe) => self.probe_all(id, ffprobe, items, todo)?,
            }
        }
        Ok(left_out)
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
                        let _ = sender.send((at, probe::run(ffprobe, &url, &[])));
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
                            addon.probe_failures.remove(&id_of(item));
                            addon.probes.insert(id_of(item), probe);
                            addon.paths.insert(path_of(item), id_of(item));
                        }
                        batch.push(Json::Object(item.clone()));
                    }
                    Err(error) => {
                        failed += 1;
                        warn(id, &format!("{}: probe failed: {error}", id_of(item)));
                        if let Some(addon) = self.addons.lock().unwrap().get_mut(id) {
                            let count = addon.probe_failures.get(&id_of(item)).and_then(|f| f["count"].as_u64()).unwrap_or(0) + 1;
                            addon.probe_failures.insert(id_of(item), json!({"count": count, "at": now()}));
                        }
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
        if failed > 0 {
            warn(id, &format!("{failed} of {} probes failed; rex holds those until a later sync probes them", todo.len()));
        }
        Ok(())
    }

    fn push(&self, id: &str, items: Vec<Json>) -> Result<Json, String> {
        let (status, answer) = self.feed_call(id, "POST", &format!("/rex/feed/{}/items", source_name(id)), &json!({"items": items}))?;
        if status != 200 {
            return Err(format!("rex refused items ({status}): {}", answer["error"]));
        }
        self.rejected(id, &answer);
        Ok(answer)
    }

    /// A partial sync's titles, upserted CHUNK at a time. Once rex has taken a chunk, the paths its
    /// titles had before leave the index; a title rex refused keeps its old ones.
    fn upsert(&self, id: &str, items: &[Map<String, Json>]) -> Result<(), String> {
        for chunk in items.chunks(CHUNK) {
            let answer = self.push(id, chunk.iter().cloned().map(Json::Object).collect())?;
            let refused: HashSet<&str> = answer["rejected"].as_array().into_iter().flatten().filter_map(|r| r["id"].as_str()).collect();
            let taken: Vec<&Map<String, Json>> = chunk.iter().filter(|i| !refused.contains(i["id"].as_str().unwrap_or_default())).collect();
            let ids: HashSet<String> = taken.iter().map(|i| id_of(i)).collect();
            let paths: HashSet<String> = taken.iter().map(|i| path_of(i)).collect();
            let mut addons = self.addons.lock().unwrap();
            let addon = addons.get_mut(id).ok_or("removed")?;
            addon.paths.retain(|path, item| !ids.contains(item) || path == item || paths.contains(path));
            for item in taken {
                match probed(item) {
                    true => addon.held_ids.remove(&id_of(item)),
                    false => addon.held_ids.insert(id_of(item)),
                };
            }
        }
        Ok(())
    }

    /// A partial sync's `removed`, once every page is in: rex removes them, and so does the index.
    /// An id the sync also sent is kept. Like rex's snapshot, it refuses to take more than half of
    /// the addon's titles; a forced sync asks for every title instead.
    fn remove_titles(&self, id: &str, removed: Vec<String>, sent: &HashSet<String>) -> Result<(), String> {
        let (kept, gone): (Vec<String>, Vec<String>) = removed.into_iter().partition(|r| sent.contains(r));
        let gone: BTreeSet<String> = gone.into_iter().collect();
        let kept: Vec<String> = kept.iter().map(|r| format!("items(): {r} is in items and in removed; kept")).collect();
        for why in &kept {
            warn(id, why);
        }
        let (known, total) = {
            let mut addons = self.addons.lock().unwrap();
            let addon = addons.get_mut(id).ok_or("removed")?;
            warned(addon, kept);
            let known = gone.iter().filter(|g| addon.paths.get(*g) == Some(g)).count();
            (known, addon.paths.iter().filter(|(path, item)| path == item).count())
        };
        if gone.is_empty() {
            return Ok(());
        }
        if known * 2 > total {
            if let Some(addon) = self.addons.lock().unwrap().get_mut(id) {
                addon.error_kind = Some("would_remove".into());
            }
            return Err(format!(
                "rex-addons removed nothing: items() removed {known} of the {total} titles the addon has, more than half. \
                 If that is right, sync with force"
            ));
        }
        let path = format!("/rex/feed/{}/items", source_name(id));
        for chunk in gone.iter().collect::<Vec<_>>().chunks(1000) {
            let (status, answer) = self.feed_call(id, "DELETE", &path, &json!({"ids": chunk}))?;
            if status != 200 {
                return Err(format!("rex refused the removal ({status}): {}", answer["error"]));
            }
        }
        let mut addons = self.addons.lock().unwrap();
        let addon = addons.get_mut(id).ok_or("removed")?;
        addon.paths.retain(|_, item| !gone.contains(item));
        for item in &gone {
            addon.held_ids.remove(item);
            addon.probes.remove(item);
            addon.probe_failures.remove(item);
        }
        Ok(())
    }

    /// Every title, CHUNK at a time, as the parts of one snapshot session that `complete` ends.
    /// rex refuses a snapshot that would remove more than half of the source's items; that is
    /// the addon's error to show, never forced, unless the person changed its settings or asked
    /// for it (`force`).
    fn snapshot(&self, id: &str, session: &str, part: &mut usize, items: &[Map<String, Json>], complete: bool, force: bool) -> Result<(), String> {
        let path = format!("/rex/feed/{}/snapshot", source_name(id));
        let mut chunks: Vec<&[Map<String, Json>]> = items.chunks(CHUNK).collect();
        // A last page with nothing on it still ends what earlier pages started.
        if chunks.is_empty() && complete && *part > 0 {
            chunks.push(&[]);
        }
        let count = chunks.len();
        for (n, chunk) in chunks.into_iter().enumerate() {
            let complete = complete && n + 1 == count;
            let mut body = json!({"session": session, "part": *part, "complete": complete, "items": chunk});
            if complete && force {
                body["force"] = json!(true);
            }
            *part += 1;
            let (status, answer) = self.feed_call(id, "PUT", &path, &body)?;
            self.rejected(id, &answer);
            match status {
                200 => {}
                409 if answer.get("would_remove").is_some() => {
                    if let Some(addon) = self.addons.lock().unwrap().get_mut(id) {
                        addon.error_kind = Some("would_remove".into());
                    }
                    return Err(format!(
                        "rex kept everything: this sync would remove {} of the {} items it has from this addon, more than half. \
                         If that is right, sync with force",
                        answer["would_remove"], answer["of"]
                    ));
                }
                _ => return Err(format!("rex refused the snapshot ({status}): {}", answer["error"])),
            }
        }
        Ok(())
    }

    /// Items rex refused: logged, and counted with the sync's warnings.
    fn rejected(&self, id: &str, answer: &Json) {
        let messages: Vec<String> = answer["rejected"].as_array().into_iter().flatten().map(|r| format!("rex rejected {}: {}", r["id"], r["reason"])).collect();
        for message in &messages {
            warn(id, message);
        }
        if let Some(addon) = self.addons.lock().unwrap().get_mut(id).filter(|_| !messages.is_empty()) {
            warned(addon, messages);
        }
    }

    pub fn set_config(self: &Arc<Self>, id: &str, patch: &Json) -> Result<Json, String> {
        let mut addons = self.addons.lock().unwrap();
        let addon = addons.get_mut(id).ok_or("no such addon")?;
        addon.config = addon.manifest.apply(&addon.config, patch)?;
        remember_secrets(addon);
        write_atomic(&dir(self, id).join("config.json"), Json::Object(addon.config.clone()).to_string().as_bytes())
            .map_err(|e| e.to_string())?;
        // Nothing opened with the old settings is served again, not even an open() still running.
        addon.opened.clear();
        addon.opening.clear();
        clear_probe_failures(self, addon);
        addon.full_due = true;
        save_state(self, addon);
        let masked = addon.manifest.masked(&addon.config);
        let enabled = addon.enabled;
        drop(addons);
        if enabled {
            // New settings can take more than half of the items away on purpose.
            self.start_sync(id, true, "settings");
        }
        Ok(Json::Object(masked))
    }

    /// Every title a person deleted in a Plex app is pushed again, from the next sync on.
    pub fn restore(self: &Arc<Self>, id: &str) -> Result<(), String> {
        let mut addons = self.addons.lock().unwrap();
        let addon = addons.get_mut(id).ok_or("no such addon")?;
        let restored = std::mem::take(&mut addon.deleted).len();
        // A delta would never bring them back: they are unchanged.
        addon.full_due = true;
        save_state(self, addon);
        let enabled = addon.enabled;
        drop(addons);
        log(id, &format!("{restored} deleted titles restored"));
        if enabled {
            self.start_sync(id, false, "manual");
        }
        Ok(())
    }

    /// `id` can be called again (signed in, or enabled): the addons that require it and whose
    /// last sync failed sync now, instead of waiting for their retry.
    fn wake_dependents(self: &Arc<Self>, id: &str) {
        let waiting: Vec<String> = {
            let addons = self.addons.lock().unwrap();
            addons
                .values()
                .filter(|a| a.enabled && a.error.is_some() && a.manifest.requires.iter().any(|r| r == id))
                .map(|a| a.manifest.id.clone())
                .collect()
        };
        for dependent in waiting {
            self.start_sync(&dependent, false, "retry");
        }
    }

    pub fn set_enabled(self: &Arc<Self>, id: &str, enabled: bool) -> Result<(), String> {
        let mut addons = self.addons.lock().unwrap();
        let addon = addons.get_mut(id).ok_or("no such addon")?;
        addon.enabled = enabled;
        if !enabled && addon.pending.take().is_some() {
            remember_secrets(addon);
        }
        save_state(self, addon);
        drop(addons);
        if enabled {
            self.start_sync(id, false, "manual");
            self.wake_dependents(id);
        }
        Ok(())
    }
}

/// Signing an addon in to its website with a code typed on another device.
impl App {
    /// The addon's `method(...args, ctx)`, with `open()`'s limits. Its error has secrets masked.
    fn call_addon(self: &Arc<Self>, id: &str, method: &str, args: &[Json]) -> Result<Json, String> {
        let (manifest, source, config, storage) = {
            let addons = self.addons.lock().unwrap();
            let a = addons.get(id).ok_or("no such addon")?;
            (a.manifest.clone(), a.source.clone(), a.config.clone(), a.storage.clone())
        };
        let env = self.env(id, &manifest, &config, &source, storage, true);
        let mut args = args.to_vec();
        args.push(call_ctx(&manifest, &config));
        sandbox::call(&env, method, &args, sandbox::OPEN).map_err(|error| masked(id, &error))
    }

    /// `POST /api/addons/{id}/sign-in`: the addon's `signIn()` starts one, which replaces any
    /// pending, and a thread of its own polls it.
    pub fn sign_in(self: &Arc<Self>, id: &str) -> Result<(), (u16, String)> {
        let spotify = match self.addons.lock().unwrap().get(id) {
            None => return Err((404, "no such addon".into())),
            Some(a) if !a.enabled && a.manifest.sign_in.is_some() => return Err((409, "the addon is disabled".into())),
            Some(a) => a.manifest.sign_in.as_ref().ok_or((400, "this addon has no sign-in".to_string()))?.spotify(),
        };
        if spotify {
            self.pair(id)
        } else {
            self.sign_in_with_code(id)
        }
    }

    fn sign_in_with_code(self: &Arc<Self>, id: &str) -> Result<(), (u16, String)> {
        let started = self.call_addon(id, "signIn", &[]).map_err(|error| first_line(&error));
        let pending = started.and_then(|answer| pending_of(&answer)).map_err(|error| {
            warn(id, &format!("sign-in failed to start: {error}"));
            (502, error)
        })?;
        let number = pending.number;
        let mut addons = self.addons.lock().unwrap();
        let addon = addons.get_mut(id).ok_or((404, "no such addon".to_string()))?;
        if !addon.enabled {
            return Err((409, "the addon is disabled".into()));
        }
        log(id, &format!("sign-in started at {}, for {} s", pending.url, pending.expires_at.saturating_sub(now())));
        addon.pending = Some(pending);
        addon.sign_in_error = None;
        remember_secrets(addon);
        save_state(self, addon);
        drop(addons);
        let (app, id) = (self.clone(), id.to_string());
        std::thread::spawn(move || app.poll_sign_in(&id, number));
        Ok(())
    }

    /// Polls the sign-in numbered `number` every interval until it is done, fails or expires,
    /// or is replaced or cancelled, or its addon removed or disabled.
    fn poll_sign_in(self: &Arc<Self>, id: &str, number: u64) {
        let current = |a: &Addon| a.pending.as_ref().is_some_and(|p| p.number == number);
        loop {
            let wait = match self.addons.lock().unwrap().get(id).and_then(|a| a.pending.as_ref()).filter(|p| p.number == number) {
                Some(p) => p.interval.min(p.expires.saturating_duration_since(Instant::now())),
                None => return,
            };
            std::thread::sleep(wait);
            let state = {
                let mut addons = self.addons.lock().unwrap();
                let Some(addon) = addons.get_mut(id).filter(|a| current(a)) else { return };
                let pending = addon.pending.as_ref().unwrap();
                if Instant::now() >= pending.expires {
                    end_sign_in(self, addon, Err("the code expired before it was entered".into()));
                    return;
                }
                pending.state.clone()
            };
            let polled = self.call_addon(id, "signInPoll", &[state]);
            let mut addons = self.addons.lock().unwrap();
            let Some(addon) = addons.get_mut(id).filter(|a| current(a)) else { return };
            let outcome = match polled {
                Ok(answer) if answer["done"] != json!(true) => continue,
                Ok(answer) => answer["account"]
                    .as_str()
                    .filter(|account| !account.trim().is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| "signInPoll() said done but returned no account".to_string()),
                Err(error) => Err(first_line(&error)),
            };
            let done = outcome.is_ok();
            end_sign_in(self, addon, outcome);
            drop(addons);
            if done {
                self.start_sync(id, false, "sign_in");
                self.wake_dependents(id);
            }
            return;
        }
    }

    /// `DELETE /api/addons/{id}/sign-in`: the account and any pending sign-in are forgotten, and
    /// the addon's `signOut()` forgets its tokens. Its titles, settings and storage stay.
    pub fn sign_out(self: &Arc<Self>, id: &str) -> Result<(), (u16, String)> {
        {
            let mut addons = self.addons.lock().unwrap();
            let addon = addons.get_mut(id).ok_or((404, "no such addon".to_string()))?;
            if addon.manifest.sign_in.is_none() {
                return Err((400, "this addon has no sign-in".into()));
            }
            forget_spotify(self, addon);
            addon.account = None;
            addon.pending = None;
            addon.sign_in_error = None;
            addon.opened.clear();
            remember_secrets(addon);
            save_state(self, addon);
        }
        match self.call_addon(id, "signOut", &[]) {
            Err(error) if error != "main.js exports no signOut()" => warn(id, &format!("signOut() failed: {error}")),
            _ => log(id, "signed out"),
        }
        Ok(())
    }
}

/// Signing in with Spotify, and the globals some addons get.
impl App {
    /// `POST /api/addons/{id}/sign-in` for a Spotify addon: this device is discoverable on the
    /// local network until the person picks it in the Spotify app, for PAIRING_LIFETIME.
    fn pair(self: &Arc<Self>, id: &str) -> Result<(), (u16, String)> {
        let number = SIGN_INS.fetch_add(1, Ordering::Relaxed);
        let (app, owner) = (self.clone(), id.to_string());
        let done = Box::new(move |outcome: Result<Json, String>| app.paired(&owner, number, outcome));
        // Held while pairing starts, so credentials that come at once find the sign-in pending.
        let mut addons = self.addons.lock().unwrap();
        let addon = addons.get_mut(id).ok_or((404, "no such addon".to_string()))?;
        if !addon.enabled {
            return Err((409, "the addon is disabled".into()));
        }
        let pairing = self.spotify.pair(&self.device_name, Duration::from_secs(PAIRING_LIFETIME), done).map_err(|error| {
            warn(id, &format!("pairing failed to start: {error}"));
            (502, error)
        })?;
        log(id, &format!("pairing: discoverable as {} in the Spotify app for {PAIRING_LIFETIME} s", self.device_name));
        addon.pending = Some(Pending {
            code: String::new(),
            url: String::new(),
            device: Some(self.device_name.clone()),
            _pairing: Some(pairing),
            expires_at: now() + PAIRING_LIFETIME,
            expires: Instant::now() + Duration::from_secs(PAIRING_LIFETIME),
            interval: Duration::ZERO,
            state: Json::Null,
            number,
        });
        addon.sign_in_error = None;
        remember_secrets(addon);
        save_state(self, addon);
        Ok(())
    }

    /// The Spotify app sent credentials, or the pairing numbered `number` ended without them.
    fn paired(self: &Arc<Self>, id: &str, number: u64, outcome: Result<Json, String>) {
        let current = |app: &App| app.addons.lock().unwrap().get(id).is_some_and(|a| a.pending.as_ref().is_some_and(|p| p.number == number));
        if !current(self) {
            return;
        }
        let signed = outcome.and_then(|credentials| {
            // Another account's session goes first.
            self.spotify.forget(id);
            let signed = self.spotify.connect(id, &credentials).map_err(|e| first_line(&e).trim_start_matches("SIGN_IN_REQUIRED").trim_start_matches([':', ' ']).to_string())?;
            write_private(&dir(self, id).join(SPOTIFY_CREDENTIALS), signed.credentials.to_string().as_bytes())
                .map_err(|e| format!("cannot keep the sign-in: {e}"))?;
            Ok(signed.account)
        });
        let mut addons = self.addons.lock().unwrap();
        let Some(addon) = addons.get_mut(id).filter(|a| a.pending.as_ref().is_some_and(|p| p.number == number)) else {
            // Cancelled while it signed in.
            drop(addons);
            self.spotify.forget(id);
            return;
        };
        let done = signed.is_ok();
        end_sign_in(self, addon, signed);
        drop(addons);
        if done {
            self.start_sync(id, false, "sign_in");
            self.wake_dependents(id);
        }
    }

    /// The `spotify` global's requests: the addon's session, connected from its credentials
    /// when it has none, which are kept again when Spotify renews them.
    fn spotify_call(&self, id: &str, method: &str, path: &str, body: Option<&Json>) -> Result<Json, String> {
        if !spotify::valid_path(path) {
            return Err(format!("spotify: {path:?} is not a path such as /playlist/v2/user/<user>/rootlist"));
        }
        let file = dir(self, id).join(SPOTIFY_CREDENTIALS);
        let credentials = read_json(&file).ok_or("SIGN_IN_REQUIRED: sign in to Spotify again")?;
        let signed = self.spotify.connect(id, &credentials)?;
        if signed.credentials != credentials {
            write_private(&file, signed.credentials.to_string().as_bytes()).map_err(|e| format!("cannot keep the sign-in: {e}"))?;
        }
        match method {
            "USER" => Ok(json!(signed.username)),
            _ => self.spotify.request(id, method, path, body),
        }
    }

    /// `addons.call(callee, function, ...args)` from the addon `caller`: the callee's exported
    /// function in a sandbox of its own, with its own hosts, settings, storage and sign-in and
    /// `open()`'s limits. One level: what it calls cannot call a third.
    pub fn call_export(self: &Arc<Self>, caller: &str, callee: &str, function: &str, args: Vec<Json>) -> Result<Json, String> {
        let (manifest, source, config, storage) = {
            let addons = self.addons.lock().unwrap();
            if !addons.get(caller).is_some_and(|a| a.manifest.requires.iter().any(|r| r == callee)) {
                return Err(format!("{callee} is not in this addon's requires"));
            }
            if let Some(reason) = unmet(&addons, callee) {
                let name = addons.get(callee).map_or(callee.to_string(), |a| a.manifest.name.clone());
                return Err(match reason {
                    "not installed" => format!("the addon {callee} is not installed: install it and sign in to it first"),
                    "disabled" => format!("{name} is disabled: enable it first"),
                    _ => format!("sign in to {name} first"),
                });
            }
            let a = &addons[callee];
            if !a.manifest.exports.iter().any(|e| e == function) {
                return Err(format!("{} exports no {function}()", a.manifest.name));
            }
            (a.manifest.clone(), a.source.clone(), a.config.clone(), a.storage.clone())
        };
        let env = self.env(callee, &manifest, &config, &source, storage, false);
        let mut args = args;
        args.push(call_ctx(&manifest, &config));
        sandbox::call(&env, function, &args, sandbox::OPEN).map_err(|error| {
            let mut addons = self.addons.lock().unwrap();
            match addons.get_mut(callee).and_then(|a| Some((sign_in_required(a, &error)?, a))) {
                // Never SIGN_IN_REQUIRED itself: that would sign the caller out.
                Some((reason, addon)) => {
                    signed_out(self, addon, &reason);
                    format!("sign in to {} again: {reason}", manifest.name)
                }
                None => format!("{}: {}", manifest.name, first_line(&masked(callee, &error))),
            }
        })
    }

    /// The sandbox an addon's call runs in, with the globals its manifest asks for. `calls`:
    /// whether `addons.call` reaches other addons (false inside a call from another one).
    pub fn env<'a>(self: &Arc<Self>, id: &'a str, manifest: &'a Manifest, config: &Map<String, Json>, source: &'a str, storage: Arc<Storage>, calls: bool) -> Env<'a> {
        let spotify = manifest.sign_in.as_ref().filter(|s| s.spotify()).map(|_| {
            let (app, id) = (self.clone(), id.to_string());
            Arc::new(move |method: &str, path: &str, body: Vec<Json>| app.spotify_call(&id, method, path, body.first())) as Bridge
        });
        let calls = (!manifest.requires.is_empty()).then(|| match calls {
            true => {
                let (app, caller) = (self.clone(), id.to_string());
                Arc::new(move |callee: &str, function: &str, args: Vec<Json>| app.call_export(&caller, callee, function, args)) as Bridge
            }
            false => Arc::new(|_: &str, _: &str, _: Vec<Json>| Err("addons.call: an addon called by another cannot call a third".to_string())) as Bridge,
        });
        Env { addon: id, source, hosts: manifest.reach(config), storage, prober: self.prober.clone(), spotify, calls }
    }
}

/// What `open()` returned, checked: playlists (`"host": "0.4"`, every link on `hosts`, what the
/// addon reaches), or a link on them and the headers it needs.
pub fn opened_from(answer: &Json, manifest: &Manifest, hosts: &net::Hosts) -> Result<Opened, String> {
    if !answer["playlist"].is_null() {
        // An older rex-addons answers it with "open() returned no url".
        if manifest.host.as_deref().is_none_or(|host| newer("0.4", host)) {
            return Err("open() returned playlists, which need \"host\": \"0.4\" in the manifest".into());
        }
        let playlists = probe::Playlists::parse(answer, hosts).map_err(|e| format!("open() returned playlists: {e}"))?;
        return Ok(Opened { url: String::new(), headers: Vec::new(), playlists: Some(Arc::new(playlists)) });
    }
    let url = answer["url"].as_str().ok_or("open() returned no url")?;
    let parsed = url::Url::parse(url).map_err(|e| format!("open() returned {url}: {e}"))?;
    let (host, port) = (parsed.host_str().unwrap_or(""), parsed.port_or_known_default().unwrap_or(0));
    if !matches!(parsed.scheme(), "http" | "https") || !hosts.allows(host, port) {
        return Err(format!("open() returned {url}, which is not on the addon's hosts{}", hosts.port_note(host, port)));
    }
    let headers = answer["headers"]
        .as_object()
        .map(|h| h.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    Ok(Opened { url: url.to_string(), headers, playlists: None })
}

/// Why the addon `id` cannot be called now: `not installed`, `disabled` or `sign in needed`.
fn unmet(addons: &BTreeMap<String, Addon>, id: &str) -> Option<&'static str> {
    match addons.get(id) {
        None => Some("not installed"),
        Some(a) if !a.enabled => Some("disabled"),
        Some(a) if !a.signed_in() => Some("sign in needed"),
        Some(_) => None,
    }
}

/// A Spotify addon's session and saved credentials go.
fn forget_spotify(app: &App, addon: &Addon) {
    if addon.manifest.sign_in.as_ref().is_some_and(|s| s.spotify()) {
        app.spotify.forget(&addon.manifest.id);
        let _ = std::fs::remove_file(dir(app, &addon.manifest.id).join(SPOTIFY_CREDENTIALS));
    }
}

/// Written whole, readable by this user only.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&tmp)?.write_all(bytes)?;
    std::fs::rename(&tmp, path)
}

/// What `signIn()` returned, as a pending sign-in: its interval and lifetime kept within bounds.
fn pending_of(answer: &Json) -> Result<Pending, String> {
    let code = answer["code"].as_str().filter(|code| !code.trim().is_empty()).ok_or("signIn() returned no code")?;
    let url = answer["url"].as_str().filter(|url| url::Url::parse(url).is_ok_and(|url| url.scheme() == "https"));
    let url = url.ok_or("signIn() returned no https url")?;
    let seconds = |key: &str| answer[key].as_f64().ok_or(format!("signIn() returned no {key} in seconds"));
    let lifetime = seconds("expires_in")?.clamp(0.0, SIGN_IN_LIFETIME as f64) as u64;
    let interval = seconds("interval")?.clamp(SIGN_IN_INTERVAL.0 as f64, SIGN_IN_INTERVAL.1 as f64) as u64;
    Ok(Pending {
        code: code.into(),
        url: url.into(),
        device: None,
        _pairing: None,
        expires_at: now() + lifetime,
        expires: Instant::now() + Duration::from_secs(lifetime),
        interval: Duration::from_secs(interval),
        state: answer["state"].clone(),
        number: SIGN_INS.fetch_add(1, Ordering::Relaxed),
    })
}

/// A pending sign-in ends: signed in with the account named, or failed for the reason given,
/// which leaves any account it had.
fn end_sign_in(app: &App, addon: &mut Addon, outcome: Result<String, String>) {
    let id = addon.manifest.id.clone();
    match outcome {
        Ok(account) => {
            log(&id, &format!("signed in as {account}"));
            addon.account = Some(account);
            addon.sign_in_error = None;
            // Maybe another account, whose library no delta describes.
            addon.full_due = true;
        }
        Err(error) => {
            warn(&id, &format!("sign-in failed: {error}"));
            addon.sign_in_error = Some(error);
        }
    }
    // The code stays masked in the lines above; from here on it means nothing.
    addon.pending = None;
    remember_secrets(addon);
    save_state(app, addon);
}

/// The website's reason, when an addon that signs in threw `SIGN_IN_REQUIRED...`.
fn sign_in_required(addon: &Addon, error: &str) -> Option<String> {
    let rest = error.strip_prefix("SIGN_IN_REQUIRED").filter(|_| addon.manifest.sign_in.is_some())?;
    let reason = first_line(rest).trim_start_matches([':', ' ']).trim().to_string();
    Some(if reason.is_empty() { "signed out by the website".into() } else { reason })
}

/// `RETRY_AFTER <seconds>[: why]` thrown from `items()`: how long to wait before the next retry,
/// within RETRY_AFTER, and what to show (`why`, or that it waits).
fn retry_after(error: &str) -> Option<(Duration, String)> {
    let rest = error.strip_prefix("RETRY_AFTER")?.trim_start();
    let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let seconds = rest[..digits].parse::<u64>().ok()?.clamp(RETRY_AFTER.0, RETRY_AFTER.1);
    let reason = first_line(&rest[digits..]).trim_start_matches([':', ' ']).trim().to_string();
    let shown = if reason.is_empty() { format!("the website asked to wait {seconds} s") } else { reason };
    Some((Duration::from_secs(seconds), shown))
}

/// The website ended the session: the addon signs in again before it syncs or plays. Its
/// titles stay in rex.
fn signed_out(app: &App, addon: &mut Addon, reason: &str) {
    warn(&addon.manifest.id, &format!("signed out by the website: {reason}"));
    forget_spotify(app, addon);
    addon.account = None;
    addon.sign_in_error = Some(reason.to_string());
    addon.opened.clear();
    save_state(app, addon);
}

/// What every call of an addon gets as `ctx`: its settings, this rex-addons' version and which
/// addon and version it is.
pub fn call_ctx(manifest: &Manifest, config: &Map<String, Json>) -> Json {
    json!({
        "config": manifest.effective(config), "host": env!("CARGO_PKG_VERSION"),
        "addon": {"id": manifest.id, "version": manifest.version},
    })
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

/// Addon repositories, and what is installed from them.
impl App {
    fn repo_dir(&self, id: &str) -> PathBuf {
        self.data.join("repos").join(id)
    }

    fn save_repos(&self, repos: &Repos) {
        let saved = json!({"repos": repos.list.iter().map(Repo::to_json).collect::<Vec<_>>(), "defaults": repos.defaults});
        if let Err(error) = write_atomic(&self.data.join("repos.json"), saved.to_string().as_bytes()) {
            warn("host", &format!("repos.json: {error}"));
        }
    }

    /// Fetches a repository into its folder and lists its addons. A fetch that fails leaves
    /// the folder as it was.
    fn fetch_repo(&self, url: &str) -> Result<Repo, String> {
        let id = repos::repo_id(url)?;
        let dest = self.repo_dir(&id);
        std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
        let (commit, written) = crate::git::fetch(url, &dest)?;
        let (addons, problems) = repos::discover(&dest);
        for problem in &problems {
            warn(&id, problem);
        }
        let (name, description) = repos::describe(&dest, url);
        log(&id, &format!("fetched {commit}: {} files, {} addons{}", written.files, addons.len(),
            if written.skipped > 0 { format!(", {} links and submodules left out", written.skipped) } else { String::new() }));
        let error = addons.is_empty().then(|| match problems.first() {
            Some(problem) => format!("no addons in it ({problem})"),
            None => "no addons in it: no folder holds a manifest.json and a main.js".to_string(),
        });
        Ok(Repo { id, url: url.trim().to_string(), name, description, commit: Some(commit), last_fetch: Some(now()), error, addons })
    }

    /// The installed addons: id to (version, repository, hosts).
    fn installed(&self) -> HashMap<String, Installed> {
        self.addons.lock().unwrap().values().map(|a| (a.manifest.id.clone(), (a.manifest.version.clone(), a.repo.clone(), a.manifest.hosts.listed.clone()))).collect()
    }

    /// A repository's addons as the API lists them: each with what is installed of it.
    fn repo_addons(repo: &Repo, installed: &HashMap<String, Installed>) -> Vec<Json> {
        repo.addons
            .iter()
            .map(|addon| {
                let here = installed.get(&addon.id);
                // An update is this repository's, or adopts a bundled one; one installed from
                // another repository is that one's to update.
                let ours = here.is_some_and(|(_, from, _)| from.as_deref().is_none_or(|from| from == repo.id));
                // What installing it would change, for the app to ask about first.
                json!({
                    "id": addon.id, "name": addon.name, "version": addon.version, "description": addon.description,
                    "hosts": addon.hosts,
                    "installed_version": here.map(|(version, _, _)| version),
                    "installed_hosts": here.map(|(_, _, hosts)| hosts),
                    "installed_repo": here.and_then(|(_, from, _)| from.as_ref()),
                    "compatible": addon.compatible(),
                    "update_available": ours && here.is_some_and(|(version, _, _)| newer(&addon.version, version)),
                })
            })
            .collect()
    }

    fn repo_json(repo: &Repo, installed: &HashMap<String, Installed>) -> Json {
        let mut value = repo.to_json();
        value["addons"] = json!(Self::repo_addons(repo, installed));
        value
    }

    pub fn list_repos(&self) -> Json {
        let installed = self.installed();
        json!(self.repos.lock().unwrap().list.iter().map(|r| Self::repo_json(r, &installed)).collect::<Vec<_>>())
    }

    pub fn repo_addons_json(&self, id: &str) -> Option<Json> {
        let installed = self.installed();
        let repos = self.repos.lock().unwrap();
        repos.list.iter().find(|r| r.id == id).map(|r| json!(Self::repo_addons(r, &installed)))
    }

    /// `POST /api/repos`: a new repository is fetched and must hold an addon; the same one
    /// again is refreshed.
    pub fn add_repo(&self, url: &str) -> Result<(u16, Json), (u16, String)> {
        let id = repos::repo_id(url).map_err(|e| (400, e))?;
        let known = self.repos.lock().unwrap().list.iter().find(|r| r.id == id).map(|r| r.url.clone());
        match known {
            Some(known) if known == url.trim() => return self.refresh_repo(&id).map(|repo| (200, repo)),
            Some(known) => return Err((409, format!("that repository is already added as {known}"))),
            None => {}
        }
        let repo = match self.fetch_repo(url) {
            Ok(repo) if repo.error.is_none() => repo,
            Ok(repo) => {
                let _ = std::fs::remove_dir_all(self.repo_dir(&id));
                return Err((400, repo.error.unwrap()));
            }
            Err(error) => return Err((400, error)),
        };
        let mut repos = self.repos.lock().unwrap();
        repos.list.retain(|r| r.id != id);
        repos.list.push(repo.clone());
        self.save_repos(&repos);
        drop(repos);
        Ok((201, Self::repo_json(&repo, &self.installed())))
    }

    /// Fetches a repository again. A failure is kept on it, beside what it had.
    pub fn refresh_repo(&self, id: &str) -> Result<Json, (u16, String)> {
        let url = self.repos.lock().unwrap().list.iter().find(|r| r.id == id).map(|r| r.url.clone()).ok_or((404, "no such repository".to_string()))?;
        let fetched = self.fetch_repo(&url);
        let mut repos = self.repos.lock().unwrap();
        let repo = repos.list.iter_mut().find(|r| r.id == id).ok_or((404, "no such repository".to_string()))?;
        match fetched {
            Ok(fetched) => *repo = fetched,
            Err(error) => {
                warn(id, &format!("fetch failed: {error}"));
                repo.error = Some(error);
            }
        }
        let repo = repo.clone();
        self.save_repos(&repos);
        drop(repos);
        Ok(Self::repo_json(&repo, &self.installed()))
    }

    /// Its addons stay installed, and get no more updates.
    pub fn remove_repo(&self, id: &str) -> Result<(), String> {
        let mut repos = self.repos.lock().unwrap();
        let before = repos.list.len();
        repos.list.retain(|r| r.id != id);
        if repos.list.len() == before {
            return Err("no such repository".into());
        }
        self.save_repos(&repos);
        drop(repos);
        let _ = std::fs::remove_dir_all(self.repo_dir(id));
        log(id, "repository removed");
        Ok(())
    }

    /// Each `--default-repo` not added before is added once, and fetched in the background;
    /// so is any repository never fetched yet.
    pub fn start_repos(self: &Arc<Self>, defaults: &[String]) {
        let mut repos = self.repos.lock().unwrap();
        for url in defaults {
            let Ok(id) = repos::repo_id(url) else {
                warn("host", &format!("--default-repo {url}: not a repository address"));
                continue;
            };
            if repos.defaults.insert(url.clone()) && !repos.list.iter().any(|r| r.id == id) {
                repos.list.push(Repo {
                    id, url: url.clone(), name: repos::name_from_url(url), description: String::new(),
                    commit: None, last_fetch: None, error: Some("not fetched yet".into()), addons: Vec::new(),
                });
            }
        }
        self.save_repos(&repos);
        let unfetched: Vec<String> = repos.list.iter().filter(|r| r.commit.is_none()).map(|r| r.id.clone()).collect();
        for id in &unfetched {
            repos.attempted.insert(id.clone(), now());
        }
        drop(repos);
        for id in unfetched {
            let app = self.clone();
            std::thread::spawn(move || {
                let _ = app.refresh_repo(&id);
            });
        }
    }

    /// Repositories whose last good fetch is REPO_REFRESH old, each claimed for one fetch: one
    /// that keeps failing comes back every REPO_RETRY. Only fetched; nothing is installed.
    pub fn due_repos(&self) -> Vec<String> {
        let mut repos = self.repos.lock().unwrap();
        let now = now();
        let due: Vec<String> = repos
            .list
            .iter()
            .filter(|r| now >= r.last_fetch.unwrap_or(0) + REPO_REFRESH)
            .filter(|r| repos.attempted.get(&r.id).is_none_or(|at| now >= at + REPO_RETRY))
            .map(|r| r.id.clone())
            .collect();
        for id in &due {
            repos.attempted.insert(id.clone(), now);
        }
        due
    }

    /// `POST /api/addons`: installs a repository's addon, or updates it, keeping its settings
    /// and storage. One installed from another repository is refused.
    pub fn install_from_repo(&self, repo_id: &str, addon_id: &str) -> Result<(u16, String), (u16, String)> {
        let dir = {
            let repos = self.repos.lock().unwrap();
            let repo = repos.list.iter().find(|r| r.id == repo_id).ok_or((404, "no such repository".to_string()))?;
            let addon = repo.addons.iter().find(|a| a.id == addon_id).ok_or((404, format!("{} holds no addon {addon_id}", repo.name)))?;
            self.repo_dir(repo_id).join(&addon.dir)
        };
        let raw = read_json(&dir.join("manifest.json")).ok_or((404, "its manifest.json is gone: refresh the repository".to_string()))?;
        let source = std::fs::read_to_string(dir.join("main.js")).map_err(|_| (404, "its main.js is gone: refresh the repository".to_string()))?;
        if raw["id"].as_str() != Some(addon_id) {
            return Err((409, "the addon changed: refresh the repository".into()));
        }
        let existing = self.addons.lock().unwrap().get(addon_id).map(|a| a.repo.clone());
        if let Some(Some(other)) = &existing {
            if other != repo_id {
                return Err((409, format!("{addon_id} is already installed from {other}: remove it first")));
            }
        }
        let id = self.install(&raw, &source, Some(repo_id)).map_err(|e| (400, e))?;
        log(&id, &format!("{} from {repo_id}", if existing.is_some() { "updated" } else { "installed" }));
        Ok((if existing.is_some() { 200 } else { 201 }, id))
    }

    /// An addon as the API shows it: its state, and whether its repository has a newer version.
    pub fn status(&self, id: &str) -> Option<Json> {
        let offers: HashMap<(String, String), repos::RepoAddon> = self
            .repos
            .lock()
            .unwrap()
            .list
            .iter()
            .flat_map(|r| r.addons.iter().map(move |a| ((r.id.clone(), a.id.clone()), a.clone())))
            .collect();
        let addons = self.addons.lock().unwrap();
        let addon = addons.get(id)?;
        let mut status = addon.status();
        status["requires"] = addon.manifest.requires.iter().map(|r| {
            let reason = unmet(&addons, r);
            json!({"id": r, "ok": reason.is_none(), "reason": reason})
        }).collect();
        let update = addon.repo.as_ref().and_then(|r| offers.get(&(r.clone(), id.to_string()))).filter(|a| newer(&a.version, &addon.manifest.version));
        status["update_available"] = json!(update.is_some());
        status["update_hosts"] = json!(update.map(|a| &a.hosts));
        // The host settings that version adds: each one more host the person may be asked for.
        let has = |key: &str| addon.manifest.config.iter().any(|f| f.key == key && f.kind == "host");
        status["update_host_settings"] = json!(update.map(|a| a.host_settings.iter().filter(|(key, _)| !has(key)).map(|(_, label)| label).collect::<Vec<_>>()));
        Some(status)
    }

    pub fn statuses(&self) -> Vec<Json> {
        let ids: Vec<String> = self.addons.lock().unwrap().keys().cloned().collect();
        ids.iter().filter_map(|id| self.status(id)).collect()
    }
}

fn id_of(item: &Map<String, Json>) -> String {
    item["id"].as_str().unwrap_or_default().to_string()
}

fn path_of(item: &Map<String, Json>) -> String {
    item["path"].as_str().unwrap_or_default().to_string()
}

/// What `/bytes` reads items by: each one's path, and its id.
fn index_of(items: &[Map<String, Json>]) -> HashMap<String, String> {
    items.iter().flat_map(|i| [(path_of(i), id_of(i)), (id_of(i), id_of(i))]).collect()
}

/// Whether an item carries its probe: one without is held by rex.
fn probed(item: &Map<String, Json>) -> bool {
    item.get("media_info").is_some_and(Json::is_object)
}

/// What one sync is: whether it asked for every title (`ctx.full`), whether `items()` answered at
/// all, and how many pages it took.
#[derive(Default)]
struct Run {
    full: bool,
    answered: bool,
    pages: usize,
}

/// One answer of `items()`, checked: the titles kept, why each entry left out was, and for a
/// partial answer the ids it removes; `next`, the cursor of the page after it.
pub struct Page {
    pub items: Vec<Map<String, Json>>,
    pub left_out: Vec<String>,
    pub partial: bool,
    pub removed: Vec<String>,
    pub next: Option<Json>,
}

/// What a sync's pages returned so far: each id and path once over all of them, the bytes the
/// host keeps for them, and whether the pages are partial.
#[derive(Default)]
pub struct Seen {
    pub ids: HashSet<String>,
    paths: HashSet<String>,
    kept: usize,
    pub pages: usize,
    pub partial: Option<bool>,
}

/// Calls `items()` with `ctx` and, while an answer has a `next`, again with `ctx.cursor` set to
/// it, each call with its own limits; each page goes to `each` as soon as it is checked, with
/// whether it is the last. `seen` counts the pages that answered, also when one fails. The pages together are bounded by MAX_PAGES, PAGES_TIME, MAX_TITLES
/// and MAX_KEPT.
pub fn pages(env: &Env, mut ctx: Json, manifest: &Manifest, full: bool, deleted: &BTreeSet<String>, seen: &mut Seen, mut each: impl FnMut(Page, bool) -> Result<(), String>) -> Result<(), String> {
    let mut spent = Duration::ZERO;
    loop {
        if seen.pages == MAX_PAGES {
            return Err(format!("items() returned {MAX_PAGES} pages and still a next: at most {MAX_PAGES} a sync"));
        }
        if spent >= PAGES_TIME {
            return Err(format!("items() ran for longer than {} s over all its pages", PAGES_TIME.as_secs()));
        }
        let started = Instant::now();
        let returned = sandbox::call(env, "items", std::slice::from_ref(&ctx), sandbox::ITEMS)?;
        spent += started.elapsed();
        seen.pages += 1;
        let page = page_of(returned, manifest, full, seen, deleted)?;
        let next = page.next.clone();
        each(page, next.is_none())?;
        match next {
            Some(next) => ctx["cursor"] = next,
            None => return Ok(()),
        }
    }
}

/// One answer of `items()`: a list, or `{items, next, partial, removed}` (`"host": "0.8"`). Its
/// titles are kept to what rex can take: objects with an `id` and a relative `path`, each id and
/// path once over every page, none a person deleted.
pub fn page_of(returned: Json, manifest: &Manifest, full: bool, seen: &mut Seen, deleted: &BTreeSet<String>) -> Result<Page, String> {
    let (list, partial, removed, next) = match returned {
        Json::Array(list) => (list, false, Vec::new(), None),
        Json::Object(mut answer) => {
            // An older rex-addons answers it with "items() must return a list".
            if manifest.host.as_deref().is_none_or(|host| newer("0.8", host)) {
                return Err("items() returned {items: [...]}, which needs \"host\": \"0.8\" in the manifest".into());
            }
            let Some(Json::Array(list)) = answer.remove("items") else {
                return Err("items() must return a list, or {items: [...]}".into());
            };
            let partial = answer.get("partial") == Some(&json!(true));
            let removed = match answer.remove("removed") {
                None | Some(Json::Null) => Vec::new(),
                Some(_) if !partial => return Err("items(): removed needs partial: true".into()),
                Some(Json::Array(ids)) if ids.iter().all(|id| id.as_str().is_some_and(|id| !id.is_empty())) => {
                    ids.into_iter().filter_map(|id| id.as_str().map(str::to_string)).collect()
                }
                Some(_) => return Err("items(): removed must be a list of ids".into()),
            };
            (list, partial, removed, answer.remove("next").filter(|next| !next.is_null()))
        }
        _ => return Err("items() must return a list".into()),
    };
    if partial && full {
        return Err("items() returned partial: true, but this sync needs every title (ctx.full is true)".into());
    }
    if seen.partial.is_some_and(|before| before != partial) {
        return Err("items(): a sync's pages are all partial, or none is".into());
    }
    seen.partial = Some(partial);
    seen.kept += removed.iter().map(String::len).sum::<usize>();
    let (mut kept, mut left_out) = (Vec::new(), Vec::new());
    for entry in list {
        let why = match &entry {
            Json::Object(item) => match (item.get("id").and_then(Json::as_str), item.get("path").and_then(Json::as_str)) {
                (Some(item_id), Some(path)) if item_id.is_empty() || path.starts_with('/') || path.split('/').any(|s| s.is_empty() || s == "." || s == "..") => {
                    Some(format!("items(): {item_id:?} has the path {path:?}, which is not relative; left out"))
                }
                (Some(item_id), Some(path)) if !seen.ids.insert(item_id.to_string()) || !seen.paths.insert(path.to_string()) => {
                    Some(format!("items(): {item_id} repeats an id or a path; left out"))
                }
                (Some(item_id), Some(path)) => {
                    seen.kept += item_id.len() + path.len();
                    None
                }
                _ => Some("items(): an entry has no id or path; left out".to_string()),
            },
            _ => Some("items(): an entry is not an object; left out".to_string()),
        };
        match (why, entry) {
            (Some(why), _) => left_out.push(why),
            (None, Json::Object(item)) if !deleted.contains(item["id"].as_str().unwrap_or_default()) => kept.push(item),
            _ => {}
        }
    }
    if seen.ids.len() > MAX_TITLES {
        return Err(format!("items() returned more than {MAX_TITLES} titles: at most {MAX_TITLES} a sync"));
    }
    if seen.kept > MAX_KEPT {
        return Err(format!("items() returned more than {} MB of ids and paths: at most {} MB a sync", MAX_KEPT >> 20, MAX_KEPT >> 20));
    }
    Ok(Page { items: kept, left_out, partial, removed, next })
}

/// Adds to the addon's warnings: each counted, the first few kept, masked and cut short.
fn warned(addon: &mut Addon, messages: Vec<String>) {
    addon.warned += messages.len();
    let room = WARNINGS_SHOWN.saturating_sub(addon.warnings.len());
    let id = addon.manifest.id.clone();
    addon.warnings.extend(messages.into_iter().take(room).map(|message| format!("{:.300}", masked(&id, &message))));
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
/// an addon cannot know whether a link is Matroska or MP4 before it is read. An extension that is
/// one of ffprobe's names for the format stays: a track's `.m4a` is MP4 too, and rex takes an
/// `.mp4` for a video, never a track.
fn attach(item: &mut Map<String, Json>, probe: Json) {
    let format = probe["format"]["format_name"].as_str().unwrap_or_default();
    if let Some(ext) = extension(format) {
        let path = path_of(item);
        let (folder, file) = path.rsplit_once('/').map_or(("", path.as_str()), |(d, f)| (d, f));
        let (stem, own) = file.rsplit_once('.').unwrap_or((file, ""));
        let ext = if format.split(',').any(|name| name.eq_ignore_ascii_case(own)) { own } else { ext };
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
    let addon = Addon {
        manifest: Arc::new(manifest),
        source: Arc::new(source),
        config: object("config.json"),
        storage: Arc::new(Storage::open(path.join("storage.json"))),
        enabled: state["enabled"].as_bool().unwrap_or(true),
        repo: state["repo"].as_str().map(str::to_string),
        last_sync: state["last_sync"].as_u64(),
        items: state["items"].as_u64().unwrap_or(0) as usize,
        held: state["held"].as_u64().unwrap_or(0) as usize,
        error: state["error"].as_str().map(str::to_string),
        error_kind: state["error_kind"].as_str().map(str::to_string),
        warned: state["warnings"]["count"].as_u64().unwrap_or(0) as usize,
        warnings: serde_json::from_value(state["warnings"]["first"].clone()).unwrap_or_default(),
        deleted: serde_json::from_value(state["deleted"].clone()).unwrap_or_default(),
        paths,
        held_ids: read_json(&path.join("held.json")).and_then(|held| serde_json::from_value(held).ok()).unwrap_or_default(),
        // A state an older rex-addons wrote, or a push a crash cut short: the host can't apply a delta.
        full_due: state["full_due"].as_bool().unwrap_or(true) || state["unfinished"] == json!(true),
        unfinished: false,
        since: state["since"].as_u64(),
        last_full: state["last_full"].as_u64().unwrap_or(0),
        probes: object("probes.json"),
        probe_failures: object("probe_failures.json"),
        syncing: false,
        force: false,
        again: None,
        attempted: None,
        retry: RETRY,
        opened: HashMap::new(),
        opening: HashMap::new(),
        account: state["account"].as_str().map(str::to_string),
        sign_in_error: state["sign_in_error"].as_str().map(str::to_string),
        pending: None,
    };
    remember_secrets(&addon);
    // An error saved by an older host may still hold a secret.
    let error = addon.error.as_deref().map(|error| masked(&addon.manifest.id, error));
    Ok(Addon { error, ..addon })
}

fn save_state(app: &App, addon: &Addon) {
    let state = json!({
        "enabled": addon.enabled, "last_sync": addon.last_sync, "items": addon.items, "held": addon.held,
        "error": addon.error, "error_kind": addon.error_kind, "deleted": addon.deleted, "repo": addon.repo,
        "account": addon.account, "sign_in_error": addon.sign_in_error,
        "warnings": {"count": addon.warned, "first": addon.warnings},
        "full_due": addon.full_due, "unfinished": addon.unfinished, "since": addon.since, "last_full": addon.last_full,
    });
    let _ = write_atomic(&dir(app, &addon.manifest.id).join("state.json"), state.to_string().as_bytes());
}

/// An item whose probe failed `count` times waits a sync interval, doubled for each failure
/// after the first, up to PROBE_BACKOFF_MAX.
fn probe_waits(failure: Option<&Json>, interval: u64, now: u64) -> bool {
    let Some(failure) = failure else { return false };
    let count = failure["count"].as_u64().unwrap_or(1).max(1);
    let wait = interval.saturating_mul(1 << (count - 1).min(32)).min(PROBE_BACKOFF_MAX);
    now < failure["at"].as_u64().unwrap_or(0).saturating_add(wait)
}

/// New settings or a new version may well probe what failed before.
fn clear_probe_failures(app: &App, addon: &mut Addon) {
    if !addon.probe_failures.is_empty() {
        addon.probe_failures.clear();
        let _ = write_atomic(&dir(app, &addon.manifest.id).join("probe_failures.json"), b"{}");
    }
}

fn write_index(app: &App, addon: &Addon) {
    let _ = write_atomic(&dir(app, &addon.manifest.id).join("index.json"), &serde_json::to_vec(&addon.paths).unwrap());
}

/// What a sync changed of the host's own bookkeeping, written once when it ends.
fn save_bookkeeping(app: &App, addon: &Addon) {
    let dir = dir(app, &addon.manifest.id);
    write_index(app, addon);
    let _ = write_atomic(&dir.join("held.json"), &serde_json::to_vec(&addon.held_ids).unwrap());
    let _ = write_atomic(&dir.join("probes.json"), &serde_json::to_vec(&addon.probes).unwrap());
    let _ = write_atomic(&dir.join("probe_failures.json"), &serde_json::to_vec(&addon.probe_failures).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(host: &str) -> Manifest {
        Manifest::parse(&json!({"id": "a", "name": "A", "version": "1", "host": host})).unwrap()
    }

    #[test]
    fn items_are_checked() {
        let deleted = BTreeSet::from(["gone".to_string()]);
        let page = page_of(
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
            &manifest("0.6"),
            true,
            &mut Seen::default(),
            &deleted,
        )
        .unwrap();
        assert_eq!(page.items.iter().map(id_of).collect::<Vec<_>>(), ["a", "g"]);
        assert_eq!(page.left_out, [
            "items(): a repeats an id or a path; left out",
            "items(): b repeats an id or a path; left out",
            "items(): \"c\" has the path \"/etc/passwd\", which is not relative; left out",
            "items(): \"d\" has the path \"movies/../x.mp4\", which is not relative; left out",
            "items(): an entry has no id or path; left out",
            "items(): an entry is not an object; left out",
        ]);
        assert!(page_of(json!("a"), &manifest("0.8"), true, &mut Seen::default(), &deleted).is_err());
    }

    /// The object shape, paging and changes only need "host": "0.8"; each shape's rules, and the
    /// left-out rules over every page of one sync.
    #[test]
    fn pages_and_changes_are_checked() {
        let none = BTreeSet::new();
        let page = |returned: Json, host: &str, full: bool, seen: &mut Seen| page_of(returned, &manifest(host), full, seen, &none);
        let one = json!({"items": [{"id": "a", "path": "f/a.mp4"}], "next": 2});
        assert_eq!(page(one.clone(), "0.6", true, &mut Seen::default()).err().unwrap(), "items() returned {items: [...]}, which needs \"host\": \"0.8\" in the manifest");
        let mut seen = Seen::default();
        let first = page(one, "0.8", true, &mut seen).unwrap();
        assert_eq!((first.next, first.partial, first.items.len()), (Some(json!(2)), false, 1));
        // The same id or path on a later page is left out; next: null ends the sync.
        let last = page(json!({"items": [{"id": "a", "path": "f/b.mp4"}, {"id": "b", "path": "f/a.mp4"}, {"id": "c", "path": "f/c.mp4"}], "next": null}), "0.8", true, &mut seen).unwrap();
        assert_eq!((last.next, last.items.iter().map(id_of).collect::<Vec<_>>(), last.left_out.len()), (None, vec!["c".to_string()], 2));
        // partial: only where ctx.full is false; removed only with it; never mixed in one sync.
        let partial = json!({"partial": true, "items": [{"id": "d", "path": "f/d.mp4"}], "removed": ["x", "y"]});
        assert_eq!(page(partial.clone(), "0.8", true, &mut Seen::default()).err().unwrap(), "items() returned partial: true, but this sync needs every title (ctx.full is true)");
        let changes = page(partial.clone(), "0.8", false, &mut Seen::default()).unwrap();
        assert_eq!((changes.partial, changes.removed), (true, vec!["x".to_string(), "y".to_string()]));
        assert_eq!(page(json!({"items": [], "removed": ["x"]}), "0.8", false, &mut Seen::default()).err().unwrap(), "items(): removed needs partial: true");
        assert_eq!(page(json!({"partial": true, "items": [], "removed": [1]}), "0.8", false, &mut Seen::default()).err().unwrap(), "items(): removed must be a list of ids");
        let mut seen = Seen::default();
        page(partial, "0.8", false, &mut seen).unwrap();
        assert_eq!(page(json!([]), "0.8", false, &mut seen).err().unwrap(), "items(): a sync's pages are all partial, or none is");
        assert!(page(json!({"next": 1}), "0.8", true, &mut Seen::default()).is_err());
        // What one sync's pages may add up to.
        let mut full = Seen { kept: MAX_KEPT, ..Seen::default() };
        assert_eq!(page(json!([{"id": "a", "path": "a.mp4"}]), "0.8", true, &mut full).err().unwrap(), "items() returned more than 64 MB of ids and paths: at most 64 MB a sync");
        let mut many = Seen { ids: (0..MAX_TITLES).map(|n| n.to_string()).collect(), ..Seen::default() };
        assert_eq!(page(json!([{"id": "a", "path": "a.mp4"}]), "0.8", true, &mut many).err().unwrap(), "items() returned more than 200000 titles: at most 200000 a sync");
        let env = Env { addon: "a", source: "", hosts: Default::default(), storage: Arc::new(Storage::open("/dev/null".into())), prober: Arc::new(Prober::new(None, "127.0.0.1:9".parse().unwrap())), spotify: None, calls: None };
        let mut paged = Seen { pages: MAX_PAGES, ..Seen::default() };
        assert_eq!(pages(&env, json!({}), &manifest("0.8"), true, &none, &mut paged, |_, _| Ok(())).err().unwrap(), "items() returned 10000 pages and still a next: at most 10000 a sync");
    }

    #[test]
    fn probe_sets_the_extension() {
        let mut item = json!({"id": "x", "path": "films/Some Film (1950).mp4"}).as_object().unwrap().clone();
        attach(&mut item, json!({"format": {"format_name": "matroska,webm"}, "streams": [{}]}));
        assert_eq!(item["path"], "films/Some Film (1950).mkv");
        let mut item = json!({"id": "y", "path": "live/channel"}).as_object().unwrap().clone();
        attach(&mut item, json!({"format": {"format_name": "hls"}, "streams": [{}]}));
        assert_eq!(item["path"], "live/channel.m3u8");
        // A track's .m4a is one of the format's names: it stays, and rex takes it as audio.
        let mut item = json!({"id": "t", "path": "music/album/track.m4a"}).as_object().unwrap().clone();
        attach(&mut item, json!({"format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2"}, "streams": [{}]}));
        assert_eq!(item["path"], "music/album/track.m4a");
        let mut item = json!({"id": "w", "path": "films/clip.webm"}).as_object().unwrap().clone();
        attach(&mut item, json!({"format": {"format_name": "matroska,webm"}, "streams": [{}]}));
        assert_eq!(item["path"], "films/clip.webm");
        assert_eq!(encode_path("films/Some Film (1950).mkv"), "films/Some%20Film%20%281950%29.mkv");
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    /// A rex that takes registrations and refuses every snapshot unforced as one that would remove
    /// more than half, recording each snapshot body it was sent.
    fn stub_rex() -> (String, Arc<Mutex<Vec<Json>>>) {
        let (url, seen, _) = stub_rex_registrations();
        (url, seen)
    }

    type Bodies = Arc<Mutex<Vec<Json>>>;

    /// stub_rex, also recording each registration's body.
    fn stub_rex_registrations() -> (String, Bodies, Bodies) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        let seen: Arc<Mutex<Vec<Json>>> = Arc::default();
        let registered: Arc<Mutex<Vec<Json>>> = Arc::default();
        let (recorded, registrations) = (seen.clone(), registered.clone());
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let body: Json = serde_json::from_str(&body).unwrap_or(Json::Null);
                if request.url().starts_with("/rex/sources/") {
                    registrations.lock().unwrap().push(body.clone());
                }
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
        (url, seen, registered)
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
    fn an_addon_registers_the_libraries_its_manifest_declares() {
        let (rex, _, registered) = stub_rex_registrations();
        let data = std::env::temp_dir().join(format!("rex-addons-directories-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let source = "export default { items() { return [{id: 'a', path: 'clips/a.mp4'}]; }, open() {} };";
        let directories = json!([{"name": "clips", "shows": "Clips"}]);
        app.install(&json!({"id": "clips", "name": "Clips", "version": "1.0.0", "directories": directories}), source, None).unwrap();
        assert!(app.start_sync("clips", true, "manual"));
        wait_idle(&app, "clips");
        assert_eq!(registered.lock().unwrap()[0]["directories"], directories);
        assert!(Manifest::parse(&json!({"id": "x", "name": "X", "version": "1.0.0", "directories": {"name": "clips"}})).is_err());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_refused_snapshot_is_the_addons_error_until_forced() {
        let (rex, seen) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        // No ffprobe: items go to rex unprobed, which rex holds.
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let source = "export default { items() { return [{id: 'a', path: 'films/a.mp4'}]; }, open() {} };";
        app.install(&json!({"id": "demo", "name": "Demo", "version": "1.0.0"}), source, None).unwrap();

        assert!(app.start_sync("demo", false, "manual"));
        let status = wait_idle(&app, "demo");
        assert_eq!(status["state"], "error");
        let error = status["error"].as_str().unwrap();
        assert!(error.contains("would remove 3 of the 4"), "{error}");
        assert_eq!(status["last_sync"], Json::Null);
        assert_eq!(seen.lock().unwrap()[0]["items"][0], json!({"id": "a", "path": "films/a.mp4"}));
        assert!(seen.lock().unwrap()[0].get("force").is_none());

        assert!(app.start_sync("demo", true, "manual"));
        let status = wait_idle(&app, "demo");
        assert_eq!((status["state"].as_str(), status["items"].as_u64(), status["held"].as_u64()), (Some("idle"), Some(0), Some(1)));
        assert_eq!(seen.lock().unwrap()[1]["force"], json!(true));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn an_addons_secret_settings_never_reach_the_log_or_its_status() {
        let data = std::env::temp_dir().join(format!("rex-addons-leaky-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
        // Prints its key, its link and only the link's path, then throws with the key in it.
        let source = "export default {
            items(ctx) {
                const link = ctx.config.addons[0];
                console.log('using', ctx.config.token, link);
                console.warn('path', new URL(link).pathname);
                throw new Error('refused key ' + ctx.config.token);
            },
            open(item, ctx) { throw new Error('cannot open with ' + ctx.config.addons[0]); },
        };";
        let manifest = json!({"id": "leaky", "name": "Leaky", "version": "1.0.0", "hosts": ["*"], "config": [
            {"key": "token", "label": "Token", "type": "secret"},
            {"key": "addons", "label": "Addons", "type": "list", "secret": true}
        ]});
        app.install(&manifest, source, None).unwrap();
        let (key, link) = ("KEY-abc123", "https://addon.example/realdebrid=LINKKEY99/manifest.json");
        app.set_config("leaky", &json!({"token": key, "addons": [link]})).unwrap();
        let status = wait_idle(&app, "leaky");
        let error = status["error"].as_str().unwrap();
        assert!(error.contains("refused key ********"), "{error}");
        let opened = app.open("leaky", "a").err().unwrap();
        assert!(opened.contains("cannot open with ********"), "{opened}");
        warn("leaky", &format!("open(a): {opened}"));

        let logged: Vec<String> = LOGGED.lock().unwrap().iter().filter(|l| l.contains("leaky")).cloned().collect();
        assert!(logged.iter().any(|l| l.contains("[addon leaky] using ******** ********")), "{logged:?}");
        assert!(logged.iter().any(|l| l.contains("WARN [addon leaky] path ********")), "{logged:?}");
        assert!(logged.iter().any(|l| l.contains("sync failed")), "{logged:?}");
        for line in &logged {
            assert!(!line.contains("abc123") && !line.contains("LINKKEY99"), "{line}");
        }
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
        app.install(&manifest, source, None).unwrap();
        assert!(app.start_sync("slow", false, "manual"));
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

    /// Every call's ctx names this rex-addons and the addon; items() also gets why it runs.
    #[test]
    fn ctx_says_which_host_which_addon_and_why_it_syncs() {
        let (rex, _) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-ctx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let source = "export default {
            items(ctx) { storage.set('items', [...(storage.get('items') ?? []), ctx]); return []; },
            open(id, ctx) { storage.set('open', ctx); return { url: 'https://cdn.example.com/a.mp4' }; } };";
        let manifest = json!({"id": "ctx", "name": "Ctx", "version": "1.2.3", "hosts": ["cdn.example.com"], "config": [{"key": "n", "label": "N", "type": "number", "default": 1}]});
        app.install(&manifest, source, None).unwrap();
        // Installed and never synced: due, as an install.
        assert_eq!(app.due(), [("ctx".to_string(), "install")]);
        assert!(app.start_sync("ctx", false, "manual"));
        wait_idle(&app, "ctx");
        app.set_config("ctx", &json!({"n": 2})).unwrap();
        wait_idle(&app, "ctx");
        app.open("ctx", "a").unwrap();
        let stored = read_json(&dir(&app, "ctx").join("storage.json")).unwrap();
        let ctx = |config: i64, reason: &str, last: Json| json!({"config": {"n": config}, "host": env!("CARGO_PKG_VERSION"), "addon": {"id": "ctx", "version": "1.2.3"}, "reason": reason, "full": true, "last_sync": last});
        // The second sync is told when the first began; new settings ask for every title.
        let first = stored["items"][1]["last_sync"].as_u64().unwrap();
        assert!(first <= now() && first + 60 > now());
        assert_eq!(stored["items"], json!([ctx(1, "manual", Json::Null), ctx(2, "settings", json!(first))]));
        assert_eq!(stored["open"], json!({"config": {"n": 2}, "host": env!("CARGO_PKG_VERSION"), "addon": {"id": "ctx", "version": "1.2.3"}}));
        // A sync that failed is a retry once RETRY has passed; one that worked, a schedule once refresh_minutes have.
        let mut addons = app.addons.lock().unwrap();
        let addon = addons.get_mut("ctx").unwrap();
        addon.error = Some("down".into());
        addon.attempted = Instant::now().checked_sub(RETRY);
        drop(addons);
        assert_eq!(app.due(), [("ctx".to_string(), "retry")]);
        let mut addons = app.addons.lock().unwrap();
        let addon = addons.get_mut("ctx").unwrap();
        addon.error = None;
        addon.last_sync = Some(now() - addon.manifest.refresh_minutes * 60);
        drop(addons);
        assert_eq!(app.due(), [("ctx".to_string(), "schedule")]);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A sync that threw RETRY_AFTER waits that long, within bounds, before its retry; the reason
    /// after the number is what the app shows. Any other failure waits RETRY.
    #[test]
    fn retry_after_sets_when_a_failed_sync_is_tried_again() {
        assert_eq!(retry_after("RETRY_AFTER 3600: YouTube answered 429\n    at items (main.js:3:9)"), Some((Duration::from_secs(3600), "YouTube answered 429".into())));
        assert_eq!(retry_after("RETRY_AFTER 5"), Some((Duration::from_secs(60), "the website asked to wait 60 s".into())));
        assert_eq!(retry_after("RETRY_AFTER 9999999 too long"), Some((Duration::from_secs(86400), "too long".into())));
        assert_eq!(retry_after("RETRY_AFTER soon"), None);
        assert_eq!(retry_after("archive.org answered 503"), None);

        let (rex, _) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-retry-after-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let source = "export default { items(ctx) { throw new Error(ctx.config.error); }, open() {} };";
        let manifest = json!({"id": "busy", "name": "Busy", "version": "1.0.0", "config": [{"key": "error", "label": "E", "type": "text", "default": "RETRY_AFTER 600: rate limited"}]});
        app.install(&manifest, source, None).unwrap();
        assert!(app.start_sync("busy", false, "manual"));
        assert_eq!(wait_idle(&app, "busy")["error"], "rate limited");
        let attempted = |ago: Duration| app.addons.lock().unwrap().get_mut("busy").unwrap().attempted = Instant::now().checked_sub(ago);
        attempted(Duration::from_secs(599));
        assert!(app.due().is_empty());
        attempted(Duration::from_secs(600));
        assert_eq!(app.due(), [("busy".to_string(), "retry")]);
        // A plain failure goes back to RETRY.
        app.addons.lock().unwrap().get_mut("busy").unwrap().config.insert("error".into(), json!("down"));
        assert!(app.start_sync("busy", false, "manual"));
        assert_eq!(wait_idle(&app, "busy")["error"], "down");
        attempted(RETRY);
        assert_eq!(app.due(), [("busy".to_string(), "retry")]);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// The status counts the entries the last sync left out and quotes the first few, kept over
    /// a restart; the next sync's replace them.
    #[test]
    fn the_status_shows_what_the_last_sync_left_out() {
        let (rex, _) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-warnings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let source = "export default { items(ctx) {
            return [{id: 'a', path: 'films/a.mp4'}, ...Array.from({length: ctx.config.bad}, (_, n) => ({id: 'bad' + n, path: '/abs/' + n + '.mp4'})), 'nope']; }, open() {} };";
        let manifest = json!({"id": "warn", "name": "Warn", "version": "1.0.0", "config": [{"key": "bad", "label": "Bad", "type": "number", "default": 7}]});
        app.install(&manifest, source, None).unwrap();
        assert_eq!(app.status("warn").unwrap()["warnings"], json!({"count": 0, "first": []}));
        assert!(app.start_sync("warn", true, "manual"));
        let warnings = wait_idle(&app, "warn")["warnings"].clone();
        assert_eq!(warnings["count"], 8);
        assert_eq!(warnings["first"].as_array().unwrap().len(), WARNINGS_SHOWN);
        assert_eq!(warnings["first"][0], "items(): \"bad0\" has the path \"/abs/0.mp4\", which is not relative; left out");
        let again = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
        assert_eq!(again.status("warn").unwrap()["warnings"], warnings);
        app.addons.lock().unwrap().get_mut("warn").unwrap().config.insert("bad".into(), json!(0));
        assert!(app.start_sync("warn", true, "manual"));
        assert_eq!(wait_idle(&app, "warn")["warnings"], json!({"count": 1, "first": ["items(): an entry is not an object; left out"]}));
        let _ = std::fs::remove_dir_all(&data);
    }

    /// `--dev`: an addon is installed at once, and again, then synced, when its main.js or
    /// manifest.json changes, without a new version; one that fails to install keeps the last.
    #[test]
    fn dev_reinstalls_an_addon_whose_files_change() {
        let (rex, seen) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-dev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let folder = data.join("dev/films");
        std::fs::create_dir_all(&folder).unwrap();
        let write = |name: &str, text: &str| std::fs::write(folder.join(name), text).unwrap();
        let addon = |n: u32| format!("export default {{ items() {{ return [{{id: 'a', path: 'films/v{n}.mp4'}}]; }}, open() {{}} }};");
        write("manifest.json", r#"{"id": "films", "name": "Films", "version": "1.0.0"}"#);
        write("main.js", &addon(1));
        let app = App::new(data.join("data"), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        app.watch_dev(data.join("dev"));
        let wait = |until: &dyn Fn() -> bool| {
            for _ in 0..200 {
                if until() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            panic!("never happened");
        };
        wait(&|| app.addons.lock().unwrap().get("films").is_some_and(|a| a.source.contains("v1")));
        // Installed on start without a sync: the schedule syncs it once rex answers.
        assert!(seen.lock().unwrap().is_empty());
        write("main.js", &addon(2));
        wait(&|| seen.lock().unwrap().iter().any(|body| body["items"][0]["path"] == "films/v2.mp4"));
        write("main.js", "export default {");
        std::thread::sleep(Duration::from_millis(1500));
        assert!(app.addons.lock().unwrap()["films"].source.contains("v2"));
        assert!(LOGGED.lock().unwrap().iter().any(|line| line.starts_with("WARN [host] dev ") && line.contains("films: main.js:")), "{:?}", LOGGED.lock().unwrap());
        write("manifest.json", r#"{"id": "films", "name": "Films renamed", "version": "1.0.0"}"#);
        write("main.js", &addon(3));
        wait(&|| app.addons.lock().unwrap()["films"].manifest.name == "Films renamed" && app.addons.lock().unwrap()["films"].source.contains("v3"));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn concurrent_first_reads_share_one_open() {
        let data = std::env::temp_dir().join(format!("rex-addons-open-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
        // Each call counts itself in storage, then takes a while, as a scraper does.
        let source = "export default { items() { return []; }, open(id) {
            const n = (storage.get('calls') || 0) + 1; storage.set('calls', n);
            const end = Date.now() + 300; while (Date.now() < end);
            if (id === 'bad') throw new Error('no stream ' + n);
            return { url: 'https://cdn.example.com/' + n }; } };";
        app.install(&json!({"id": "slow", "name": "Slow", "version": "1.0.0", "hosts": ["cdn.example.com"]}), source, None).unwrap();
        for item in ["good", "bad"] {
            let answers: Vec<Result<String, String>> = std::thread::scope(|scope| {
                let readers: Vec<_> = (0..4)
                    .map(|n| {
                        let app = &app;
                        scope.spawn(move || {
                            std::thread::sleep(Duration::from_millis(20 * n));
                            app.open("slow", item).map(|o| o.url)
                        })
                    })
                    .collect();
                readers.into_iter().map(|r| r.join().unwrap()).collect()
            });
            let first = answers[0].clone();
            assert!(answers.iter().all(|a| *a == first), "{item}: {answers:?}");
        }
        // One call for the four good reads, one for the four bad ones.
        assert_eq!(read_json(&dir(&app, "slow").join("storage.json")).unwrap()["calls"], json!(2));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_failed_probe_waits_before_it_is_tried_again() {
        let (rex, _) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-probe-backoff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        // An ffprobe that counts its runs and always fails.
        let ffprobe = data.join("ffprobe");
        std::fs::write(&ffprobe, format!("#!/bin/sh\necho run >> '{}'\nexit 1\n", data.join("runs").display())).unwrap();
        std::fs::set_permissions(&ffprobe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let runs = || std::fs::read_to_string(data.join("runs")).unwrap_or_default().lines().count();
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), Some(ffprobe.display().to_string()), None);
        let source = "export default { items() { return [{id: 'a', path: 'films/a.mp4'}]; }, open() {} };";
        let manifest = json!({"id": "flaky", "name": "Flaky", "version": "1.0.0", "config": [{"key": "n", "label": "N", "type": "number", "default": 1}]});
        app.install(&manifest, source, None).unwrap();

        assert!(app.start_sync("flaky", true, "manual"));
        wait_idle(&app, "flaky");
        assert_eq!(runs(), 1);
        let saved = read_json(&dir(&app, "flaky").join("probe_failures.json")).unwrap();
        assert_eq!(saved["a"]["count"], 1);
        // The next sync, a moment later, leaves it held rather than probing it again.
        assert!(app.start_sync("flaky", true, "manual"));
        assert_eq!(wait_idle(&app, "flaky")["held"], 1);
        assert_eq!(runs(), 1);

        // New settings clear the wait.
        app.set_config("flaky", &json!({"n": 2})).unwrap();
        for _ in 0..200 {
            if runs() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        wait_idle(&app, "flaky");
        assert_eq!(runs(), 2);

        // The wait doubles per failure, up to a week.
        let failed = |count: u64| json!({"count": count, "at": 1000});
        assert!(probe_waits(Some(&failed(1)), 60, 1059) && !probe_waits(Some(&failed(1)), 60, 1060));
        assert!(probe_waits(Some(&failed(3)), 60, 1239) && !probe_waits(Some(&failed(3)), 60, 1240));
        assert!(!probe_waits(Some(&failed(90)), 21600, 1000 + PROBE_BACKOFF_MAX));
        assert!(!probe_waits(None, 60, 0));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_refused_snapshot_keeps_the_old_titles_readable() {
        let (rex, _) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-keep-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let source = "export default { items() { return [{id: 'a', path: 'films/a.mp4'}]; }, open() {} };";
        app.install(&json!({"id": "keep", "name": "Keep", "version": "1.0.0"}), source, None).unwrap();
        let old = HashMap::from([("films/old.mp4".to_string(), "old".to_string()), ("old".to_string(), "old".to_string())]);
        app.addons.lock().unwrap().get_mut("keep").unwrap().paths = old;

        // rex refuses and keeps "old": it must still be readable at /bytes.
        assert!(app.start_sync("keep", false, "manual"));
        assert_eq!(wait_idle(&app, "keep")["error_kind"], "would_remove");
        let paths = app.addons.lock().unwrap()["keep"].paths.clone();
        assert_eq!((paths.get("films/old.mp4"), paths.get("films/a.mp4")), (Some(&"old".to_string()), Some(&"a".to_string())));

        // Once rex takes it, "old" is gone, from the index on disk too.
        assert!(app.start_sync("keep", true, "manual"));
        assert_eq!(wait_idle(&app, "keep")["state"], "idle");
        assert!(!app.addons.lock().unwrap()["keep"].paths.contains_key("films/old.mp4"));
        assert!(read_json(&dir(&app, "keep").join("index.json")).unwrap().get("films/old.mp4").is_none());
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn force_is_for_one_attempt_and_the_guard_is_named() {
        let (rex, seen) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-force-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let source = "export default { items() { return [{id: 'a', path: 'films/a.mp4'}]; }, open() {} };";
        let manifest = json!({"id": "need", "name": "Need", "version": "1.0.0", "config": [{"key": "n", "label": "N", "type": "number", "required": true}]});
        app.install(&manifest, source, None).unwrap();

        // A forced sync that fails before rex is asked leaves nothing forced behind.
        assert!(app.start_sync("need", true, "manual"));
        let status = wait_idle(&app, "need");
        assert_eq!((status["error"].as_str(), &status["error_kind"]), (Some("set N first"), &Json::Null));
        assert!(!app.addons.lock().unwrap()["need"].force);

        // Its retry is unforced and meets the guard, which the status names.
        app.addons.lock().unwrap().get_mut("need").unwrap().config.insert("n".into(), json!(1));
        assert!(app.start_sync("need", false, "manual"));
        let status = wait_idle(&app, "need");
        assert_eq!(status["error_kind"], "would_remove");
        assert!(seen.lock().unwrap()[0].get("force").is_none());
        let saved = read_json(&dir(&app, "need").join("state.json")).unwrap();
        assert_eq!(saved["error_kind"], "would_remove");

        assert!(app.start_sync("need", true, "manual"));
        assert_eq!(wait_idle(&app, "need")["error_kind"], Json::Null);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A rex that keeps what it is sent as rex does: upserts, removals, snapshot sessions and the
    /// guard against removing more than half. Every feed request after `fail_after` answers 503.
    #[derive(Default)]
    struct FakeRex {
        items: BTreeMap<String, Json>,
        session: Option<(String, u64, BTreeSet<String>)>,
        requests: Vec<String>,
        fail_after: Option<usize>,
    }

    fn fake_rex() -> (String, Arc<Mutex<FakeRex>>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        let rex: Arc<Mutex<FakeRex>> = Arc::default();
        let state = rex.clone();
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let body: Json = serde_json::from_str(&body).unwrap_or(Json::Null);
                let mut rex = state.lock().unwrap();
                let (method, leaf) = (request.method().to_string(), request.url().rsplit('/').next().unwrap_or_default().to_string());
                let (status, answer) = if !request.url().starts_with("/rex/feed/") {
                    (200, json!({}))
                } else if rex.fail_after.is_some_and(|n| rex.requests.len() >= n) {
                    (503, json!({"error": "down"}))
                } else {
                    rex.requests.push(format!("{method} {leaf}"));
                    let upsert = |rex: &mut FakeRex, items: &Json| {
                        for item in items.as_array().unwrap() {
                            rex.items.insert(item["id"].as_str().unwrap().to_string(), item.clone());
                        }
                    };
                    match (method.as_str(), leaf.as_str()) {
                        ("POST", "items") => {
                            upsert(&mut rex, &body["items"]);
                            (200, json!({}))
                        }
                        ("DELETE", "items") => {
                            for id in body["ids"].as_array().unwrap() {
                                rex.items.remove(id.as_str().unwrap());
                            }
                            (200, json!({}))
                        }
                        _ => {
                            let (session, part) = (body["session"].as_str().unwrap().to_string(), body["part"].as_u64().unwrap());
                            if part == 0 {
                                rex.session = Some((session.clone(), 0, BTreeSet::new()));
                            }
                            if rex.session.as_ref().is_none_or(|(open, next, _)| *open != session || *next != part) {
                                (409, json!({"error": "no such snapshot session at this part"}))
                            } else {
                                upsert(&mut rex, &body["items"]);
                                let named: Vec<String> = body["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
                                let (_, next, seen) = rex.session.as_mut().unwrap();
                                *next += 1;
                                seen.extend(named);
                                if body["complete"] != json!(true) {
                                    (200, json!({}))
                                } else {
                                    let (_, _, seen) = rex.session.take().unwrap();
                                    let gone: Vec<String> = rex.items.keys().filter(|id| !seen.contains(*id)).cloned().collect();
                                    if gone.len() * 2 > rex.items.len() && body["force"] != json!(true) {
                                        (409, json!({"error": "would remove", "would_remove": gone.len(), "of": rex.items.len()}))
                                    } else {
                                        for id in gone {
                                            rex.items.remove(&id);
                                        }
                                        (200, json!({}))
                                    }
                                }
                            }
                        }
                    }
                };
                drop(rex);
                let _ = request.respond(tiny_http::Response::from_string(answer.to_string()).with_status_code(status));
            }
        });
        (url, rex)
    }

    /// An addon that answers what its `plan` setting says: `full` the pages of every title, `delta`
    /// the pages of changes; a page "throw" throws. It records each call's ctx in storage.
    const PLANNED: &str = "export default {
        items(ctx) {
            storage.set('calls', [...(storage.get('calls') ?? []), {full: ctx.full, cursor: ctx.cursor ?? 'none', last: ctx.last_sync}]);
            const plan = JSON.parse(ctx.config.plan);
            const pages = ctx.full || !plan.delta ? plan.full : plan.delta;
            const at = ctx.cursor ?? 0;
            if (pages[at] === 'throw') throw new Error('page ' + at + ' failed');
            return at + 1 < pages.length ? {...pages[at], next: at + 1} : pages[at];
        },
        open() { return { url: 'https://cdn.example.com/x' }; } };";

    fn titles(ids: std::ops::Range<usize>) -> Json {
        ids.map(|n| json!({"id": format!("t{n}"), "path": format!("films/t{n}.mp4"), "media_info": {"format": {"format_name": "matroska,webm"}, "streams": [{}]}})).collect()
    }

    fn planned(name: &str, rex: &str) -> (Arc<App>, PathBuf) {
        let data = std::env::temp_dir().join(format!("rex-addons-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex.into(), "127.0.0.1:9".parse().unwrap(), None, None);
        let manifest = json!({"id": "plan", "name": "Plan", "version": "1.0.0", "host": "0.8", "hosts": ["cdn.example.com"],
            "config": [{"key": "plan", "label": "Plan", "type": "text", "default": "{}"}]});
        app.install(&manifest, PLANNED, None).unwrap();
        (app, data)
    }

    fn plan(app: &App, plan: Json) {
        app.addons.lock().unwrap().get_mut("plan").unwrap().config.insert("plan".into(), json!(plan.to_string()));
    }

    fn synced(app: &Arc<App>) -> Json {
        assert!(app.start_sync("plan", false, "schedule"));
        wait_idle(app, "plan")
    }

    fn calls(app: &App) -> Vec<Json> {
        let calls = read_json(&dir(app, "plan").join("storage.json")).and_then(|s| s["calls"].as_array().cloned()).unwrap_or_default();
        app.addons.lock().unwrap()["plan"].storage.change("calls", None).unwrap();
        app.addons.lock().unwrap()["plan"].storage.flush();
        calls
    }

    /// Pages are one snapshot: every part of one session, the last one complete, so what no page
    /// named leaves rex only once every page is in; a page that throws fails the whole sync, rex
    /// removes nothing and the next sync asks for every title again.
    #[test]
    fn pages_reach_rex_as_one_snapshot() {
        let (url, rex) = fake_rex();
        let (app, data) = planned("pages", &url);
        plan(&app, json!({"full": [{"items": titles(0..300)}, {"items": titles(300..450)}, {"items": []}]}));
        let status = synced(&app);
        assert_eq!((status["state"].as_str(), status["items"].as_u64()), (Some("idle"), Some(450)));
        assert_eq!(rex.lock().unwrap().items.len(), 450);
        // 300 in two parts, 150 in one, and the empty last page's part that ends the session.
        assert_eq!(rex.lock().unwrap().requests, ["PUT snapshot"; 4]);
        assert_eq!(calls(&app).iter().map(|c| c["cursor"].clone()).collect::<Vec<_>>(), [json!("none"), json!(1), json!(2)]);

        // The second page throws: the first page's titles are upserted, nothing is removed.
        plan(&app, json!({"full": [{"items": titles(0..200)}, "throw"]}));
        app.addons.lock().unwrap().get_mut("plan").unwrap().full_due = true;
        let status = synced(&app);
        assert_eq!(status["error"], "page 1 failed");
        assert_eq!(rex.lock().unwrap().items.len(), 450);
        assert!(app.addons.lock().unwrap()["plan"].full_due);
        // Every title is readable at /bytes still.
        assert_eq!(app.addons.lock().unwrap()["plan"].paths.get("films/t449.mkv"), Some(&"t449".to_string()));

        // A page past the last title of a sync goes too: the same id or path on another page is left out.
        plan(&app, json!({"full": [{"items": titles(0..2)}, {"items": titles(1..3)}]}));
        let status = synced(&app);
        // 3 of 450 is a removal of more than half: forced, as the person would.
        assert_eq!((status["error_kind"].as_str(), status["warnings"]["count"].as_u64()), (Some("would_remove"), Some(1)));
        assert!(app.start_sync("plan", true, "manual"));
        assert_eq!(wait_idle(&app, "plan")["items"], 3);
        assert_eq!(rex.lock().unwrap().items.keys().cloned().collect::<Vec<_>>(), ["t0", "t1", "t2"]);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A partial answer upserts its titles and removes its `removed`, with no snapshot. The index
    /// follows a changed path, the counts and held titles stay right, also across a restart.
    #[test]
    fn changes_only_upsert_and_remove() {
        let (url, rex) = fake_rex();
        let (app, data) = planned("changes", &url);
        plan(&app, json!({"full": [{"items": titles(0..10)}]}));
        synced(&app);
        assert_eq!(calls(&app)[0]["full"], true);
        let moved = json!({"id": "t1", "path": "films/moved/t1.mp4", "media_info": {"format": {"format_name": "matroska,webm"}, "streams": [{}]}});
        let unprobed = json!({"id": "new", "path": "films/new.mp4"});
        plan(&app, json!({"full": [{"items": titles(0..10)}], "delta": [{"partial": true, "items": [moved], "removed": ["t2"]}, {"partial": true, "items": [unprobed], "removed": ["t3"]}]}));
        let status = synced(&app);
        let call = &calls(&app)[0];
        assert_eq!((&call["full"], call["last"].is_u64()), (&json!(false), true));
        assert_eq!((status["state"].as_str(), status["items"].as_u64(), status["held"].as_u64()), (Some("idle"), Some(8), Some(1)));
        let ids: Vec<String> = rex.lock().unwrap().items.keys().cloned().collect();
        assert_eq!(ids, ["new", "t0", "t1", "t4", "t5", "t6", "t7", "t8", "t9"]);
        let paths = app.addons.lock().unwrap()["plan"].paths.clone();
        assert_eq!((paths.get("films/moved/t1.mkv"), paths.get("films/t1.mkv"), paths.get("films/t2.mkv")), (Some(&"t1".to_string()), None, None));

        // Restarted, it knows the same, from disk, and a partial sync goes on from there.
        let again = App::new(data.clone(), url.clone(), "127.0.0.1:9".parse().unwrap(), None, None);
        assert_eq!(again.addons.lock().unwrap()["plan"].paths, paths);
        assert!(!again.addons.lock().unwrap()["plan"].full_due);
        plan(&again, json!({"full": [], "delta": [{"partial": true, "items": [], "removed": ["new"]}]}));
        let status = synced(&again);
        assert_eq!((status["items"].as_u64(), status["held"].as_u64()), (Some(8), Some(0)));
        assert!(!rex.lock().unwrap().items.contains_key("new"));
        // One snapshot, the first sync's; the partial ones only upsert and remove.
        assert_eq!(rex.lock().unwrap().requests, ["PUT snapshot", "POST items", "POST items", "DELETE items", "DELETE items"]);

        // A path another title has is left out, as rex would refuse it; an id both sent and
        // removed stays.
        plan(&again, json!({"full": [], "delta": [{"partial": true, "items": [{"id": "x", "path": "films/t0.mkv"}, {"id": "t4", "path": "films/t4.mkv"}], "removed": ["t4"]}]}));
        let status = synced(&again);
        assert_eq!(status["warnings"]["count"], 2);
        assert!(!rex.lock().unwrap().items.contains_key("x") && rex.lock().unwrap().items.contains_key("t4"));
        let _ = std::fs::remove_dir_all(&data);
    }

    /// `removed` can't take more than half of the titles: nothing is removed, the status names
    /// the guard, and the next sync asks for every title, which a forced sync gets through.
    #[test]
    fn removing_most_titles_through_removed_is_refused() {
        let (url, rex) = fake_rex();
        let (app, data) = planned("removed", &url);
        plan(&app, json!({"full": [{"items": titles(0..10)}]}));
        synced(&app);
        let gone: Vec<String> = (0..6).map(|n| format!("t{n}")).collect();
        plan(&app, json!({"full": [{"items": titles(6..10)}], "delta": [{"partial": true, "items": [], "removed": gone}]}));
        let status = synced(&app);
        assert_eq!(status["error_kind"], "would_remove");
        assert!(status["error"].as_str().unwrap().starts_with("rex-addons removed nothing: items() removed 6 of the 10 titles"), "{status}");
        assert_eq!(rex.lock().unwrap().items.len(), 10);
        assert_eq!(app.addons.lock().unwrap()["plan"].paths.len(), 20);
        calls(&app);
        assert!(app.start_sync("plan", true, "manual"));
        assert_eq!(wait_idle(&app, "plan")["items"], 4);
        assert_eq!(calls(&app)[0]["full"], true);
        assert_eq!(rex.lock().unwrap().items.len(), 4);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// rex stops answering halfway through a partial sync: it fails, and the next sync asks for
    /// every title, which brings rex back to exactly what the addon has.
    #[test]
    fn a_partial_sync_cut_short_is_followed_by_a_full_one() {
        let (url, rex) = fake_rex();
        let (app, data) = planned("cut", &url);
        plan(&app, json!({"full": [{"items": titles(0..10)}]}));
        synced(&app);
        let changed: Json = titles(0..400).as_array().unwrap().iter().map(|t| json!({"id": t["id"], "path": t["path"], "display": {"title": "new"}, "media_info": t["media_info"]})).collect();
        plan(&app, json!({"full": [{"items": changed}], "delta": [{"partial": true, "items": changed, "removed": ["t9"]}]}));
        let sent = rex.lock().unwrap().requests.len();
        rex.lock().unwrap().fail_after = Some(sent + 1);
        let status = synced(&app);
        assert!(status["error"].as_str().unwrap().contains("503"), "{status}");
        assert_eq!(rex.lock().unwrap().items.len(), 200);
        assert!(app.addons.lock().unwrap()["plan"].full_due);
        assert_eq!(read_json(&dir(&app, "plan").join("state.json")).unwrap()["full_due"], true);
        rex.lock().unwrap().fail_after = None;
        calls(&app);
        let status = synced(&app);
        assert_eq!((status["state"].as_str(), status["items"].as_u64()), (Some("idle"), Some(400)));
        assert_eq!(calls(&app)[0]["full"], true);
        assert!(rex.lock().unwrap().items.values().all(|i| i["display"]["title"] == "new"));
        let _ = std::fs::remove_dir_all(&data);
    }

    /// When the host asks for every title: first sync, new settings, a sign-in, a restore, a
    /// forced sync, a day after the last full one, a held title due for a probe, a state an older
    /// rex-addons wrote or a crash left unfinished; else ctx.full is false.
    #[test]
    fn a_sync_asks_for_every_title_when_a_delta_cannot_be_trusted() {
        let (url, _) = fake_rex();
        let (app, data) = planned("full", &url);
        plan(&app, json!({"full": [{"items": titles(0..2)}]}));
        let full = |app: &Arc<App>| {
            calls(app);
            synced(app);
            calls(app)[0]["full"].as_bool().unwrap()
        };
        assert!(full(&app));
        assert!(!full(&app));
        let mark = |app: &Arc<App>, f: &dyn Fn(&mut Addon)| f(app.addons.lock().unwrap().get_mut("plan").unwrap());
        mark(&app, &|a| a.last_full = now() - FULL_EVERY);
        assert!(full(&app));
        assert!(!full(&app));
        app.restore("plan").unwrap();
        wait_idle(&app, "plan");
        assert_eq!(calls(&app)[0]["full"], true);
        app.set_config("plan", &json!({"plan": json!({"full": [{"items": titles(0..2)}]}).to_string()})).unwrap();
        wait_idle(&app, "plan");
        assert_eq!(calls(&app)[0]["full"], true);
        assert!(app.start_sync("plan", true, "manual"));
        wait_idle(&app, "plan");
        assert_eq!(calls(&app)[0]["full"], true);
        assert!(!full(&app));
        // Partial where ctx.full is true fails, and the next one is still full.
        plan(&app, json!({"full": [{"partial": true, "items": []}]}));
        mark(&app, &|a| a.full_due = true);
        assert!(full(&app));
        assert_eq!(app.status("plan").unwrap()["error"], "items() returned partial: true, but this sync needs every title (ctx.full is true)");
        plan(&app, json!({"full": [{"items": titles(0..2)}]}));
        assert!(full(&app));
        assert!(!full(&app));
        // A partial answer the host can't take: the addon may count it as sent, so the next is full.
        plan(&app, json!({"full": [{"items": titles(0..2)}], "delta": [{"partial": true, "items": [], "removed": [1]}]}));
        assert!(!full(&app));
        assert!(full(&app));
        // A crash with a push unfinished, or a state from before 0.8: full after a restart.
        for state in [json!({"enabled": true, "last_sync": 1, "since": 1, "last_full": now(), "full_due": false, "unfinished": true}), json!({"enabled": true, "last_sync": 1})] {
            std::fs::write(dir(&app, "plan").join("state.json"), state.to_string()).unwrap();
            let again = App::new(data.clone(), url.clone(), "127.0.0.1:9".parse().unwrap(), None, None);
            assert!(again.addons.lock().unwrap()["plan"].full_due, "{state}");
        }
        // A held title whose probe is due again: only with an ffprobe.
        let held = |app: &Arc<App>| mark(app, &|a| {
            a.held_ids.insert("t0".into());
            a.probe_failures.insert("t0".into(), json!({"count": 1, "at": 0}));
        });
        held(&app);
        assert!(!full(&app));
        let probing = App::new(data.clone(), url.clone(), "127.0.0.1:9".parse().unwrap(), Some("/bin/false".into()), None);
        mark(&probing, &|a| a.full_due = false);
        held(&probing);
        assert!(full(&probing));
        let _ = std::fs::remove_dir_all(&data);
    }

    /// An addon that returns the object shape without "host": "0.8" is told so.
    #[test]
    fn the_object_shape_needs_host_0_8() {
        let (url, _) = fake_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-old-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), url, "127.0.0.1:9".parse().unwrap(), None, None);
        app.install(&json!({"id": "old", "name": "Old", "version": "1", "host": "0.6"}), "export default { items() { return {items: []}; }, open() {} };", None).unwrap();
        assert!(app.start_sync("old", false, "manual"));
        assert_eq!(wait_idle(&app, "old")["error"], "items() returned {items: [...]}, which needs \"host\": \"0.8\" in the manifest");
        let _ = std::fs::remove_dir_all(&data);
    }

    fn write_addon(root: &Path, id: &str, version: &str) {
        let dir = root.join("addons").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = json!({"id": id, "name": id, "version": version, "config": [{"key": "n", "label": "N", "type": "number", "default": 1}]});
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(dir.join("main.js"), format!("export default {{ items() {{ return []; }}, open() {{}}, version: '{version}' }};")).unwrap();
    }

    /// A repository as a fetch leaves it: its checkout in `repos/<id>/`, its record listed.
    fn add_local_repo(app: &App, id: &str) {
        let (addons, _) = repos::discover(&app.repo_dir(id));
        let mut repos = app.repos.lock().unwrap();
        repos.list.retain(|r| r.id != id);
        repos.list.push(Repo {
            id: id.into(), url: format!("https://example.com/{id}"), name: id.into(), description: String::new(),
            commit: Some("0".repeat(40)), last_fetch: Some(now()), error: None, addons,
        });
    }

    #[test]
    fn repository_installs_update_keep_settings_and_conflict() {
        let (rex, _) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-repos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        write_addon(&app.repo_dir("one"), "films", "1.0.0");
        write_addon(&app.repo_dir("two"), "films", "1.0.0");
        add_local_repo(&app, "one");
        add_local_repo(&app, "two");

        assert_eq!(app.install_from_repo("one", "films").unwrap(), (201, "films".to_string()));
        assert_eq!(app.status("films").unwrap()["repo"], "one");
        assert_eq!(app.status("films").unwrap()["update_available"], false);
        app.set_config("films", &json!({"n": 5})).unwrap();
        std::fs::write(dir(&app, "films").join("storage.json"), r#"{"kept": true}"#).unwrap();

        // The repository moves on: an update shows, and installing it keeps settings and storage.
        write_addon(&app.repo_dir("one"), "films", "1.1.0");
        add_local_repo(&app, "one");
        assert_eq!(app.status("films").unwrap()["update_available"], true);
        let listed = app.repo_addons_json("one").unwrap();
        assert_eq!((listed[0]["installed_version"].as_str(), &listed[0]["update_available"]), (Some("1.0.0"), &json!(true)));
        assert_eq!(app.install_from_repo("one", "films").unwrap(), (200, "films".to_string()));
        let addon = app.addons.lock().unwrap();
        assert_eq!((addon["films"].manifest.version.as_str(), &addon["films"].config["n"]), ("1.1.0", &json!(5)));
        assert!(addon["films"].source.contains("1.1.0"));
        drop(addon);
        assert_eq!(std::fs::read_to_string(dir(&app, "films").join("storage.json")).unwrap(), r#"{"kept": true}"#);
        assert_eq!(app.status("films").unwrap()["update_available"], false);

        // The same id from another repository is refused; the other repository's listing shows
        // it installed, with no update of its own to offer.
        let (status, error) = app.install_from_repo("two", "films").unwrap_err();
        assert_eq!(status, 409, "{error}");
        assert!(error.contains("already installed from one"), "{error}");
        assert_eq!(app.repo_addons_json("two").unwrap()[0]["update_available"], false);
        assert_eq!(app.install_from_repo("two", "nope").unwrap_err().0, 404);
        assert_eq!(app.install_from_repo("three", "films").unwrap_err().0, 404);

        // A bundled addon is adopted by the repository that offers it.
        write_addon(&app.repo_dir("two"), "solo", "2.0.0");
        add_local_repo(&app, "two");
        let solo = std::fs::read_to_string(app.repo_dir("two").join("addons/solo/main.js")).unwrap();
        app.install(&json!({"id": "solo", "name": "solo", "version": "1.0.0"}), &solo, None).unwrap();
        assert_eq!(app.status("solo").unwrap()["repo"], Json::Null);
        assert_eq!(app.repo_addons_json("two").unwrap()[1]["update_available"], true);
        assert_eq!(app.install_from_repo("two", "solo").unwrap().0, 200);
        assert_eq!(app.status("solo").unwrap()["repo"], "two");

        // Removing a repository leaves its addons installed, with no more updates.
        app.remove_repo("one").unwrap();
        assert!(!app.repo_dir("one").exists());
        assert_eq!(app.status("films").unwrap()["repo"], "one");
        assert_eq!(app.status("films").unwrap()["update_available"], false);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn updates_and_adoptions_show_what_they_change() {
        let (rex, _) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-consent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let offer = |repo: &str, version: &str, hosts: Json| {
            let dir = app.repo_dir(repo).join("addons/films");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("manifest.json"), json!({"id": "films", "name": "films", "version": version, "hosts": hosts}).to_string()).unwrap();
            std::fs::write(dir.join("main.js"), "export default { items() { return []; }, open() {} };").unwrap();
            add_local_repo(&app, repo);
        };
        // Bundled with one host; a repository offers it with every host.
        let main = "export default { items() { return []; }, open() {} };";
        app.install(&json!({"id": "films", "name": "films", "version": "1.0.0", "hosts": ["a.example"]}), main, None).unwrap();
        offer("one", "1.1.0", json!(["*"]));
        let listed = &app.repo_addons_json("one").unwrap()[0];
        assert_eq!((&listed["installed_hosts"], &listed["installed_repo"], &listed["hosts"]), (&json!(["a.example"]), &Json::Null, &json!(["*"])));
        let status = app.status("films").unwrap();
        assert_eq!((&status["hosts"], &status["update_hosts"]), (&json!(["a.example"]), &Json::Null));

        app.install_from_repo("one", "films").unwrap();
        offer("one", "1.2.0", json!(["*", "b.example"]));
        let status = app.status("films").unwrap();
        assert_eq!((&status["hosts"], &status["update_hosts"]), (&json!(["*"]), &json!(["*", "b.example"])));
        assert_eq!(app.list_repos()[0]["addons"][0]["installed_repo"], "one");
        // Another repository's listing names where it is installed from.
        offer("two", "9.0.0", json!([]));
        assert_eq!(app.repo_addons_json("two").unwrap()[0]["installed_repo"], "one");
        let nothing = app.repo_addons_json("two").unwrap();
        assert_eq!(nothing[0]["installed_hosts"], json!(["*"]));
        let _ = std::fs::remove_dir_all(&data);
    }

    /// An update shows the host settings it adds, for the app to say so before updating; a value
    /// saved in a setting that only becomes a host setting with it is dropped, never granted.
    #[test]
    fn updates_show_the_host_settings_they_add() {
        let (rex, _) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-host-consent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        let main = "export default { items() { return []; }, open() {} };";
        let manifest = |version: &str, config: Json| json!({"id": "home", "name": "Home", "version": version, "host": "0.7", "config": config});
        let offer = |version: &str, config: Json| {
            let dir = app.repo_dir("one").join("addons/home");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("manifest.json"), manifest(version, config).to_string()).unwrap();
            std::fs::write(dir.join("main.js"), main).unwrap();
            add_local_repo(&app, "one");
        };
        let text = |key: &str, label: &str| json!({"key": key, "label": label, "type": "text"});
        let host = |key: &str, label: &str| json!({"key": key, "label": label, "type": "host"});
        offer("1.0.0", json!([text("server", "Server"), host("nas", "NAS")]));
        app.install_from_repo("one", "home").unwrap();
        app.addons.lock().unwrap().get_mut("home").unwrap().enabled = false;
        app.set_config("home", &json!({"server": "10.0.0.2", "nas": "10.0.0.3:5005"})).unwrap();
        let status = app.status("home").unwrap();
        assert_eq!((&status["granted_hosts"], &status["update_host_settings"]), (&json!(["10.0.0.3:5005"]), &Json::Null));
        // 1.1 turns the text setting into a host setting and adds another; NAS was one already.
        offer("1.1.0", json!([host("server", "Server"), host("nas", "NAS"), host("backup", "Backup")]));
        assert_eq!(app.status("home").unwrap()["update_host_settings"], json!(["Server", "Backup"]));
        app.install_from_repo("one", "home").unwrap();
        let status = app.status("home").unwrap();
        assert_eq!((&status["granted_hosts"], &status["update_host_settings"]), (&json!(["10.0.0.3:5005"]), &Json::Null));
        assert_eq!(read_json(&dir(&app, "home").join("config.json")).unwrap(), json!({"nas": "10.0.0.3:5005"}));
        offer("1.2.0", json!([host("server", "Server"), host("nas", "NAS")]));
        assert_eq!(app.status("home").unwrap()["update_host_settings"], json!([]));
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Once the settings change, nothing open() answered before is served again, not even an
    /// answer still being worked out while they changed.
    #[test]
    fn new_settings_drop_what_open_answered() {
        let data = std::env::temp_dir().join(format!("rex-addons-settings-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
        let manifest = json!({"id": "home", "name": "Home", "version": "1.0.0", "hosts": ["10.0.0.2", "10.0.0.3", "10.0.0.4"],
            "config": [{"key": "server", "label": "Server", "type": "text"}]});
        let source = "export default { items() { return []; }, async open(id, ctx) { await sleep(300); return { url: `http://${ctx.config.server}/${id}.mp4` }; } };";
        app.install(&manifest, source, None).unwrap();
        // Disabled: new settings start no sync.
        app.addons.lock().unwrap().get_mut("home").unwrap().enabled = false;
        let opened = |app: &Arc<App>, item: &str| app.open("home", item).map(|o| o.url);
        app.set_config("home", &json!({"server": "10.0.0.2:8096"})).unwrap();
        assert_eq!(opened(&app, "a").unwrap(), "http://10.0.0.2:8096/a.mp4");
        assert!(app.addons.lock().unwrap()["home"].opened.contains_key("a"));
        app.set_config("home", &json!({"server": "10.0.0.3:8096"})).unwrap();
        assert_eq!(opened(&app, "a").unwrap(), "http://10.0.0.3:8096/a.mp4");
        // Changed while an open() runs: that call answers its own caller, and is not kept.
        let running = { let app = app.clone(); std::thread::spawn(move || opened(&app, "b")) };
        std::thread::sleep(Duration::from_millis(100));
        app.set_config("home", &json!({"server": "10.0.0.4:8096"})).unwrap();
        assert_eq!(running.join().unwrap().unwrap(), "http://10.0.0.3:8096/b.mp4");
        assert_eq!(opened(&app, "b").unwrap(), "http://10.0.0.4:8096/b.mp4");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn repositories_refresh_daily_and_retry_hourly() {
        let data = std::env::temp_dir().join(format!("rex-addons-repo-due-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
        for (id, fetched) in [("fresh", now() - 3600), ("stale", now() - 25 * 3600)] {
            add_local_repo(&app, id);
            app.repos.lock().unwrap().list.iter_mut().find(|r| r.id == id).unwrap().last_fetch = Some(fetched);
        }
        assert_eq!(app.due_repos(), ["stale"]);
        // Claimed: its fetch runs (and fails, say), so it is not due again for an hour.
        assert!(app.due_repos().is_empty());
        app.repos.lock().unwrap().attempted.insert("stale".into(), now() - 3601);
        assert_eq!(app.due_repos(), ["stale"]);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn an_addon_needing_a_newer_host_is_listed_incompatible_and_refused() {
        let (rex, _) = stub_rex();
        let data = std::env::temp_dir().join(format!("rex-addons-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
        write_addon(&app.repo_dir("one"), "old", "1.0.0");
        let dir = app.repo_dir("one").join("addons/new");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), json!({"id": "new", "name": "new", "version": "1.0.0", "host": "99"}).to_string()).unwrap();
        std::fs::write(dir.join("main.js"), "export default { items() { return []; }, open() {} };").unwrap();
        add_local_repo(&app, "one");
        let listed = app.repo_addons_json("one").unwrap();
        assert_eq!((listed[0]["id"].as_str(), &listed[0]["compatible"]), (Some("new"), &json!(false)));
        assert_eq!((listed[1]["id"].as_str(), &listed[1]["compatible"]), (Some("old"), &json!(true)));
        let (status, error) = app.install_from_repo("one", "new").unwrap_err();
        assert_eq!(status, 400, "{error}");
        assert!(error.contains("needs rex-addons 99 or newer"), "{error}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn a_default_repository_is_added_once_and_stays_removed() {
        let data = std::env::temp_dir().join(format!("rex-addons-defaults-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        // A host nothing answers on: the background fetch fails, the repository stays listed.
        let defaults = ["https://127.0.0.1:9/me/addons".to_string()];
        let app = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
        app.start_repos(&defaults);
        let listed = app.list_repos();
        assert_eq!((listed[0]["id"].as_str(), listed[0]["name"].as_str()), (Some("127-0-0-1-me-addons"), Some("addons")));
        app.remove_repo("127-0-0-1-me-addons").unwrap();
        drop(app);
        let app = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
        app.start_repos(&defaults);
        assert_eq!(app.list_repos(), json!([]));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    #[ignore = "network: GitHub and Codeberg"]
    fn real_repositories() {
        let data = std::env::temp_dir().join(format!("rex-addons-real-repos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
        let url = std::env::var("REX_ADDONS_REPO").unwrap_or("https://github.com/debridmediamanager/rex-addons".into());
        let (status, repo) = app.add_repo(&url).unwrap();
        eprintln!("{status} {repo}");
        assert_eq!(status, 201);
        let ids: Vec<&str> = repo["addons"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"internet-archive") && ids.contains(&"stremio"), "{ids:?}");
        assert_eq!(app.add_repo(&url).unwrap().0, 200);
        let (status, error) = app.add_repo("https://codeberg.org/Codeberg/Community").unwrap_err();
        eprintln!("{status} {error}");
        assert_eq!(status, 400);
        assert!(error.starts_with("no addons in it"), "{error}");
        assert!(!app.repo_dir("codeberg-org-codeberg-community").exists());
        let (status, error) = app.add_repo("https://notes.debridmediamanager.com/rex").unwrap_err();
        eprintln!("{status} {error}");
        assert_eq!(status, 400);
        let _ = std::fs::remove_dir_all(&data);
    }
}
