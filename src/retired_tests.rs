//! An installed addon whose manifest asks for a sign-in kind this host no longer has: the real
//! Spotify 1.0.1 (`tests/fixtures/spotify-1.0.1`, its main.js named `.fixture` so the repository doesn't offer it), which hosts 0.5 to 0.8 signed in by pairing
//! and kept `spotify-credentials.json` for. It stays listed with its titles until updated.
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::app::App;

const OLD_MANIFEST: &str = include_str!("../tests/fixtures/spotify-1.0.1/manifest.json");
const OLD_MAIN: &str = include_str!("../tests/fixtures/spotify-1.0.1/main.js.fixture");
const NEW_MANIFEST: &str = include_str!("../community/addons/spotify/manifest.json");
const NEW_MAIN: &str = include_str!("../community/addons/spotify/main.js");
const UPDATE: &str = "this addon needs an update: update Spotify in its repository";
const CREDENTIALS: &str = "spotify-credentials.json";

struct Rig {
    app: Arc<App>,
    api: SocketAddr,
    /// Every request rex was sent: `METHOD /path`.
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

fn data(name: &str) -> PathBuf {
    let data = std::env::temp_dir().join(format!("rex-addons-retired-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    data
}

/// `spotify` as an older host left it: `manifest` installed, signed in, three titles in rex
/// from a sync long ago, and the pairing's credentials beside them.
fn seed(data: &std::path::Path, manifest: &str, main: &str) {
    let dir = data.join("addons/spotify");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), manifest).unwrap();
    std::fs::write(dir.join("main.js"), main).unwrap();
    let state = json!({"enabled": true, "account": "Someone Listening", "items": 3, "last_sync": 1, "since": 1, "last_full": 1});
    std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
    std::fs::write(dir.join(CREDENTIALS), r#"{"username":"someone","auth_type":1,"auth_data":"c2VjcmV0"}"#).unwrap();
}

fn rig(data: PathBuf) -> Rig {
    let (rex_url, rex) = rex();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let api = server.server_addr().to_ip().unwrap();
    let app = App::new(data, rex_url, api, None, None);
    let served = app.clone();
    std::thread::spawn(move || crate::server::serve(served, server, None));
    Rig { app, api, rex }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.app.data);
    }
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
        self.app.status("spotify").expect("spotify is listed")
    }

    fn credentials(&self) -> PathBuf {
        self.app.data.join("addons/spotify").join(CREDENTIALS)
    }
}

#[test]
fn an_addon_of_a_removed_sign_in_kind_stays_listed_with_its_titles_and_asks_for_an_update() {
    let data = data("listed");
    seed(&data, OLD_MANIFEST, OLD_MAIN);
    let rig = rig(data);
    let status = rig.status();
    assert_eq!((&status["version"], &status["state"], &status["error"], &status["error_kind"], &status["items"]),
        (&json!("1.0.1"), &json!("error"), &json!(UPDATE), &json!("update"), &json!(3)));

    // It neither syncs, nor signs in, nor plays.
    assert!(rig.app.due().is_empty());
    assert!(!rig.app.start_sync("spotify", false, "manual"));
    assert_eq!(rig.call("POST", "/api/addons/spotify/sign-in"), (400, json!({"error": UPDATE})));
    assert_eq!(rig.app.open("spotify", "spotify:track:1").err().unwrap(), UPDATE);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(rig.status()["error"], UPDATE);
    // rex keeps its titles: nothing was registered, pushed or deleted.
    assert_eq!(*rig.rex.lock().unwrap(), Vec::<String>::new());
    // Its manifest still declares the kind the credentials belong to.
    assert!(rig.credentials().exists());

    // Still listed after a restart.
    let again = App::new(rig.app.data.clone(), rig.app.rex.clone(), "127.0.0.1:9".parse().unwrap(), None, None);
    assert_eq!(again.status("spotify").unwrap()["error"], UPDATE);

    // Signing out forgets the account and the credentials.
    let (code, answer) = rig.call("DELETE", "/api/addons/spotify/sign-in");
    assert_eq!((code, &answer["sign_in"]["account"]), (200, &Json::Null));
    assert!(!rig.credentials().exists());
}

#[test]
fn installing_an_addon_of_a_removed_sign_in_kind_is_refused() {
    let rig = rig(data("refused"));
    let old: Json = serde_json::from_str(OLD_MANIFEST).unwrap();
    assert_eq!(rig.app.install(&old, OLD_MAIN, None).unwrap_err(), UPDATE);
    assert!(rig.app.status("spotify").is_none());
    assert!(!rig.app.data.join("addons/spotify").exists());
}

/// Updated to 2.0.0, which signs in with a code and keeps its tokens in `storage`: the account
/// stays until the first sync, which 2.0 fails with SIGN_IN_REQUIRED (its storage has no tokens),
/// signing it out with why while its titles stay. The pairing's credentials go with the update.
#[test]
fn updating_it_to_spotify_2_forgets_the_pairing_signs_in_again_and_keeps_the_titles() {
    let data = data("update");
    seed(&data, OLD_MANIFEST, OLD_MAIN);
    let rig = rig(data);
    rig.app.install(&json!({"id": "youtube", "name": "YouTube", "version": "1.0.0", "host": "0.5", "exports": ["findSong", "songAudio"]}),
        "export default { items() { return []; }, open() {}, findSong() { return null; }, songAudio() { return null; } };", None).unwrap();
    let new: Json = serde_json::from_str(NEW_MANIFEST).unwrap();
    rig.app.install(&new, NEW_MAIN, None).unwrap();
    let status = rig.status();
    assert_eq!((&status["version"], &status["sign_in"]["kind"], &status["sign_in"]["account"]), (&json!("2.0.0"), &json!("code"), &json!("Someone Listening")));
    assert_eq!((&status["error"], &status["error_kind"]), (&Json::Null, &Json::Null));
    assert!(!rig.credentials().exists());

    assert!(rig.app.start_sync("spotify", false, "manual"));
    let mut status = rig.status();
    for _ in 0..400 {
        if status["state"] != "syncing" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
        status = rig.status();
    }
    assert_eq!((&status["error"], &status["error_kind"]), (&json!("sign in again: this update signs in to Spotify with a code"), &json!("sign_in")));
    assert_eq!(status["sign_in"]["account"], Json::Null);
    assert!(!rig.rex.lock().unwrap().iter().any(|r| r.starts_with("DELETE")), "rex keeps the titles");
}

/// An install a host before this one updated to 2.0.0, which left the pairing's credentials.
#[test]
fn credentials_left_by_a_removed_sign_in_kind_are_forgotten_on_start() {
    let data = data("start");
    seed(&data, NEW_MANIFEST, NEW_MAIN);
    let rig = rig(data);
    assert_eq!(rig.status()["version"], "2.0.0");
    assert!(!rig.credentials().exists());
}
