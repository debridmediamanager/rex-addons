//! Addons that find and translate subtitles: what installing takes from them, what rex is told,
//! and the byte server's subtitle paths, driven through real sandboxed addons, a stub rex and a
//! stub subtitle website. The website answers on this machine's network address, as the sandbox
//! never fetches loopback.
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::app::App;

/// Each request rex was sent: its path and body.
type Sent = Arc<Mutex<Vec<(String, Json)>>>;

/// A rex that takes every registration and snapshot, and removes a source in its two steps.
fn rex() -> (String, Sent) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let sent: Sent = Arc::default();
    let recorded = sent.clone();
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let url = request.url().to_string();
            recorded.lock().unwrap().push((format!("{} {url}", request.method()).replace("GET ", "").replace("PUT ", "").replace("POST ", ""), serde_json::from_str(&body).unwrap_or(Json::Null)));
            let answer = match (request.method(), url.contains("confirm")) {
                (tiny_http::Method::Delete, false) => json!({"confirm": format!("{url}?confirm=yes")}),
                (tiny_http::Method::Delete, true) => json!({"removed": 1}),
                _ => json!({}),
            };
            let _ = request.respond(tiny_http::Response::from_string(answer.to_string()));
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

fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/subtitles").join(name)).unwrap()
}

/// A subtitle site: its API searches, its downloads redirect to a CDN path (as SubDL's links
/// are on another host than its API), one answers 429 once, one redirects off the addon's hosts.
const SITE: &str = r#"
const SITE = "{BASE}";
export default {
  async subtitles(query, ctx) {
    const answer = await fetch(`${SITE}/api/search?imdb=${query.imdb}&langs=${query.languages.join(",")}`);
    if (!answer.ok) throw new Error(`the website answered ${answer.status}`);
    return await answer.json();
  },
  async subtitle(id, ctx) {
    if (id === "inline") return { text: "1\n00:00:01,000 --> 00:00:02,000\nInline\n" };
    return { url: `${SITE}/dl/${encodeURIComponent(id)}` };
  },
};
"#;

struct Site {
    base: String,
    /// Requests by path.
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

fn site(lan: IpAddr) -> Site {
    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let base = format!("http://{lan}:{}", server.server_addr().to_ip().unwrap().port());
    let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    let counted = hits.clone();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url().to_string();
            let path = url.split('?').next().unwrap_or_default().to_string();
            let seen = {
                let mut hits = counted.lock().unwrap();
                *hits.entry(path.clone()).or_default() += 1;
                hits[&path]
            };
            let redirect = |to: &str| tiny_http::Response::empty(302).with_header(tiny_http::Header::from_bytes("Location", to).unwrap()).boxed();
            let data = |bytes: Vec<u8>| tiny_http::Response::from_data(bytes).boxed();
            let response = match path.as_str() {
                "/api/search" if url.contains("imdb=tt0000001") => {
                    // A website that never answers.
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(15));
                        let _ = request.respond(tiny_http::Response::empty(504));
                    });
                    continue;
                }
                "/api/search" => data(json!([
                    {"id": "good", "lang": "en", "release": "His.Girl.Friday.1940.1080p-720p.BluRay.x264-SiNNERS", "format": "srt", "provider": "SubDL"},
                    {"id": "pack", "lang": "en", "release": "Show.S01.720p.WEB"},
                    {"id": "a/b", "lang": "pt-br"},
                    {"id": "bad", "lang": "English"},
                ]).to_string().into_bytes()),
                "/dl/good" | "/dl/busy" if path == "/dl/busy" && seen == 1 => {
                    tiny_http::Response::empty(429).with_header(tiny_http::Header::from_bytes("Retry-After", "1").unwrap()).boxed()
                }
                "/dl/good" | "/dl/busy" => redirect("/cdn/578492-1497278.zip"),
                "/cdn/578492-1497278.zip" => data(fixture("subdl-578492-1497278.excerpt.zip")),
                "/dl/pack" => data(fixture("season-pack.FABRICATED.zip")),
                "/dl/a%2Fb" => data(fixture("his-girl-friday.excerpt.FABRICATED.srt.gz")),
                "/dl/evil" => redirect("http://evil.example/578492-1497278.zip"),
                _ => tiny_http::Response::empty(404).boxed(),
            };
            let _ = request.respond(response);
        }
    });
    Site { base, hits }
}

