//! Addons that find and translate subtitles: what installing takes from them and what rex is told,
//! driven through real sandboxed addons and a stub rex.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::app::App;

/// Each request rex was sent: its path and body.
type Sent = Arc<Mutex<Vec<(String, Json)>>>;

/// A rex that takes every registration and snapshot.
fn rex() -> (String, Sent) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let sent: Sent = Arc::default();
    let recorded = sent.clone();
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            recorded.lock().unwrap().push((request.url().to_string(), serde_json::from_str(&body).unwrap_or(Json::Null)));
            let _ = request.respond(tiny_http::Response::from_string("{}"));
        }
    });
    (url, sent)
}

fn data(name: &str) -> PathBuf {
    let data = std::env::temp_dir().join(format!("rex-addons-subtitles-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    data
}

fn manifest(id: &str) -> Json {
    json!({"id": id, "name": id, "version": "1.0.0"})
}

fn idle(app: &App, id: &str) -> Json {
    for _ in 0..400 {
        let status = app.status(id).unwrap();
        if status["state"] != "syncing" {
            return status;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the sync never ended");
}

/// An addon may list titles, find subtitles, translate them, or any of those, each group of
/// functions whole; what it does is kept over a restart, as nothing runs main.js on a start.
#[test]
fn installing_takes_any_whole_group_of_functions() {
    let data = data("groups");
    let app = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
    for (id, source, provides) in [
        ("finds", "export default { subtitles() {}, subtitle() {} };", json!(["subtitles"])),
        ("translates", "export default { translate() {} };", json!(["translate"])),
        ("all", "export default { items() {}, open() {}, subtitles() {}, subtitle() {}, translate() {} };", json!(["content", "subtitles", "translate"])),
        ("titles", "export default { items() {}, open() {} };", json!(["content"])),
    ] {
        app.install(&manifest(id), source, None).unwrap();
        assert_eq!(app.status(id).unwrap()["provides"], provides, "{id}");
    }
    for (source, error) in [
        ("export default { subtitles() {} };", "main.js exports no subtitle()"),
        ("export default { subtitle() {}, translate() {} };", "main.js exports no subtitles()"),
        ("export default { items() {}, open() {}, subtitles() {} };", "main.js exports no subtitle()"),
        ("export default { open() {}, translate() {} };", "main.js exports no items()"),
        ("export default { search() {} };", "main.js exports none of items() and open(), subtitles() and subtitle(), or translate()"),
    ] {
        assert_eq!(app.install(&manifest("half"), source, None).unwrap_err(), format!("main.js: {error}"), "{source}");
    }
    assert!(app.status("half").is_none());
    // A sign-in still needs both of its functions.
    let signs_in = json!({"id": "signs", "name": "Signs", "version": "1.0.0", "host": "0.3", "sign_in": {}});
    assert_eq!(app.install(&signs_in, "export default { translate() {}, signIn() {} };", None).unwrap_err(), "main.js: main.js exports no signInPoll()");
    // An update that drops a group drops it from what the addon does.
    app.install(&manifest("all"), "export default { subtitles() {}, subtitle() {} };", None).unwrap();
    let again = App::new(data.clone(), "http://127.0.0.1:9".into(), "127.0.0.1:9".parse().unwrap(), None, None);
    for (id, provides) in [("finds", json!(["subtitles"])), ("translates", json!(["translate"])), ("all", json!(["subtitles"])), ("titles", json!(["content"]))] {
        assert_eq!(again.status(id).unwrap()["provides"], provides, "{id}");
    }
    let _ = std::fs::remove_dir_all(&data);
}

/// An addon with no titles never calls items() nor sends rex a snapshot: its sync registers it,
/// with where rex asks it for subtitles, and it is idle with no titles. One with titles too gets both.
#[test]
fn an_addon_without_titles_registers_for_subtitles_and_sends_no_titles() {
    let (rex, sent) = rex();
    let data = data("register");
    let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
    app.install(&manifest("subs"), "export default { subtitles() { return []; }, subtitle() {} };", None).unwrap();
    app.install(&manifest("both"), "export default { items() { return [{id: 'a', path: 'films/a.mp4'}]; }, open() {}, subtitles() {}, subtitle() {}, translate() {} };", None).unwrap();
    // Installed and never synced: due, as any addon is, so rex hears of it once it answers.
    assert!(app.due().contains(&("subs".to_string(), "install")));
    for id in ["subs", "both"] {
        assert!(app.start_sync(id, false, "install"));
        idle(&app, id);
    }
    let sent = sent.lock().unwrap().clone();
    let to = |path: &str| sent.iter().filter(|(url, _)| url == path).map(|(_, body)| body.clone()).collect::<Vec<_>>();
    assert_eq!(to("/rex/sources/addon-subs"), [json!({"url": "http://127.0.0.1:9/bytes/subs",
        "subtitles": {"url": "http://127.0.0.1:9/subtitles/subs", "search": true, "translate": false}})]);
    assert_eq!(to("/rex/sources/addon-both")[0]["subtitles"], json!({"url": "http://127.0.0.1:9/subtitles/both", "search": true, "translate": true}));
    assert!(sent.iter().all(|(url, _)| !url.starts_with("/rex/feed/addon-subs")), "{sent:?}");
    assert_eq!(to("/rex/feed/addon-both/snapshot").len(), 1);
    let status = app.status("subs").unwrap();
    assert_eq!((&status["state"], &status["items"], &status["error"], &status["warnings"]["count"]), (&json!("idle"), &json!(0), &Json::Null, &json!(0)));
    assert!(status["last_sync"].is_u64());
    assert!(crate::app::LOGGED.lock().unwrap().iter().any(|line| line == "[subs] registered with rex, for subtitles only"));

    // rex-addons check: what it can try of an addon with no titles works, the rest is a note.
    let folder = data.join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("manifest.json"), manifest("subs").to_string()).unwrap();
    std::fs::write(folder.join("main.js"), "export default { subtitles() { return []; }, subtitle() {} };").unwrap();
    assert_eq!(crate::check::run(&[folder.display().to_string()]), 0);
    let _ = std::fs::remove_dir_all(&data);
}
