//! Signing an addon in with a code: a stub website's OAuth device flow, driven through a real
//! sandboxed addon and the API. The website answers on this machine's network address, as the
//! sandbox never fetches loopback.
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::addons::read_json;
use crate::app::{now, App, LOGGED};

fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// A generic OAuth device flow, as docs/writing-addons.md shows it.
const ADDON: &str = r#"
const SITE = "{BASE}";
const form = (fields) => new URLSearchParams(fields).toString();
const authorized = () => ({ headers: { Authorization: `Bearer ${storage.get("token")}` } });

export default {
  async items(ctx) {
    const answer = await fetch(`${SITE}/list`, authorized());
    if (answer.status === 401) throw new Error("SIGN_IN_REQUIRED: the website revoked access");
    if (!answer.ok) throw new Error(`the website answered ${answer.status}`);
    return await answer.json();
  },
  async open(id) {
    if ((await fetch(`${SITE}/check`, authorized())).status === 401) throw new Error("SIGN_IN_REQUIRED");
    return { url: `${SITE}/video/${id}.mp4` };
  },
  async signIn(ctx) {
    const answer = await (await fetch(`${SITE}/device/code`, { method: "POST", body: form({ client_id: "rex" }) })).json();
    return {
      code: answer.user_code, url: answer.verification_uri,
      expires_in: answer.expires_in, interval: answer.interval,
      state: { device_code: answer.device_code, user_code: answer.user_code },
    };
  },
  async signInPoll(state, ctx) {
    const answer = await (await fetch(`${SITE}/token`, { method: "POST", body: form({ device_code: state.device_code }) })).json();
    if (answer.error === "authorization_pending" || answer.error === "slow_down") {
      console.log(`waiting for ${state.user_code} to be entered`);
      return { done: false };
    }
    if (answer.error) throw new Error(`the website said ${answer.error}`);
    storage.set("token", answer.access_token);
    return { done: true, account: answer.name };
  },
  async signOut(ctx) {
    await fetch(`${SITE}/revoke`, { method: "POST", body: form({ token: storage.get("token") }) });
    storage.delete("token");
  },
};
"#;

#[derive(Default)]
struct Website {
    /// What the next codes live for.
    expires_in: u64,
    /// Device codes the person approved (true) or refused (false).
    answered: HashMap<String, bool>,
    revoked: bool,
    issued: usize,
    /// `METHOD /path body`, with when.
    hits: Vec<(String, Instant)>,
}

struct Rig {
    app: Arc<App>,
    id: String,
    api: SocketAddr,
    site: Arc<Mutex<Website>>,
    /// Every request rex was sent: `METHOD /path`.
    rex: Arc<Mutex<Vec<String>>>,
}

fn website(lan: IpAddr) -> (String, Arc<Mutex<Website>>) {
    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let base = format!("http://{lan}:{}", server.server_addr().to_ip().unwrap().port());
    let site = Arc::new(Mutex::new(Website { expires_in: 600, ..Website::default() }));
    let shared = site.clone();
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let path = request.url().to_string();
            let auth = request.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string()).unwrap_or_default();
            let mut site = shared.lock().unwrap();
            site.hits.push((format!("{} {path} {body}", request.method()), Instant::now()));
            let signed = auth.starts_with("Bearer tok-") && !site.revoked;
            let device = body.strip_prefix("device_code=").unwrap_or_default().to_string();
            let (status, answer) = match (request.method().as_str(), path.as_str()) {
                ("POST", "/device/code") => {
                    site.issued += 1;
                    let n = site.issued;
                    // An interval under the host's floor, and a lifetime over its ceiling for long codes.
                    (200, json!({"device_code": format!("dev-secret-{n}"), "user_code": format!("WDJB-MJH{n}"),
                        "verification_uri": "https://example.com/device", "expires_in": site.expires_in, "interval": 1}))
                }
                ("POST", "/token") => match site.answered.get(&device) {
                    Some(true) => (200, json!({"access_token": format!("tok-{device}"), "name": "ben@example.com"})),
                    Some(false) => (400, json!({"error": "access_denied"})),
                    None => (400, json!({"error": "authorization_pending"})),
                },
                ("GET", "/list") if signed => (200, json!([{"id": "a", "path": "films/a.mp4"}])),
                ("GET", "/check") if signed => (200, json!({})),
                ("POST", "/revoke") => (200, json!({})),
                _ => (401, json!({"error": "unauthorized"})),
            };
            drop(site);
            let _ = request.respond(tiny_http::Response::from_string(answer.to_string()).with_status_code(status));
        }
    });
    (base, site)
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