/// rex-addons on loopback with the site's addon installed as `subs` and an addon of titles as
/// `titles`, and a GET of its paths: status, body, and the subtitle format header.
fn served(name: &str, site: &Site, lan: IpAddr) -> (Arc<App>, SocketAddr) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let app = App::new(data(name), "http://127.0.0.1:9".into(), addr, None, None);
    let subs = json!({"id": "subs", "name": "Subs", "version": "1.0.0", "hosts": [lan.to_string()]});
    app.install(&subs, &SITE.replace("{BASE}", &site.base), None).unwrap();
    app.install(&manifest("titles"), "export default { items() { return []; }, open() {} };", None).unwrap();
    let serving = app.clone();
    std::thread::spawn(move || crate::server::serve(serving, server, None));
    (app, addr)
}

fn get(addr: SocketAddr, path: &str) -> (u16, String, Option<String>) {
    let response = match ureq::get(&format!("http://{addr}{path}")).call() {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(error) => panic!("{path}: {error}"),
    };
    let format = response.header("x-subtitle-format").map(str::to_string);
    (response.status(), response.into_string().unwrap(), format)
}

fn error_of(body: &str) -> String {
    serde_json::from_str::<Json>(body).unwrap()["error"].as_str().unwrap_or_default().to_string()
}

