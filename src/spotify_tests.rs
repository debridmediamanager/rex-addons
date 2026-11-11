//! Signing in with Spotify by pairing, the `spotify` global, and one addon calling another's
//! exports, driven through the API and real sandboxed addons. Spotify itself is a stub: what it
//! answers here is FABRICATED, shaped as Spotify's own API answered on 2026-11-11.
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::addons::read_json;
use crate::app::{now, App, LOGGED};
use crate::spotify::{fake_credentials, Pairing, Signed, Spotify};

type Done = Box<dyn FnOnce(Result<Json, String>) + Send>;

/// A pairing the stub was asked for: the name, how long, its `done`, and whether it was stopped.
struct Asked {
    name: String,
    lifetime: Duration,
    done: Option<Done>,
    stopped: Arc<AtomicBool>,
}

#[derive(Default)]
struct Stub {
    pairings: Mutex<Vec<Asked>>,
    /// `pair()` fails with this.
    refuse_pairing: Mutex<Option<String>>,
    /// Credentials (`auth_data`) Spotify no longer takes.
    revoked: Mutex<BTreeSet<String>>,
    /// Each addon's session: the credentials it was made from.
    sessions: Mutex<BTreeMap<String, String>>,
    connects: AtomicUsize,
    /// `METHOD path body`, of every request.
    requests: Mutex<Vec<String>>,
}

impl Spotify for Stub {
    fn pair(&self, name: &str, lifetime: Duration, done: Done) -> Result<Pairing, String> {
        if let Some(error) = self.refuse_pairing.lock().unwrap().clone() {
            return Err(error);
        }
        let stopped = Arc::new(AtomicBool::new(false));
        self.pairings.lock().unwrap().push(Asked { name: name.into(), lifetime, done: Some(done), stopped: stopped.clone() });
        Ok(Pairing::new(move || stopped.store(true, Ordering::SeqCst)))
    }

    fn connect(&self, addon: &str, credentials: &Json) -> Result<Signed, String> {
        let (user, secret) = (credentials["username"].as_str().unwrap(), credentials["auth_data"].as_str().unwrap());
        if self.revoked.lock().unwrap().contains(secret) {
            return Err("SIGN_IN_REQUIRED: Spotify no longer accepts this sign-in (Bad credentials)".into());
        }
        // Spotify hands a session reusable credentials of its own, which are kept from then on.
        let renewed = if secret.ends_with("-renewed") { secret.to_string() } else { format!("{secret}-renewed") };
        if self.sessions.lock().unwrap().insert(addon.into(), renewed.clone()).is_none() {
            self.connects.fetch_add(1, Ordering::SeqCst);
        }
        Ok(Signed { username: user.into(), account: "Someone Listening".into(), credentials: fake_credentials(user, &renewed) })
    }