/// The addon `id`, installed and served: None when this machine has no network address.
fn rig(id: &str) -> Option<Rig> {
    let lan = lan_ip()?;
    let (base, site) = website(lan);
    let (rex_url, rex) = rex();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let api = server.server_addr().to_ip().unwrap();
    let data = std::env::temp_dir().join(format!("rex-addons-sign-in-{id}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let app = App::new(data, rex_url, api, None, None);
    let manifest = json!({"id": id, "name": "Example", "version": "1.0.0", "host": "0.3", "hosts": [lan.to_string()],
        "sign_in": {"label": "Sign in to Example"}});
    app.install(&manifest, &ADDON.replace("{BASE}", &base), None).unwrap();
    let served = app.clone();
    std::thread::spawn(move || crate::server::serve(served, server, None));
    Some(Rig { app, id: id.into(), api, site, rex })
}

impl Rig {
    fn call(&self, method: &str, path: &str) -> (u16, Json) {
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let answer = match agent.request(method, &format!("http://{}{path}", self.api)).call() {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{e}"),
        };
        (answer.status(), serde_json::from_str(&answer.into_string().unwrap()).unwrap_or(Json::Null))
    }

    fn status(&self) -> Json {
        self.app.status(&self.id).unwrap()
    }

    fn sign_in(&self) -> Json {
        let (status, answer) = self.call("POST", &format!("/api/addons/{}/sign-in", self.id));
        assert_eq!(status, 200, "{answer}");
        answer
    }

    fn until(&self, what: &str, done: impl Fn(&Json) -> bool) -> Json {
        for _ in 0..400 {
            let status = self.status();
            if done(&status) {
                return status;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("{what} never happened: {}", self.status());
    }

    fn approve(&self, device: &str, approved: bool) {
        self.site.lock().unwrap().answered.insert(device.into(), approved);
    }

    /// The website's hits whose line starts with `prefix`.
    fn hits(&self, prefix: &str) -> Vec<String> {
        self.site.lock().unwrap().hits.iter().map(|(hit, _)| hit.clone()).filter(|hit| hit.starts_with(prefix)).collect()
    }

    /// Signs in through the whole flow and waits for the sync it starts.
    fn signed_in(&self) {
        self.sign_in();
        self.approve("dev-secret-1", true);
        self.until("the sync after signing in", |s| s["sign_in"]["account"].is_string() && s["state"] == "idle" && s["last_sync"].is_u64());
    }

    fn logged(&self) -> Vec<String> {
        let tag = format!("{}]", self.id);
        LOGGED.lock().unwrap().iter().filter(|line| line.contains(&tag)).cloned().collect()
    }

    fn snapshots(&self) -> usize {
        self.rex.lock().unwrap().iter().filter(|r| r.ends_with("/snapshot")).count()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.app.data);
    }
}

macro_rules! rig {
    ($id:expr) => {
        match rig($id) {
            Some(rig) => rig,
            None => {
                eprintln!("skipped: no network address for the stub website");
                return;
            }
        }
    };
}

#[test]
fn a_code_shows_until_entered_then_the_account_is_kept_and_synced() {
    let rig = rig!("sign-flow");
    // Not signed in yet: nothing is asked of items(), and no sync is due.
    assert_eq!(rig.status()["sign_in"], json!({"label": "Sign in to Example", "kind": "code", "account": null, "pending": null, "error": null}));
    let started = Instant::now();
    let answer = rig.sign_in();
    let pending = &answer["sign_in"]["pending"];
    assert_eq!((pending["code"].as_str(), pending["url"].as_str()), (Some("WDJB-MJH1"), Some("https://example.com/device")));
    // 600 s, as the website asked.
    let expires_at = pending["expires_at"].as_u64().unwrap();
    assert!((now() + 598..=now() + 600).contains(&expires_at), "{expires_at}");
    assert_eq!(pending.as_object().unwrap().len(), 3, "{pending}");
    assert!(!answer.to_string().contains("dev-secret"), "{answer}");

    // Polled, but pending until the person enters the code.
    rig.until("a poll", |_| !rig.hits("POST /token").is_empty());
    assert_eq!(rig.status()["sign_in"]["pending"]["code"], "WDJB-MJH1");
    rig.approve("dev-secret-1", true);
    let status = rig.until("the sign-in", |s| s["sign_in"]["account"].is_string());
    assert_eq!(status["sign_in"], json!({"label": "Sign in to Example", "kind": "code", "account": "ben@example.com", "pending": null, "error": null}));
    // The website asked for 1 s between polls; the host waits 2.
    let site = rig.site.lock().unwrap();
    let first = site.hits.iter().find(|(hit, _)| hit.starts_with("POST /token")).unwrap().1;
    assert!(first.duration_since(started) >= Duration::from_millis(1900), "{:?}", first.duration_since(started));
    drop(site);

    // Signing in started a sync, with the token the addon kept.
    let status = rig.until("the sync", |s| s["state"] == "idle" && s["last_sync"].is_u64());
    assert_eq!((status["error"].clone(), status["held"].as_u64()), (Json::Null, Some(1)));
    assert_eq!(rig.hits("GET /list").len(), 1);
    assert_eq!(rig.snapshots(), 1);

    // Neither the code nor the state was logged, though the addon printed the code; the
    // state is not on disk either.
    let logged = rig.logged();
    assert!(logged.iter().any(|l| l.contains("[addon sign-flow] waiting for ******** to be entered")), "{logged:?}");
    assert!(logged.iter().any(|l| l.contains("signed in as ben@example.com")), "{logged:?}");
    for line in &logged {
        assert!(!line.contains("WDJB-MJH1") && !line.contains("dev-secret"), "{line}");
    }
    let state = std::fs::read_to_string(rig.app.data.join("addons/sign-flow/state.json")).unwrap();
    assert!(!state.contains("dev-secret") && !state.contains("WDJB"), "{state}");

    // The account outlives a restart, and an update of the addon.
    let again = App::new(rig.app.data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
    assert_eq!(again.status("sign-flow").unwrap()["sign_in"]["account"], "ben@example.com");
    let (mut manifest, source) = {
        let addons = again.addons.lock().unwrap();
        (addons["sign-flow"].manifest.raw.clone(), addons["sign-flow"].source.to_string())
    };
    manifest["version"] = json!("1.1.0");
    again.install(&manifest, &source, None).unwrap();
    assert_eq!(again.status("sign-flow").unwrap()["sign_in"]["account"], "ben@example.com");
}

#[test]
fn an_unsigned_addon_neither_syncs_nor_plays() {
    let rig = rig!("sign-none");
    assert!(rig.app.start_sync("sign-none", false, "manual"));
    let status = rig.until("the sync", |s| s["state"] != "syncing");
    assert_eq!((status["error"].as_str(), status["error_kind"].as_str()), (Some("sign in first"), Some("sign_in")));
    assert!(rig.hits("GET /list").is_empty());
    assert!(rig.rex.lock().unwrap().is_empty());
    // Not retried every 5 minutes: signing in syncs it.
    assert!(rig.app.due().is_empty());
    rig.app.addons.lock().unwrap().get_mut("sign-none").unwrap().paths.insert("films/a.mp4".into(), "a".into());
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let read = agent.get(&format!("http://{}/bytes/sign-none/films/a.mp4", rig.api)).call();
    match read {
        Err(ureq::Error::Status(502, answer)) => assert!(answer.into_string().unwrap().contains("sign in first")),
        other => panic!("{other:?}"),
    }
    assert!(rig.hits("GET /check").is_empty());
}

#[test]
fn a_code_not_entered_in_time_expires() {
    let rig = rig!("sign-expiry");
    rig.site.lock().unwrap().expires_in = 1;
    rig.sign_in();
    let status = rig.until("the expiry", |s| s["sign_in"]["pending"].is_null());
    assert_eq!(status["sign_in"]["error"], "the code expired before it was entered");
    assert_eq!(status["sign_in"]["account"], Json::Null);
    assert!(rig.hits("POST /token").is_empty());
}

#[test]
fn a_refused_sign_in_keeps_the_websites_reason() {
    let rig = rig!("sign-refused");
    rig.sign_in();
    rig.approve("dev-secret-1", false);
    let status = rig.until("the refusal", |s| s["sign_in"]["pending"].is_null());
    assert_eq!(status["sign_in"]["error"], "the website said access_denied");
    assert_eq!(status["sign_in"]["account"], Json::Null);
    assert!(rig.hits("GET /list").is_empty());
    // A new sign-in clears it.
    assert_eq!(rig.sign_in()["sign_in"]["error"], Json::Null);
}

#[test]
fn a_new_sign_in_replaces_the_pending_one() {
    let rig = rig!("sign-again");
    rig.sign_in();
    assert_eq!(rig.sign_in()["sign_in"]["pending"]["code"], "WDJB-MJH2");
    rig.approve("dev-secret-1", true);
    rig.approve("dev-secret-2", true);
    rig.until("the sign-in", |s| s["sign_in"]["account"].is_string());
    std::thread::sleep(Duration::from_millis(500));
    // The first code's poll stopped when it was replaced: it was never asked about.
    assert_eq!(rig.hits("POST /token"), ["POST /token device_code=dev-secret-2"]);
}

#[test]
fn signing_out_forgets_the_account_and_stops_a_pending_sign_in() {
    let rig = rig!("sign-out");
    rig.signed_in();
    let snapshots = rig.snapshots();
    let (status, answer) = rig.call("DELETE", "/api/addons/sign-out/sign-in");
    assert_eq!(status, 200);
    assert_eq!(answer["sign_in"], json!({"label": "Sign in to Example", "kind": "code", "account": null, "pending": null, "error": null}));
    // The addon's signOut() ran and forgot its token; its titles stayed.
    assert_eq!(rig.hits("POST /revoke"), ["POST /revoke token=tok-dev-secret-1"]);
    assert_eq!(read_json(&rig.app.data.join("addons/sign-out/storage.json")).unwrap().get("token"), None);
    assert!(!rig.rex.lock().unwrap().iter().any(|r| r.starts_with("DELETE")));
    assert!(rig.app.due().is_empty());
    assert_eq!(rig.app.open("sign-out", "a").err().unwrap(), "sign in first");
    assert_eq!(rig.snapshots(), snapshots);

    // Signing out while a code is pending stops its poll.
    rig.sign_in();
    let polls = rig.hits("POST /token").len();
    assert_eq!(rig.call("DELETE", "/api/addons/sign-out/sign-in").1["sign_in"]["pending"], Json::Null);
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(rig.hits("POST /token").len(), polls);
}

#[test]
fn sign_in_required_from_items_signs_the_addon_out() {
    let rig = rig!("sign-revoked");
    rig.signed_in();
    let rex_calls = rig.rex.lock().unwrap().len();
    rig.site.lock().unwrap().revoked = true;
    assert!(rig.app.start_sync("sign-revoked", false, "manual"));
    let status = rig.until("the sync", |s| s["state"] != "syncing");
    assert_eq!(status["error"], "sign in again: the website revoked access");
    assert_eq!(status["error_kind"], "sign_in");
    assert_eq!(status["sign_in"]["account"], Json::Null);
    assert_eq!(status["sign_in"]["error"], "the website revoked access");
    // rex was told nothing: its titles stay.
    assert_eq!(rig.rex.lock().unwrap().len(), rex_calls);
    assert!(rig.app.due().is_empty());
}

#[test]
fn sign_in_required_from_open_signs_the_addon_out() {
    let rig = rig!("sign-open");
    rig.signed_in();
    assert!(rig.app.open("sign-open", "a").unwrap().url.ends_with("/video/a.mp4"));
    rig.site.lock().unwrap().revoked = true;
    rig.app.forget_open("sign-open", "a");
    assert!(rig.app.open("sign-open", "a").err().unwrap().starts_with("SIGN_IN_REQUIRED"));
    let status = rig.status();
    assert_eq!((&status["sign_in"]["account"], status["sign_in"]["error"].as_str()), (&Json::Null, Some("signed out by the website")));
    let saved = read_json(&rig.app.data.join("addons/sign-open/state.json")).unwrap();
    assert_eq!(saved["account"], Json::Null);
}

#[test]
fn the_api_refuses_what_cannot_sign_in() {
    let rig = rig!("sign-api");
    // An addon with no sign_in shows null, and has nothing to sign in to.
    let plain = "export default { items() { return []; }, open() {} };";
    rig.app.install(&json!({"id": "sign-plain", "name": "Plain", "version": "1.0.0"}), plain, None).unwrap();
    let (status, answer) = rig.call("GET", "/api/addons/sign-plain");
    assert_eq!((status, &answer["sign_in"]), (200, &Json::Null));
    assert_eq!(rig.call("GET", "/api/status").1["addons"][0]["sign_in"]["label"], "Sign in to Example");
    assert_eq!(rig.call("POST", "/api/addons/sign-plain/sign-in").0, 400);
    assert_eq!(rig.call("DELETE", "/api/addons/sign-plain/sign-in").0, 400);
    assert_eq!(rig.call("POST", "/api/addons/nope/sign-in").0, 404);

    // A disabled addon is not signed in, and disabling stops a pending sign-in.
    rig.sign_in();
    rig.app.set_enabled("sign-api", false).unwrap();
    assert_eq!(rig.status()["sign_in"]["pending"], Json::Null);
    assert_eq!(rig.call("POST", "/api/addons/sign-api/sign-in").0, 409);

    // signIn() throwing is a 502 with its message, and so is an answer with no https url.
    let manifest = json!({"id": "sign-broken", "name": "Broken", "version": "1.0.0", "host": "0.3", "sign_in": {}});
    let broken = "export default { items() { return []; }, open() {}, signIn() { throw new Error('the website is down'); }, signInPoll() {} };";
    rig.app.install(&manifest, broken, None).unwrap();
    let (status, answer) = rig.call("POST", "/api/addons/sign-broken/sign-in");
    assert_eq!((status, answer["error"].as_str()), (502, Some("the website is down")));
    let plain_http = "export default { items() { return []; }, open() {}, signInPoll() {},
        signIn() { return { code: 'X', url: 'http://example.com/device', expires_in: 60, interval: 5 }; } };";
    rig.app.install(&manifest, plain_http, None).unwrap();
    let (status, answer) = rig.call("POST", "/api/addons/sign-broken/sign-in");
    assert_eq!((status, answer["error"].as_str()), (502, Some("signIn() returned no https url")));
    assert_eq!(rig.status()["sign_in"]["label"], "Sign in to Example");
    assert_eq!(rig.app.status("sign-broken").unwrap()["sign_in"]["label"], "Sign in to Broken");
}

#[test]
fn an_addon_that_signs_in_must_export_both_functions() {
    let data = std::env::temp_dir().join(format!("rex-addons-sign-exports-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let app = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
    let manifest = json!({"id": "sign-exports", "name": "Exports", "version": "1.0.0", "host": "0.3", "sign_in": {}});
    for (source, missing) in [
        ("export default { items() {}, open() {}, signIn() {} };", "signInPoll"),
        ("export default { items() {}, open() {}, signInPoll() {} };", "signIn"),
    ] {
        let error = app.install(&manifest, source, None).unwrap_err();
        assert_eq!(error, format!("main.js: main.js exports no {missing}()"));
    }
    assert!(app.status("sign-exports").is_none());
    app.install(&manifest, "export default { items() {}, open() {}, signIn() {}, signInPoll() {} };", None).unwrap();
    let _ = std::fs::remove_dir_all(&data);
}