/// Search, then each kind of file: a ZIP behind a redirect, an id with a `/` in it, an episode
/// of a season's archive, a 429 waited out, text the addon returned itself; the same question
/// again from the cache; a redirect off the addon's hosts refused; and what no addon can answer.
#[test]
fn subtitles_are_found_and_fetched_through_the_byte_server() {
    let Some(lan) = lan_ip() else {
        eprintln!("SKIP: no network address for the subtitle site (loopback is never fetched)");
        return;
    };
    let site = site(lan);
    let (app, addr) = served("served", &site, lan);
    let hits = |path: &str| site.hits.lock().unwrap().get(path).copied().unwrap_or(0);

    let (status, body, _) = get(addr, "/subtitles/subs/search?type=movie&imdb=tt0032599&lang=en,pt-BR");
    assert_eq!(status, 200, "{body}");
    let found: Json = serde_json::from_str(&body).unwrap();
    assert_eq!(found["candidates"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect::<Vec<_>>(), ["good", "pack", "a/b"]);
    assert_eq!((&found["candidates"][2]["lang"], &found["left_out"], &found["truncated"]), (&json!("pt-BR"), &json!(1), &json!(false)));
    assert_eq!(get(addr, "/subtitles/subs/search?lang=EN&imdb=tt0032599&type=movie&lang=pt-br").1, body);
    assert_eq!(hits("/api/search"), 1, "the same question, asked again, answers from the cache");

    let (status, text, format) = get(addr, "/subtitles/subs/file/good");
    assert_eq!((status, format.as_deref()), (200, Some("srt")), "{text}");
    assert!(text.starts_with("1\r\n00:01:12,990 --> 00:01:16,200\r\n(chattering)"));
    assert_eq!(get(addr, "/subtitles/subs/file/good"), (200, text.clone(), Some("srt".into())));
    assert_eq!((hits("/dl/good"), hits("/cdn/578492-1497278.zip")), (1, 1));
    let (status, excerpt, _) = get(addr, "/subtitles/subs/file/a%2Fb");
    assert_eq!((status, excerpt.lines().nth(2)), (200, Some("(chattering)")));
    assert_eq!(get(addr, "/subtitles/subs/file/pack?season=1&episode=2").1.lines().nth(2), Some("episode two"));
    assert_eq!(error_of(&get(addr, "/subtitles/subs/file/pack?season=1&episode=9").1), "the ZIP holds no subtitle for S01E09");
    let started = Instant::now();
    assert_eq!(get(addr, "/subtitles/subs/file/busy?lang=en").1, text);
    assert!(started.elapsed() >= Duration::from_secs(1) && hits("/dl/busy") == 2, "the 429's Retry-After was waited out");
    assert_eq!(get(addr, "/subtitles/subs/file/inline"), (200, "1\n00:00:01,000 --> 00:00:02,000\nInline\n".into(), Some("srt".into())));

    let (status, body, _) = get(addr, "/subtitles/subs/file/evil");
    assert_eq!((status, error_of(&body)), (502, "evil.example is not in the addon's hosts".to_string()));
    for (path, want) in [
        ("/subtitles/subs/search?imdb=tt0032599", (400, "give type: movie or episode")),
        ("/subtitles/subs/file/good?season=1", (400, "give season and episode together")),
        ("/subtitles/titles/search?type=movie&imdb=tt0032599", (404, "titles finds no subtitles: its main.js exports no subtitles() and subtitle()")),
        ("/subtitles/nope/search?type=movie&imdb=tt0032599", (404, "no such addon")),
        ("/subtitles/subs/other", (404, "not found")),
    ] {
        let (status, body, _) = get(addr, path);
        assert_eq!((status, error_of(&body).as_str()), want, "{path}");
    }
    // New settings forget what was found and fetched with the old ones.
    assert!(app.data.join("addons/subs/cache/files").exists());
    app.set_config("subs", &json!({})).unwrap();
    assert!(!app.data.join("addons/subs/cache").exists());
    // Disabled, it answers nothing, cached or not; as the byte paths are, these are this device's
    // and private networks' only (the same rule, in server::handle).
    app.set_enabled("subs", false).unwrap();
    assert_eq!(get(addr, "/subtitles/subs/file/good").0, 409);
    assert_eq!(error_of(&get(addr, "/subtitles/subs/search?type=movie&imdb=tt0032599&lang=en,pt-BR").1), "the addon is disabled");
    let _ = std::fs::remove_dir_all(&app.data);
}

/// A website that never answers: the search ends at its deadline as a 504 that says so, never
/// as an empty list, and nothing is cached.
#[test]
fn a_search_ends_at_its_deadline() {
    let Some(lan) = lan_ip() else {
        eprintln!("SKIP: no network address for the subtitle site (loopback is never fetched)");
        return;
    };
    let site = site(lan);
    let (app, addr) = served("deadline", &site, lan);
    let started = Instant::now();
    let (status, body, _) = get(addr, "/subtitles/subs/search?type=movie&imdb=tt0000001");
    assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    assert_eq!(status, 504, "{body}");
    assert!(error_of(&body).contains("no time left, a call may take 8 s in all"), "{body}");
    assert!(!app.data.join("addons/subs/cache/search").exists());
    let _ = std::fs::remove_dir_all(&app.data);
}

fn post(addr: SocketAddr, path: &str, kind: &str, body: &str) -> (u16, Json) {
    let response = match ureq::post(&format!("http://{addr}{path}")).set("Content-Type", kind).send_string(body) {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(error) => panic!("{path}: {error}"),
    };
    (response.status(), serde_json::from_str(&response.into_string().unwrap()).unwrap())
}

/// A translation through the byte server: a JSON body, checked; a job polled until it's done,
/// one job of an addon at a time, the same lines asked for meanwhile joining their job, and
/// asked for again once done, answered from disk at once.
#[test]
fn a_translation_is_a_job_polled_until_done_one_at_a_time() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let app = App::new(data("translate"), "http://127.0.0.1:9".into(), addr, None, None);
    // Each call takes a moment and says when it ran; its Myanmar is the id after a word.
    let source = "export default { translate(b) {
        console.log('start ' + Date.now()); const end = Date.now() + 300; while (Date.now() < end); console.log('end ' + Date.now());
        return { lines: b.lines.map((l) => ({ id: l.id, text: 'မြန်မာ ' + l.id })) }; } };";
    app.install(&manifest("trans"), source, None).unwrap();
    app.install(&manifest("finds"), "export default { subtitles() { return []; }, subtitle() {} };", None).unwrap();
    let serving = app.clone();
    std::thread::spawn(move || crate::server::serve(serving, server, None));
    let ask = |ids: &[&str]| json!({"from": "en", "to": "my", "title": "His Girl Friday", "lines": ids.iter().map(|id| json!({"id": id, "text": format!("line {id}\nsecond row")})).collect::<Vec<_>>()}).to_string();
    let json = "application/json";

    assert_eq!(post(addr, "/subtitles/trans/translate", "text/plain", &ask(&["1"])).0, 415);
    assert_eq!(post(addr, "/subtitles/trans/translate", json, &ask(&["1", "1"])), (400, json!({"error": "line 1 is given twice"})));
    let huge = json!({"to": "my", "lines": [{"id": "1", "text": "x".repeat(3 << 20)}]}).to_string();
    assert_eq!(post(addr, "/subtitles/trans/translate", json, &huge).0, 413);
    assert_eq!(post(addr, "/subtitles/finds/translate", json, &ask(&["1"])), (404, json!({"error": "finds translates nothing: its main.js exports no translate()"})));

    let (status, first) = post(addr, "/subtitles/trans/translate", json, &ask(&["1", "2", "3"]));
    assert_eq!(status, 202, "{first}");
    let (status, second) = post(addr, "/subtitles/trans/translate", json, &ask(&["4"]));
    assert_eq!(status, 202, "{second}");
    assert_ne!(first["job"], second["job"]);
    assert_eq!(post(addr, "/subtitles/trans/translate", json, &ask(&["1", "2", "3"])), (202, first.clone()));
    let poll = |job: &Json| -> Json {
        let (_, body, _) = get(addr, &format!("/subtitles/trans/translate/{}", job["job"].as_str().unwrap()));
        serde_json::from_str(&body).unwrap()
    };
    assert_eq!(poll(&second)["total"], 1);
    let until_done = |job: &Json| {
        for _ in 0..200 {
            let answer = poll(job);
            if answer["state"] != "queued" && answer["state"] != "running" {
                return answer;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{job} never ended");
    };
    let done = until_done(&first);
    assert_eq!(done, json!({"job": first["job"], "state": "done", "done": 3, "total": 3, "kept": [],
        "lines": [{"id": "1", "text": "မြန်မာ 1"}, {"id": "2", "text": "မြန်မာ 2"}, {"id": "3", "text": "မြန်မာ 3"}]}));
    assert_eq!(until_done(&second)["lines"], json!([{"id": "4", "text": "မြန်မာ 4"}]));
    // One call at a time: each began after the one before it ended, as logged, in order.
    let ran: Vec<(String, u64)> = crate::app::LOGGED.lock().unwrap().iter().filter_map(|line| {
        let (kind, at) = line.strip_prefix("[addon trans] ")?.split_once(' ')?;
        Some((kind.to_string(), at.parse().ok()?))
    }).collect();
    assert_eq!(ran.iter().map(|(kind, _)| kind.as_str()).collect::<Vec<_>>(), ["start", "end", "start", "end"], "calls overlapped: {ran:?}");
    assert!(ran.windows(2).all(|pair| pair[0].1 <= pair[1].1), "{ran:?}");

    // Done: the same lines answer at once (200, not a job), from disk, also once the job is forgotten.
    assert_eq!(post(addr, "/subtitles/trans/translate", json, &ask(&["1", "2", "3"])), (200, done.clone()));
    app.jobs.forget("trans");
    assert_eq!(poll(&first), done);
    for job in ["0123456789abcdef0123456789abcdef", "..%2F..%2Fstate"] {
        let (status, body, _) = get(addr, &format!("/subtitles/trans/translate/{job}"));
        assert_eq!((status, error_of(&body)), (404, "no such translation: it ended over an hour ago, or never began".to_string()));
    }
    app.set_enabled("trans", false).unwrap();
    assert_eq!(post(addr, "/subtitles/trans/translate", json, &ask(&["5"])), (409, json!({"error": "the addon is disabled"})));
    let _ = std::fs::remove_dir_all(&app.data);
}

/// rex-addons on loopback with `source` installed as `id`, nothing else.
fn serve_one(name: &str, id: &str, source: &str) -> (Arc<App>, SocketAddr) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let app = App::new(data(name), "http://127.0.0.1:9".into(), addr, None, None);
    app.install(&manifest(id), source, None).unwrap();
    let serving = app.clone();
    std::thread::spawn(move || crate::server::serve(serving, server, None));
    (app, addr)
}