    fn request(&self, addon: &str, method: &str, path: &str, body: Option<&Json>) -> Result<Json, String> {
        self.requests.lock().unwrap().push(format!("{method} {path} {}", body.map_or(String::new(), Json::to_string)));
        if !self.sessions.lock().unwrap().contains_key(addon) {
            return Err("not connected to Spotify".into());
        }
        Ok(match path.split('?').next().unwrap() {
            "/playlist/v2/user/someone/rootlist" => json!({"length": 2, "contents": {"items": [
                {"uri": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"}, {"uri": "spotify:playlist:37i9dQZF1DX0XUsuxWHRQd"}]}}),
            "/collection/v2/paging" => json!({"items": [
                {"uri": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "addedAt": "1700000000"},
                {"uri": "spotify:album:6dVIqQ8qmQ5GBnJ9shOYGE", "addedAt": "1700000100"},
                {"uri": "spotify:track:7GhIk7Il098yCjg4BQjzvb", "addedAt": "1700000200"}]}),
            _ => return Err(format!("Spotify answered 404 to {path}")),
        })
    }

    fn forget(&self, addon: &str) {
        self.sessions.lock().unwrap().remove(addon);
    }
}

impl Stub {
    /// The newest pairing's `done`, called as the Spotify app would.
    fn finish(&self, outcome: Result<Json, String>) {
        let done = self.pairings.lock().unwrap().last_mut().unwrap().done.take().unwrap();
        done(outcome);
    }

    /// Whether pairing `n` was stopped.
    fn stopped(&self, n: usize) -> bool {
        self.pairings.lock().unwrap()[n].stopped.load(Ordering::SeqCst)
    }

    /// Spotify drops the connection and refuses the credentials from now on.
    fn revoke(&self) {
        let mut sessions = self.sessions.lock().unwrap();
        self.revoked.lock().unwrap().extend(sessions.values().cloned());
        sessions.clear();
    }
}

/// Lists liked songs and playlists through the `spotify` global, as a Spotify addon would.
const LIBRARY: &str = r#"
export default {
  async items(ctx) {
    const user = spotify.user();
    const root = await spotify.get(`/playlist/v2/user/${user}/rootlist?decorate=length,attributes`);
    const library = await spotify.post("/collection/v2/paging", { username: user, set: "collection", limit: 300 });
    const tracks = library.items.filter((item) => item.uri.startsWith("spotify:track:"));
    return tracks.map((track, n) => ({ id: track.uri, path: `spotify/${n}.m4a`, display: { title: `one of ${root.length} playlists` } }));
  },
  async open(id) {
    if (id === "evil") return { url: String(await spotify.get("@evil.example/steal")) };
    return { url: "https://cdn.example.com/a.m4a" };
  },
};
"#;

struct Rig {
    app: Arc<App>,
    api: SocketAddr,
    stub: Arc<Stub>,
    rex: Arc<Mutex<Vec<String>>>,
}

fn rex() -> (String, Arc<Mutex<Vec<String>>>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = seen.clone();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            recorded.lock().unwrap().push(format!("{} {}", request.method(), request.url()));
            let _ = request.respond(tiny_http::Response::from_string("{}"));
        }
    });
    (url, seen)
}

fn rig(name: &str) -> Rig {
    let (rex_url, rex) = rex();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let api = server.server_addr().to_ip().unwrap();
    let data = std::env::temp_dir().join(format!("rex-addons-spotify-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let stub = Arc::new(Stub::default());
    let app = App::with_spotify(data, rex_url, api, None, None, "rex-test".into(), stub.clone());
    let served = app.clone();
    std::thread::spawn(move || crate::server::serve(served, server, None));
    Rig { app, api, stub, rex }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.app.data);
    }
}

fn spotify_manifest(id: &str) -> Json {
    json!({"id": id, "name": "Spotify", "version": "1.0.0", "host": "0.5", "hosts": ["cdn.example.com"],
        "sign_in": {"kind": "spotify", "label": "Connect Spotify"}})
}

impl Rig {
    fn spotify(name: &str) -> Rig {
        let rig = rig(name);
        rig.app.install(&spotify_manifest("spotify"), LIBRARY, None).unwrap();
        rig
    }

    fn call(&self, method: &str, path: &str) -> (u16, Json) {
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let answer = match agent.request(method, &format!("http://{}{path}", self.api)).call() {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{e}"),
        };
        (answer.status(), serde_json::from_str(&answer.into_string().unwrap()).unwrap_or(Json::Null))
    }

    fn status(&self, id: &str) -> Json {
        self.app.status(id).unwrap()
    }

    fn until(&self, id: &str, what: &str, done: impl Fn(&Json) -> bool) -> Json {
        for _ in 0..400 {
            let status = self.status(id);
            if done(&status) {
                return status;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("{what} never happened: {}", self.status(id));
    }

    fn credentials_file(&self) -> std::path::PathBuf {
        self.app.data.join("addons/spotify/spotify-credentials.json")
    }

    /// Pairs and waits for the sync that follows.
    fn signed_in(&self) {
        assert_eq!(self.call("POST", "/api/addons/spotify/sign-in").0, 200);
        self.stub.finish(Ok(fake_credentials("someone", "blob-1")));
        self.until("spotify", "the sync after pairing", |s| s["sign_in"]["account"].is_string() && s["state"] == "idle" && s["last_sync"].is_u64());
    }
}

#[test]
fn pairing_signs_in_keeps_the_credentials_to_itself_and_syncs() {
    let rig = Rig::spotify("pair");
    let signed_out = json!({"label": "Connect Spotify", "kind": "spotify", "account": null, "pending": null, "error": null});
    assert_eq!(rig.status("spotify")["sign_in"], signed_out);
    // Not signed in: nothing syncs.
    assert!(rig.app.due().is_empty());

    let (status, answer) = rig.call("POST", "/api/addons/spotify/sign-in");
    assert_eq!(status, 200, "{answer}");
    let pending = &answer["sign_in"]["pending"];
    assert_eq!(pending["device"], "rex-test");
    let expires_at = pending["expires_at"].as_u64().unwrap();
    assert!((now() + 598..=now() + 600).contains(&expires_at), "{expires_at}");
    assert_eq!(pending.as_object().unwrap().len(), 2, "{pending}");
    {
        let pairings = rig.stub.pairings.lock().unwrap();
        assert_eq!((pairings.len(), pairings[0].name.as_str(), pairings[0].lifetime), (1, "rex-test", Duration::from_secs(600)));
    }
    assert!(!rig.stub.stopped(0));

    // The person picks rex-test in the Spotify app.
    rig.stub.finish(Ok(fake_credentials("someone", "blob-1")));
    let status = rig.until("spotify", "the sync", |s| s["sign_in"]["account"].is_string() && s["state"] == "idle" && s["last_sync"].is_u64());
    assert_eq!(status["sign_in"], json!({"label": "Connect Spotify", "kind": "spotify", "account": "Someone Listening", "pending": null, "error": null}));
    // The pairing ended with the sign-in: the device is no longer discoverable.
    assert!(rig.stub.stopped(0));
    // The addon listed the library through the global, on the session the pairing made.
    let requests = rig.stub.requests.lock().unwrap().clone();
    assert_eq!(requests, [
        "GET /playlist/v2/user/someone/rootlist?decorate=length,attributes ",
        "POST /collection/v2/paging {\"username\":\"someone\",\"set\":\"collection\",\"limit\":300}",
    ]);
    assert_eq!(rig.stub.connects.load(Ordering::SeqCst), 1);
    assert_eq!((status["held"].as_u64(), status["error"].clone()), (Some(2), Json::Null));
    assert_eq!(rig.rex.lock().unwrap().iter().filter(|r| r.ends_with("/snapshot")).count(), 1);

    // The credentials Spotify renewed are kept, readable by this user only, and nowhere else:
    // not in the API, the state, the addon's storage or the log.
    let file = rig.credentials_file();
    assert_eq!(read_json(&file).unwrap(), fake_credentials("someone", "blob-1-renewed"));
    #[cfg(unix)]
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&file).unwrap().permissions()) & 0o777, 0o600);
    let detail = rig.call("GET", "/api/addons/spotify").1.to_string();
    let everything = [detail, rig.call("GET", "/api/status").1.to_string(),
        std::fs::read_to_string(rig.app.data.join("addons/spotify/state.json")).unwrap(),
        std::fs::read_to_string(rig.app.data.join("addons/spotify/storage.json")).unwrap_or_default(),
        LOGGED.lock().unwrap().join("\n")];
    for text in everything {
        assert!(!text.contains("blob-1"), "{text}");
    }

    // After a restart the saved credentials connect a new session by themselves.
    rig.stub.forget("spotify");
    let again = App::with_spotify(rig.app.data.clone(), rig.app.rex.clone(), "127.0.0.1:9".parse().unwrap(), None, None, "rex-test".into(), rig.stub.clone());
    assert_eq!(again.status("spotify").unwrap()["sign_in"]["account"], "Someone Listening");
    assert!(again.start_sync("spotify", false));
    for _ in 0..400 {
        if again.status("spotify").unwrap()["state"] != "syncing" { break }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(again.status("spotify").unwrap()["error"], Json::Null);
    assert_eq!(rig.stub.connects.load(Ordering::SeqCst), 2);
}

#[test]
fn a_new_pairing_replaces_the_old_and_cancelling_stops_it() {
    let rig = Rig::spotify("cancel");
    rig.call("POST", "/api/addons/spotify/sign-in");
    rig.call("POST", "/api/addons/spotify/sign-in");
    assert!(rig.stub.stopped(0) && !rig.stub.stopped(1));
    // The replaced pairing's credentials, coming late, are ignored.
    let first = rig.stub.pairings.lock().unwrap()[0].done.take().unwrap();
    first(Ok(fake_credentials("someone", "blob-old")));
    assert_eq!(rig.status("spotify")["sign_in"]["pending"]["device"], "rex-test");

    let (status, answer) = rig.call("DELETE", "/api/addons/spotify/sign-in");
    assert_eq!((status, &answer["sign_in"]["pending"]), (200, &Json::Null));
    assert!(rig.stub.stopped(1));
    rig.stub.finish(Ok(fake_credentials("someone", "blob-late")));
    std::thread::sleep(Duration::from_millis(200));
    let status = rig.status("spotify");
    assert_eq!((&status["sign_in"]["account"], status["last_sync"].clone()), (&Json::Null, Json::Null));
    assert!(!rig.credentials_file().exists());
    assert_eq!(rig.stub.connects.load(Ordering::SeqCst), 0);

    // Disabling the addon stops a pairing too.
    rig.call("POST", "/api/addons/spotify/sign-in");
    rig.app.set_enabled("spotify", false).unwrap();
    assert!(rig.stub.stopped(2));
    assert_eq!(rig.status("spotify")["sign_in"]["pending"], Json::Null);
    assert_eq!(rig.call("POST", "/api/addons/spotify/sign-in").0, 409);
}

#[test]
fn a_pairing_that_ends_without_credentials_says_why() {
    let rig = Rig::spotify("fail");
    rig.call("POST", "/api/addons/spotify/sign-in");
    rig.stub.finish(Err("nobody picked rex-test in the Spotify app in time".into()));
    let status = rig.until("spotify", "the expiry", |s| s["sign_in"]["pending"].is_null());
    assert_eq!(status["sign_in"]["error"], "nobody picked rex-test in the Spotify app in time");
    assert_eq!(status["sign_in"]["account"], Json::Null);

    // Credentials Spotify refuses: the reason, without the host's SIGN_IN_REQUIRED.
    rig.stub.revoked.lock().unwrap().insert("blob-bad".into());
    rig.call("POST", "/api/addons/spotify/sign-in");
    rig.stub.finish(Ok(fake_credentials("someone", "blob-bad")));
    let status = rig.until("spotify", "the refusal", |s| s["sign_in"]["pending"].is_null());
    assert_eq!(status["sign_in"]["error"], "Spotify no longer accepts this sign-in (Bad credentials)");
    assert!(!rig.credentials_file().exists());

    // A pairing that cannot start is a 502 with the reason.
    *rig.stub.refuse_pairing.lock().unwrap() = Some("cannot be discovered on the local network: address in use".into());
    let (status, answer) = rig.call("POST", "/api/addons/spotify/sign-in");
    assert_eq!((status, answer["error"].as_str()), (502, Some("cannot be discovered on the local network: address in use")));
}

#[test]
fn a_session_spotify_ends_signs_the_addon_out() {
    let rig = Rig::spotify("revoked");
    rig.signed_in();
    let rex_calls = rig.rex.lock().unwrap().len();
    rig.stub.revoke();
    assert!(rig.app.start_sync("spotify", false));
    let status = rig.until("spotify", "the sync", |s| s["state"] != "syncing");
    assert_eq!(status["error"], "sign in again: Spotify no longer accepts this sign-in (Bad credentials)");
    assert_eq!(status["error_kind"], "sign_in");
    assert_eq!(status["sign_in"]["account"], Json::Null);
    // Dead credentials are not kept, and rex keeps the titles.
    assert!(!rig.credentials_file().exists());
    assert_eq!(rig.rex.lock().unwrap().len(), rex_calls);
    assert!(rig.app.due().is_empty());
}

#[test]
fn signing_out_forgets_the_session_and_the_credentials() {
    let rig = Rig::spotify("out");
    rig.signed_in();
    assert!(rig.stub.sessions.lock().unwrap().contains_key("spotify"));
    let (status, answer) = rig.call("DELETE", "/api/addons/spotify/sign-in");
    assert_eq!((status, &answer["sign_in"]["account"]), (200, &Json::Null));
    assert!(!rig.stub.sessions.lock().unwrap().contains_key("spotify"));
    assert!(!rig.credentials_file().exists());
    assert_eq!(rig.app.open("spotify", "a").err().unwrap(), "sign in first");
    // Removing the addon ends its session too.
    rig.signed_in();
    rig.app.remove("spotify").unwrap();
    assert!(rig.stub.sessions.lock().unwrap().is_empty());
}

#[test]
fn the_spotify_global_stays_on_spotifys_host_and_only_spotify_addons_get_it() {
    let rig = Rig::spotify("global");
    rig.signed_in();
    let error = rig.app.open("spotify", "evil").err().unwrap();
    assert!(error.starts_with("spotify: \"@evil.example/steal\" is not a path"), "{error}");
    assert!(!rig.stub.requests.lock().unwrap().iter().any(|r| r.contains("evil")));

    // A code sign-in and no sign-in get no `spotify`, and need no host 0.5.
    let probe = "export default { items() { return []; }, open() { return { url: 'https://cdn.example.com/' + typeof spotify + '-' + typeof addons }; },
        signIn() {}, signInPoll() {} };";
    rig.app.install(&json!({"id": "plain", "name": "Plain", "version": "1.0.0", "hosts": ["cdn.example.com"]}), probe, None).unwrap();
    rig.app.install(&json!({"id": "coded", "name": "Coded", "version": "1.0.0", "host": "0.3", "hosts": ["cdn.example.com"], "sign_in": {}}), probe, None).unwrap();
    rig.app.addons.lock().unwrap().get_mut("coded").unwrap().account = Some("x".into());
    for id in ["plain", "coded"] {
        assert_eq!(rig.app.open(id, "a").unwrap().url, "https://cdn.example.com/undefined-undefined", "{id}");
    }
    // A Spotify addon exports no signIn() or signInPoll(): the host signs it in.
    assert!(rig.status("spotify")["sign_in"]["kind"] == "spotify");
}

// One addon calling another's exports.

/// An addon others may call: `find` remembers what it was asked in its own storage and answers
/// a link on its own host; `fail` and `revoked` throw.
const CALLEE: &str = r#"
export default {
  items() { return []; },
  open() { return { url: "https://tube.example/x" }; },
  signIn() {}, signInPoll() {},
  async find(query, ctx) {
    storage.set("asked", query);
    return { url: `https://tube.example/${query}.m4a`, quality: ctx.config.quality };
  },
  fail(query) { throw new Error(`no match for ${query}`); },
  revoked() { throw new Error("SIGN_IN_REQUIRED: the token was revoked"); },
  async onward() { return await addons.call("caller", "find", "x"); },
  secret() { return "not exported"; },
};
"#;

/// Plays what the callee finds, and returns what any call answers or throws.
const CALLER: &str = r#"
export default {
  items() { return []; },
  async open(id) {
    const [name, query] = id.split(":");
    try {
      const found = await addons.call("tube", name, query);
      return { url: found.url, headers: { "X-Quality": String(found.quality) } };
    } catch (error) {
      return { url: "https://tube.example/error?" + encodeURIComponent(error.message) };
    }
  },
  find() { return {}; },
};
"#;

fn callee_manifest(extra: Json) -> Json {
    let mut manifest = json!({"id": "tube", "name": "Tube", "version": "1.0.0", "host": "0.5", "hosts": ["tube.example"],
        "sign_in": {}, "exports": ["find", "fail", "revoked", "onward"],
        "config": [{"key": "quality", "label": "Quality", "type": "number", "default": 1080}]});
    for (key, value) in extra.as_object().unwrap() {
        manifest[key] = value.clone();
    }
    manifest
}

fn calls_rig(name: &str) -> Rig {
    let rig = rig(name);
    let caller = json!({"id": "caller", "name": "Caller", "version": "1.0.0", "host": "0.5", "hosts": ["tube.example"],
        "requires": ["tube"], "exports": ["find"]});
    rig.app.install(&caller, CALLER, None).unwrap();
    rig
}

/// What the caller's open() of `name:query` says: the link it got, or the error it caught.
fn ask(rig: &Rig, name: &str, query: &str) -> String {
    let item = format!("{name}:{query}");
    rig.app.forget_open("caller", &item);
    let url = rig.app.open("caller", &item).unwrap().url;
    match url.strip_prefix("https://tube.example/error?") {
        Some(error) => format!("error: {}", percent_encoding::percent_decode_str(error).decode_utf8_lossy()),
        None => url,
    }
}

fn signed_in(rig: &Rig, id: &str, account: Option<&str>) {
    rig.app.addons.lock().unwrap().get_mut(id).unwrap().account = account.map(str::to_string);
}

#[test]
fn an_addon_calls_anothers_export_in_the_others_sandbox() {
    let rig = calls_rig("calls");
    rig.app.install(&callee_manifest(json!({})), CALLEE, None).unwrap();
    signed_in(&rig, "tube", Some("someone"));
    let opened = rig.app.open("caller", "find:song").unwrap();
    // The callee's own settings, and its answer passed through the caller's open().
    assert_eq!(opened.url, "https://tube.example/song.m4a");
    assert_eq!(opened.headers, [("X-Quality".to_string(), "1080".to_string())]);
    // It wrote its own storage, not the caller's.
    assert_eq!(read_json(&rig.app.data.join("addons/tube/storage.json")).unwrap()["asked"], "song");
    assert!(read_json(&rig.app.data.join("addons/caller/storage.json")).is_none_or(|s| s.get("asked").is_none()));
    assert_eq!(rig.status("caller")["requires"], json!([{"id": "tube", "ok": true, "reason": null}]));
    assert_eq!(rig.status("tube")["requires"], json!([]));
}

#[test]
fn unmet_requires_are_shown_and_explained() {
    let rig = calls_rig("unmet");
    let requires = |rig: &Rig| rig.status("caller")["requires"].clone();
    assert_eq!(requires(&rig), json!([{"id": "tube", "ok": false, "reason": "not installed"}]));
    assert_eq!(ask(&rig, "find", "a"), "error: the addon tube is not installed: install it and sign in to it first");

    rig.app.install(&callee_manifest(json!({})), CALLEE, None).unwrap();
    assert_eq!(requires(&rig), json!([{"id": "tube", "ok": false, "reason": "sign in needed"}]));
    assert_eq!(ask(&rig, "find", "a"), "error: sign in to Tube first");

    signed_in(&rig, "tube", Some("someone"));
    rig.app.set_enabled("tube", false).unwrap();
    assert_eq!(requires(&rig), json!([{"id": "tube", "ok": false, "reason": "disabled"}]));
    assert_eq!(ask(&rig, "find", "a"), "error: Tube is disabled: enable it first");

    rig.app.set_enabled("tube", true).unwrap();
    assert_eq!(requires(&rig), json!([{"id": "tube", "ok": true, "reason": null}]));
    // Only what it exports, and only for addons that require it.
    assert_eq!(ask(&rig, "secret", "a"), "error: Tube exports no secret()");
    assert_eq!(rig.app.call_export("tube", "caller", "find", vec![]).unwrap_err(), "caller is not in this addon's requires");
}

#[test]
fn errors_propagate_and_a_callee_signed_out_leaves_the_caller_signed_in() {
    let rig = calls_rig("errors");
    rig.app.install(&callee_manifest(json!({})), CALLEE, None).unwrap();
    signed_in(&rig, "tube", Some("someone"));
    assert_eq!(ask(&rig, "fail", "x"), "error: Tube: no match for x");

    assert_eq!(ask(&rig, "revoked", "x"), "error: sign in to Tube again: the token was revoked");
    let tube = rig.status("tube");
    assert_eq!((&tube["sign_in"]["account"], tube["sign_in"]["error"].as_str()), (&Json::Null, Some("the token was revoked")));
    assert_eq!(rig.status("caller")["requires"][0]["reason"], "sign in needed");

    // A caller that signs in itself stays signed in when the error comes through uncaught.
    let rethrows = "export default { items() { return []; }, signIn() {}, signInPoll() {},
        async open() { return await addons.call('tube', 'revoked'); } };";
    let manifest = json!({"id": "rethrows", "name": "Rethrows", "version": "1.0.0", "host": "0.5", "sign_in": {}, "requires": ["tube"]});
    rig.app.install(&manifest, rethrows, None).unwrap();
    signed_in(&rig, "rethrows", Some("someone"));
    signed_in(&rig, "tube", Some("someone"));
    let error = rig.app.open("rethrows", "x").err().unwrap();
    assert!(error.starts_with("sign in to Tube again: the token was revoked"), "{error}");
    assert_eq!(rig.status("rethrows")["sign_in"]["account"], "someone");
}

#[test]
fn a_called_addon_cannot_call_a_third() {
    let rig = calls_rig("depth");
    rig.app.install(&callee_manifest(json!({"requires": ["caller"]})), CALLEE, None).unwrap();
    signed_in(&rig, "tube", Some("someone"));
    assert_eq!(ask(&rig, "onward", "x"), "error: Tube: addons.call: an addon called by another cannot call a third");
}

#[test]
fn installing_checks_exports_exist() {
    let rig = rig("exports");
    let manifest = callee_manifest(json!({"exports": ["find", "missing"]}));
    assert_eq!(rig.app.install(&manifest, CALLEE, None).unwrap_err(), "main.js: main.js exports no missing()");
    // A Spotify addon needs no signIn() or signInPoll(), unlike a code sign-in.
    rig.app.install(&spotify_manifest("spotify"), "export default { items() {}, open() {} };", None).unwrap();
}