/// A web page never reaches the subtitle paths: not through DNS rebinding (a Host that is a
/// name, not an address), nor straight from the page (`<img>`, fetch), which browsers mark with
/// `Sec-Fetch-Site` or `Origin`. rex, on this device or the network, names an address.
#[test]
fn web_pages_cannot_ask_for_subtitles() {
    let (app, addr) = serve_one("pages", "finds", "export default { subtitles() { return [{id: 'a', lang: 'en'}]; }, subtitle() { return { text: '' }; } };");
    let path = format!("http://{addr}/subtitles/finds/search?type=movie&imdb=tt0032599");
    let asked = |headers: &[(&str, &str)]| {
        let request = headers.iter().fold(ureq::get(&path), |request, (name, value)| request.set(name, value));
        match request.call() {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response.status(),
            Err(error) => panic!("{error}"),
        }
    };
    let port = addr.port();
    for host in [format!("127.0.0.1:{port}"), format!("localhost:{port}"), "192.168.1.20:32410".into(), "[fd00::5]:32410".into()] {
        assert_eq!(asked(&[("Host", &host)]), 200, "{host}");
    }
    assert_eq!(asked(&[("Sec-Fetch-Site", "none")]), 200, "a person typing the address");
    for headers in [
        vec![("Host", format!("evil.example:{port}"))],
        vec![("Host", "nas.local:32410".to_string())],
        vec![("Sec-Fetch-Site", "cross-site".to_string())],
        vec![("Origin", "https://evil.example".to_string())],
    ] {
        let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
        assert_eq!(asked(&headers), 403, "{headers:?}");
    }
    let _ = std::fs::remove_dir_all(&app.data);
}

/// What a search, a fetch or a translation found with an addon's old settings, its old version,
/// or once it was removed, is never kept: one running across the change keeps nothing, and a
/// removed addon's folder stays gone. An update empties the cache, as new settings do.
#[test]
fn answers_from_before_a_change_are_not_kept() {
    let source = "export default {
        subtitles() { const end = Date.now() + 600; while (Date.now() < end); return [{id: 'a', lang: 'en'}]; },
        subtitle() { return { text: '1\\n00:00:01,000 --> 00:00:02,000\\nHi\\n' }; },
        translate(b) { const end = Date.now() + 600; while (Date.now() < end); return { lines: b.lines.map((l) => ({ id: l.id, text: 'de ' + l.text })) }; } };";
    let (app, _) = serve_one("changes", "slow", source);
    let folder = app.data.join("addons/slow");
    let query = "type=movie&imdb=tt0032599";
    let during = |change: &dyn Fn()| {
        let searching = { let app = app.clone(); std::thread::spawn(move || app.subtitle_search("slow", query)) };
        std::thread::sleep(Duration::from_millis(200));
        change();
        searching.join().unwrap()
    };
    // New settings while it searched: the answer is given, not kept.
    assert!(during(&|| { app.set_config("slow", &json!({})).unwrap(); }).is_ok());
    assert!(!folder.join("cache/search").exists());
    // Kept when nothing changes; an update empties it.
    app.subtitle_search("slow", query).unwrap();
    app.subtitle_file("slow", "a", "").unwrap();
    assert!(folder.join("cache/search").exists() && folder.join("cache/files").exists());
    app.install(&json!({"id": "slow", "name": "slow", "version": "1.0.1"}), source, None).unwrap();
    assert!(!folder.join("cache").exists());
    // A translation across new settings fails, keeping nothing.
    let body = json!({"to": "de", "lines": [{"id": "1", "text": "Hello there"}]}).to_string();
    let job = app.translate("slow", &body).unwrap().1["job"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(200));
    app.set_config("slow", &json!({})).unwrap();
    let mut answer = Json::Null;
    for _ in 0..200 {
        answer = app.translation("slow", &job).unwrap();
        if answer["state"] == "done" || answer["state"] == "error" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(answer["state"], "error", "{answer}");
    assert!(!folder.join("cache/translations").exists());
    // Removed while it searched: its folder stays gone.
    let _ = during(&|| { app.remove("slow").unwrap(); });
    assert!(!folder.exists(), "{:?}", std::fs::read_dir(&folder).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>()));
    let _ = std::fs::remove_dir_all(&app.data);
}

/// An addon updated from titles to subtitles only: rex forgets its titles, which nothing could
/// play any more, with the two-step delete a removal uses, once; then it registers for subtitles.
#[test]
fn an_update_to_subtitles_only_takes_its_titles_out_of_rex() {
    let (rex, sent) = rex();
    let data = data("to-subtitles");
    let app = App::new(data.clone(), rex, "127.0.0.1:9".parse().unwrap(), None, None);
    app.install(&manifest("films"), "export default { items() { return [{id: 'a', path: 'films/a.mp4'}]; }, open() {} };", None).unwrap();
    assert!(app.start_sync("films", true, "install"));
    idle(&app, "films");
    assert!(!app.addons.lock().unwrap()["films"].paths.is_empty());
    app.install(&json!({"id": "films", "name": "films", "version": "1.0.1"}), "export default { subtitles() { return []; }, subtitle() {} };", None).unwrap();
    sent.lock().unwrap().clear();
    for _ in 0..2 {
        assert!(app.start_sync("films", false, "install"));
        assert_eq!(idle(&app, "films")["error"], Json::Null);
    }
    let urls: Vec<String> = sent.lock().unwrap().iter().map(|(url, _)| url.clone()).collect();
    assert_eq!(urls, ["DELETE /rex/sources/addon-films", "DELETE /rex/sources/addon-films?confirm=yes", "/rex/sources/addon-films", "/rex/sources/addon-films"]);
    assert!(app.addons.lock().unwrap()["films"].paths.is_empty());
    assert_eq!(app.status("films").unwrap()["items"], 0);
    let _ = std::fs::remove_dir_all(&data);
}

/// A candidate's id comes from the client, percent-decoded: logged quoted, its line breaks
/// escaped, it can't write lines of its own into the log.
#[test]
fn a_candidate_id_cannot_forge_log_lines() {
    let (app, _) = serve_one("forged", "fails", "export default { subtitles() { return []; }, subtitle(id) { throw new Error('no such file'); } };");
    assert!(app.subtitle_file("fails", "x\nWARN [host] forged", "").is_err());
    let logged: Vec<String> = crate::app::LOGGED.lock().unwrap().iter().filter(|line| line.contains("forged")).cloned().collect();
    assert!(!logged.is_empty() && logged.iter().all(|line| !line.contains("\nWARN [host] forged") && line.contains(r#""x\nWARN [host] forged""#)), "{logged:?}");
    let _ = std::fs::remove_dir_all(&app.data);
}

/// At most MAX_QUEUED (16) translations wait for one addon, the running one counted: one more
/// is refused with 429, the same lines again still join their job.
#[test]
fn translations_waiting_for_an_addon_are_bounded() {
    let (app, _) = serve_one("queued", "busy", "export default { translate(b) { const end = Date.now() + 1500; while (Date.now() < end); return { lines: b.lines }; } };");
    let ask = |n: usize| json!({"to": "de", "lines": [{"id": "1", "text": format!("line {n}")}]}).to_string();
    for n in 0..16 {
        assert_eq!(app.translate("busy", &ask(n)).unwrap().0, 202, "{n}");
    }
    assert_eq!(app.translate("busy", &ask(16)).unwrap_err(), (429, "16 translations wait for this addon already: ask again once one is done".to_string()));
    assert_eq!(app.translate("busy", &ask(3)).unwrap().0, 202);
    // Disabled, what waits fails at once.
    app.set_enabled("busy", false).unwrap();
    let _ = std::fs::remove_dir_all(&app.data);
}
