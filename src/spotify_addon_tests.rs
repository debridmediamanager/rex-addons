//! The spotify addon against stand-ins for Spotify's sign-in (accounts.spotify.com), apresolve and
//! Spotify's own API (spclient), served from this machine's network address with the addon's links
//! pointed there (the sandbox never fetches loopback), and for the YouTube addon (`addons.call`), in
//! the sandbox it runs in, without the host's `spotify` global.
//!
//! Real: apresolve's answer (`tests/fixtures/spotify/apresolve-spclient.json`, 2026-11-14), whose
//! server names the stand-in's addresses replace.
//!
//! FABRICATED in the shape the owner's account was answered in on 2026-11-14, as no answer from a
//! real account is published: the device-flow and token answers (`*.FABRICATED.json`, codes and
//! tokens made up; the HTTP status of the waiting answers is RFC 8628's 400, not measured) and the
//! profile. FABRICATED in the structure the owner's account was answered in on 2026-11-11 from public
//! catalogue data, the ids made up: rootlist, playlist, collection paging, track, album and artist
//! metadata.
use std::collections::VecDeque;
use std::net::{IpAddr, TcpListener, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::app::{App, LOGGED};
use crate::net::Hosts;
use crate::probe::Prober;
use crate::sandbox::{self, Bridge, Env, Storage};

/// Every secret the stand-in hands out starts with this, so a leak is easy to find.
const SECRET: &str = "SECRET-";

fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn fixture(name: &str) -> Json {
    serde_json::from_str(match name {
        "apresolve" => include_str!("../tests/fixtures/spotify/apresolve-spclient.json"),
        "authorize" => include_str!("../tests/fixtures/spotify/device-authorize.FABRICATED.json"),
        "device" => include_str!("../tests/fixtures/spotify/token-device.FABRICATED.json"),
        "refresh" => include_str!("../tests/fixtures/spotify/token-refresh.FABRICATED.json"),
        _ => unreachable!(),
    })
    .unwrap()
}

const B62: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// A made-up Spotify id and its gid, from a number.
fn ids(n: u128) -> (String, String) {
    let gid = n.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c834) % (1u128 << 127);
    let (mut rest, mut id) = (gid, Vec::new());
    while rest > 0 {
        id.push(B62[(rest % 62) as usize]);
        rest /= 62;
    }
    while id.len() < 22 {
        id.push(b'0');
    }
    id.reverse();
    (String::from_utf8(id).unwrap(), format!("{gid:032x}"))
}

struct Track {
    n: u128,
    title: &'static str,
    artists: &'static [&'static str],
    album: u128,
    disc: u64,
    number: u64,
    ms: u64,
    isrc: &'static str,
}

/// Albums: (n, title, artist, year, month, day).
const ALBUMS: [(u128, &str, &str, u64, u64, u64); 3] = [
    (101, "A Night At The Opera (2011 Remaster)", "Queen", 1975, 11, 21),
    (102, "Random Access Memories", "Daft Punk", 2013, 5, 17),
    (103, "THE BOOK", "YOASOBI", 2021, 1, 6),
];

const TRACKS: [Track; 6] = [
    Track { n: 1, title: "Bohemian Rhapsody - Remastered 2011", artists: &["Queen"], album: 101, disc: 1, number: 11, ms: 354_320, isrc: "GBUM71029604" },
    Track { n: 2, title: "Love of My Life - Remastered 2011", artists: &["Queen"], album: 101, disc: 1, number: 9, ms: 218_773, isrc: "GBUM71029602" },
    Track { n: 3, title: "Get Lucky (feat. Pharrell Williams & Nile Rodgers)", artists: &["Daft Punk", "Pharrell Williams", "Nile Rodgers"], album: 102, disc: 1, number: 8, ms: 369_626, isrc: "USQX91300108" },
    Track { n: 4, title: "Instant Crush (feat. Julian Casablancas)", artists: &["Daft Punk", "Julian Casablancas"], album: 102, disc: 1, number: 5, ms: 337_560, isrc: "USQX91300105" },
    Track { n: 5, title: "夜に駆ける", artists: &["YOASOBI"], album: 103, disc: 1, number: 2, ms: 261_000, isrc: "JPP301900497" },
    Track { n: 6, title: "Giorgio by Moroder", artists: &["Daft Punk"], album: 102, disc: 1, number: 3, ms: 544_626, isrc: "USQX91300103" },
];

fn track(n: u128) -> &'static Track {
    TRACKS.iter().find(|t| t.n == n).unwrap()
}

fn uri(kind: &str, n: u128) -> String {
    format!("spotify:{kind}:{}", ids(n).0)
}

/// A gid's track or album, by the number it was made from.
fn by_gid(gid: &str) -> Option<u128> {
    (1..200).find(|&n| ids(n).1 == gid)
}

fn cover(n: u128) -> Json {
    json!({"image": [
        {"file_id": format!("ab67616d00001e02{:024x}", n), "size": "DEFAULT", "width": 300, "height": 300},
        {"file_id": format!("ab67616d00004851{:024x}", n), "size": "SMALL", "width": 64, "height": 64},
        {"file_id": format!("ab67616d0000b273{:024x}", n), "size": "LARGE", "width": 640, "height": 640},
    ]})
}

fn artist_ref(name: &str) -> Json {
    json!({"gid": ids(500 + name.len() as u128).1, "name": name})
}

/// spclient's answer to `GET path`, or to `POST path` with `body`: what the account holds, FABRICATED.
struct Account {
    playlist_one: Vec<String>,
    collection: Vec<(String, &'static str)>,
    gone: Option<u128>,
    /// Requests seen, as "GET /path" without the query.
    seen: Vec<String>,
}

fn playlist_contents(items: &[String], from: usize, length: usize, total: usize) -> Json {
    let window: Vec<Json> = items.iter().skip(from).take(length)
        .map(|uri| json!({"uri": uri, "attributes": {"addedBy": "", "timestamp": "1762800000000", "itemId": "AAAA"}})).collect();
    json!({"pos": from, "truncated": from + window.len() < total, "items": window})
}

impl Account {
    fn answer(&mut self, method: &str, path: &str, body: Option<&Json>) -> Result<Json, u16> {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        self.seen.push(format!("{method} {route}"));
        let param = |name: &str| url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
        let segments: Vec<&str> = route.split('/').collect();
        match (method, segments.as_slice()) {
            ("GET", ["", "playlist", "v2", "user", "someone", "rootlist"]) => {
                // A folder around the second playlist, as Spotify lists one, an empty playlist and one
                // gone private, as the owner's rootlist held (2026-11-11).
                let items = [uri("playlist", 11), "spotify:start-group:abc123:Folder".into(), uri("playlist", 12), "spotify:end-group:abc123".into(), uri("playlist", 13), uri("playlist", 14)];
                Ok(json!({"revision": "AAAA", "length": 6, "attributes": {}, "timestamp": "1762800000000", "contents": {
                    "pos": 0, "truncated": false,
                    "items": items.iter().map(|u| json!({"uri": u, "attributes": {"timestamp": "1762800000000", "public": false}})).collect::<Vec<_>>(),
                    "metaItems": [
                        {"revision": "r11", "attributes": {"name": "Road trip", "description": "", "picture": ""}, "length": self.playlist_one.len(), "timestamp": "1", "ownerUsername": "someone"},
                        {"revision": "r12", "attributes": {"name": "Road trip"}, "length": 1, "timestamp": "1", "ownerUsername": "someone"},
                        {"revision": "r13", "attributes": {"name": "Empty"}, "length": 0, "timestamp": "1", "ownerUsername": "someone"},
                        {},
                    ]}}))
            }
            // An empty playlist: contents without items, as Spotify answered two on 2026-11-11.
            ("GET", ["", "playlist", "v2", "playlist", id]) if *id == ids(13).0 => {
                Ok(json!({"revision": "r13", "length": 0, "attributes": {"name": "Empty"}, "timestamp": "1", "ownerUsername": "someone",
                          "contents": {"pos": 0, "truncated": false, "availableSignals": []}, "isUserCreated": true}))
            }
            // A followed playlist gone private: listed with no details, and refused.
            ("GET", ["", "playlist", "v2", "playlist", id]) if *id == ids(14).0 => Err(403),
            ("GET", ["", "playlist", "v2", "playlist", id]) => {
                let (items, name) = if *id == ids(11).0 { (self.playlist_one.clone(), "Road trip") } else { (vec![uri("track", 2)], "Road trip") };
                // The first answer holds two entries and says it is cut short; windows bring the rest.
                let from: usize = param("from").map_or(0, |f| f.parse().unwrap());
                let length = param("length").map_or(2, |l| l.parse().unwrap());
                if from > 0 && *id == ids(11).0 && self.gone == Some(99) {
                    return Ok(json!({"revision": "r11", "length": items.len(), "contents": {"pos": from, "truncated": false, "items": []}}));
                }
                Ok(json!({"revision": "r11", "length": items.len(), "attributes": {"name": name, "collaborative": false}, "timestamp": "1",
                    "ownerUsername": "someone", "contents": playlist_contents(&items, from, length, items.len())}))
            }
            ("POST", ["", "collection", "v2", "paging"]) => {
                let body = body.unwrap();
                assert_eq!((&body["username"], &body["set"]), (&json!("someone"), &json!("collection")));
                // Two to a page, as a limit of 2 would page them.
                let from: usize = body["pagination_token"].as_str().map_or(0, |t| t.trim_start_matches("page-").parse().unwrap());
                let page: Vec<Json> = self.collection.iter().skip(from).take(2).map(|(u, at)| json!({"uri": u, "added_at": at})).collect();
                let mut answer = json!({"items": page});
                if from + 2 < self.collection.len() {
                    answer["next_page_token"] = json!(format!("page-{}", from + 2));
                }
                Ok(answer)
            }
            ("GET", ["", "metadata", "4", "track", gid]) => {
                let n = by_gid(gid).ok_or(404u16)?;
                if self.gone == Some(n) {
                    return Err(404);
                }
                let t = track(n);
                let (_, album, artist, year, month, day) = ALBUMS.iter().find(|a| a.0 == t.album).unwrap();
                Ok(json!({
                    "gid": gid, "name": t.title,
                    "album": {"gid": ids(t.album).1, "name": album, "artist": [artist_ref(artist)], "type": "ALBUM", "label": "a label",
                              "date": {"year": year, "month": month, "day": day}, "cover_group": cover(t.album), "licensor": {"uuid": "x"}},
                    "artist": t.artists.iter().map(|a| artist_ref(a)).collect::<Vec<_>>(),
                    "number": t.number, "disc_number": t.disc, "duration": t.ms, "popularity": 50,
                    "external_id": [{"type": "isrc", "id": t.isrc}], "canonical_uri": uri("track", n), "media_type": "AUDIO",
                }))
            }
            ("GET", ["", "metadata", "4", "album", gid]) => {
                let n = by_gid(gid).ok_or(404u16)?;
                let tracks: Vec<Json> = TRACKS.iter().filter(|t| t.album == n).map(|t| json!({"gid": ids(t.n).1})).collect();
                Ok(json!({"gid": gid, "name": "an album", "disc": [{"number": 1, "track": tracks}], "cover_group": cover(n)}))
            }
            ("GET", ["", "metadata", "4", "artist", gid]) => {
                Ok(json!({"gid": gid, "name": "an artist", "portrait_group": {"image": [{"file_id": format!("ab6761610000e5eb{gid}")[..40].to_string(), "size": "LARGE", "width": 640, "height": 640}]}}))
            }
            // The account's profile: FABRICATED, its name made up.
            ("GET", ["", "user-profile-view", "v3", "profile", "someone"]) => Ok(json!({"uri": "spotify:user:someone", "name": "Someone Listening", "followers_count": 3})),
            _ => Err(404),
        }
    }
}

/// What the YouTube addon is asked, and answers: a recording of every track but YOASOBI's.
#[derive(Default)]
struct YouTube {
    asked: Vec<Json>,
    error: Option<String>,
}

impl YouTube {
    fn answer(&mut self, function: &str, args: Vec<Json>) -> Result<Json, String> {
        self.asked.push(json!([function, args]));
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        match function {
            "findSong" => {
                let want = &args[0];
                if want["artists"][0] == "YOASOBI" {
                    return Ok(Json::Null);
                }
                Ok(json!({"video_id": format!("vid{:0>8}", want["isrc"].as_str().unwrap()[4..].to_string()), "title": want["title"], "channel": want["artists"][0],
                    "duration_ms": want["duration_ms"].as_u64().unwrap() + 400, "kind": "song", "score": 0.94, "via": "isrc",
                    "size": 5_736_965, "bitrate": 129_532, "mime": "audio/mp4", "sample_rate": 44_100, "channels": 2}))
            }
            "songAudio" => Ok(json!({"url": format!("https://rr1---sn-x.googlevideo.com/videoplayback?id={}&itag=140", args[0].as_str().unwrap()), "mime": "audio/mp4", "bitrate": 129_532, "duration_ms": 1, "size": 1})),
            _ => Err(format!("YouTube exports no {function}()")),
        }
    }
}

/// Spotify's sign-in, apresolve and spclient, as one stand-in.
struct Site {
    account: Account,
    /// What the token endpoint answers the device grant: `authorization_pending`, `slow_down`,
    /// `expired_token`, `access_denied`, or "" once the person approved.
    device: &'static str,
    /// The interval the device-flow answer gives.
    interval: u64,
    /// Access tokens handed out; only the newest is accepted.
    issued: u32,
    refuse_refresh: bool,
    /// The token endpoint answers 429 (with Retry-After 30).
    token_limited: bool,
    /// The next spclient answers are 429, each with this Retry-After (None: none).
    limited: VecDeque<Option<&'static str>>,
    /// apresolve's servers, `host:port`.
    spclients: Vec<String>,
    /// A server (`host:port`) that answers 503.
    down: Option<String>,
    /// Every request: `METHOD /path` (sign-in ones with their form, apresolve as `GET /?type=spclient`)
    /// and its status.
    hits: Vec<(String, u16)>,
}

fn access(n: u32) -> String {
    format!("{SECRET}access-{n}")
}

impl Site {
    /// Status, body and Retry-After for one request to `server` (`host:port`).
    fn answer(&mut self, server: &str, method: &str, url: &str, auth: Option<&str>, text: &str) -> (u16, Json, Option<&'static str>) {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(text.as_bytes()).into_owned().collect();
        let field = |name: &str| form.get(name).map_or("", String::as_str);
        let token = |answer: &str, n: u32| {
            let mut answer = fixture(answer);
            answer["access_token"] = json!(access(n));
            answer
        };
        match (method, path) {
            ("POST", "/oauth2/device/authorize") => {
                assert_eq!((field("client_id"), field("scope")), ("65b708073fc0480ea92a077233ca87bd", "playlist-read-private,playlist-read-collaborative,user-library-read,user-read-private"));
                let mut answer = fixture("authorize");
                answer["interval"] = json!(self.interval);
                (200, answer, None)
            }
            ("POST", "/api/token") if self.token_limited => (429, json!({}), Some("30")),
            ("POST", "/api/token") => match field("grant_type") {
                "urn:ietf:params:oauth:grant-type:device_code" if field("device_code") == "SECRET-device-1" => match self.device {
                    "" => {
                        self.issued += 1;
                        (200, token("device", self.issued), None)
                    }
                    error => (400, json!({"error": error}), None),
                },
                "refresh_token" if !self.refuse_refresh && field("refresh_token") == "SECRET-refresh-1" => {
                    self.issued += 1;
                    (200, token("refresh", self.issued), None)
                }
                _ => (400, json!({"error": "invalid_grant", "error_description": "Refresh token revoked"}), None),
            },
            ("GET", "/") if query == "type=spclient" => {
                let mut answer = fixture("apresolve");
                answer["spclient"] = json!(self.spclients);
                (200, answer, None)
            }
            _ if self.down.as_deref() == Some(server) => (503, json!({}), None),
            _ if self.issued == 0 || auth != Some(format!("Bearer {}", access(self.issued)).as_str()) => (401, json!({"error": {"status": 401, "message": "Invalid token"}}), None),
            _ if !self.limited.is_empty() => (429, json!({}), self.limited.pop_front().unwrap()),
            _ => {
                let body: Option<Json> = serde_json::from_str(text).ok();
                match self.account.answer(method, url, body.as_ref()) {
                    Ok(answer) => (200, answer, None),
                    Err(status) => (status, json!({}), None),
                }
            }
        }
    }
}

/// A server on `lan` answering for `site`: its `host:port`.
fn listen(lan: IpAddr, site: &Arc<Mutex<Site>>) -> String {
    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let name = format!("{lan}:{}", server.server_addr().to_ip().unwrap().port());
    let (site, served) = (site.clone(), name.clone());
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut text = String::new();
            let _ = request.as_reader().read_to_string(&mut text);
            let auth = request.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string());
            let (method, url) = (request.method().to_string(), request.url().to_string());
            let mut site = site.lock().unwrap();
            let (status, body, retry) = site.answer(&served, &method, &url, auth.as_deref(), &text);
            let shown = match url.split('?').next().unwrap() {
                "/" => url.clone(),
                path if path.starts_with("/oauth2/") || path.starts_with("/api/") => format!("{path} {text}"),
                path => path.to_string(),
            };
            site.hits.push((format!("{method} {shown}"), status));
            drop(site);
            let mut response = tiny_http::Response::from_string(body.to_string()).with_status_code(status)
                .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap());
            if let Some(retry) = retry {
                response = response.with_header(tiny_http::Header::from_bytes("Retry-After", retry).unwrap());
            }
            let _ = request.respond(response);
        }
    });
    name
}

struct Rig {
    site: Arc<Mutex<Site>>,
    youtube: Arc<Mutex<YouTube>>,
    storage: Arc<Storage>,
    source: String,
    lan: IpAddr,
    /// The spclient server apresolve names first, and a second one.
    main: String,
    other: String,
    /// The log tag, one per test.
    tag: String,
}

/// The addon's source, its links pointed at `base` and its pauses taken out.
fn source(base: &str) -> String {
    let source = include_str!("../community/addons/spotify/main.js");
    for constant in ["const ACCOUNTS = \"https://accounts.spotify.com\";", "const APRESOLVE = \"https://apresolve.spotify.com/?type=spclient\";",
                     "`https://${host}${path}`", "const SPOTIFY_PAUSE_MS = 250;", "const MATCH_PAUSE_MS = 1500;"] {
        assert!(source.contains(constant), "main.js no longer has {constant}");
    }
    source.replace("https://accounts.spotify.com", base).replace("https://apresolve.spotify.com", base)
        .replace("`https://${host}${path}`", "`http://${host}${path}`")
        .replace("const SPOTIFY_PAUSE_MS = 250;", "const SPOTIFY_PAUSE_MS = 0;").replace("const MATCH_PAUSE_MS = 1500;", "const MATCH_PAUSE_MS = 0;")
}

fn rig(name: &str) -> Option<Rig> {
    let lan = lan_ip()?;
    let storage = std::env::temp_dir().join(format!("rex-addons-spotify-{name}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&storage);
    let site = Arc::new(Mutex::new(Site {
        account: Account {
            // Two tracks, a local file, a podcast episode, the first track again and one more.
            playlist_one: vec![uri("track", 1), uri("track", 3), "spotify:local:Artist:Album:Home+demo:241".into(), uri("episode", 40), uri("track", 1), uri("track", 4)],
            collection: vec![(uri("album", 102), "1700000000"), (uri("track", 1), "1700000200"), (uri("track", 5), "1700000300")],
            gone: None,
            seen: Vec::new(),
        },
        device: "authorization_pending", interval: 5, issued: 0, refuse_refresh: false, token_limited: false,
        limited: VecDeque::new(), spclients: Vec::new(), down: None, hits: Vec::new(),
    }));
    let (main, other) = (listen(lan, &site), listen(lan, &site));
    site.lock().unwrap().spclients = vec![main.clone()];
    let base = format!("http://{main}");
    Some(Rig {
        site, youtube: Arc::default(), storage: Arc::new(Storage::open(storage)), source: source(&base), lan, main, other,
        tag: format!("spotify-{name}"),
    })
}

macro_rules! rig {
    ($name:expr) => {
        match rig($name) {
            Some(rig) => rig,
            None => return eprintln!("skipped: no network address for the stand-in"),
        }
    };
}

impl Rig {
    fn call(&self, method: &str, args: &[Json]) -> Result<Json, String> {
        self.run(&self.source, method, args)
    }

    fn run(&self, source: &str, method: &str, args: &[Json]) -> Result<Json, String> {
        let youtube = self.youtube.clone();
        let calls: Bridge = Arc::new(move |id: &str, function: &str, args: Vec<Json>| {
            assert_eq!(id, "youtube");
            youtube.lock().unwrap().answer(function, args)
        });
        let env = Env {
            addon: &self.tag, source, hosts: Hosts::from(vec![self.lan.to_string()]), storage: self.storage.clone(),
            prober: Arc::new(Prober::new(None, "127.0.0.1:9".parse().unwrap())), calls: Some(calls),
        };
        let limits = if method == "items" { sandbox::ITEMS } else { sandbox::OPEN };
        let result = sandbox::call(&env, method, args, limits);
        if let Err(error) = &result {
            assert!(!error.contains(SECRET), "a secret in an error: {error}");
        }
        result
    }

    fn items(&self, config: Json) -> Result<Vec<Json>, String> {
        Ok(self.call("items", &[json!({"config": config})])?.as_array().unwrap().clone())
    }

    /// `js` run with the addon's storage.
    fn js(&self, js: &str) -> Json {
        self.run(&format!("export default {{ async items() {{ {js} }}, open() {{}} }};"), "items", &[]).unwrap()
    }

    fn stored(&self, key: &str) -> Option<Json> {
        Some(self.js(&format!("return storage.get({key:?}) ?? null;"))).filter(|v| !v.is_null())
    }

    /// Signed in before, its access token still good.
    fn sign_in(&self) {
        self.site.lock().unwrap().issued = 1;
        self.js(&format!("storage.set('tokens', {{access: '{}', refresh: 'SECRET-refresh-1', expires_at: 4000000000000, user: 'someone'}}); return null;", access(1)));
    }

    /// The requests since the last call, with their statuses.
    fn hits(&self) -> Vec<(String, u16)> {
        std::mem::take(&mut self.site.lock().unwrap().hits)
    }

    /// This test's log lines, none of which may hold a secret.
    fn logged(&self) -> Vec<String> {
        let tag = format!("[addon {}]", self.tag);
        let lines: Vec<String> = LOGGED.lock().unwrap().iter().filter(|line| line.contains(&tag)).cloned().collect();
        assert!(!lines.iter().any(|line| line.contains(SECRET)), "a secret in the log: {lines:?}");
        lines
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.logged();
        }
    }
}

fn ids_of(items: &[Json]) -> Vec<String> {
    items.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect()
}

fn item_id(n: u128) -> String {
    format!("sp-{}", ids(n).0)
}

fn paths(hits: &[(String, u16)]) -> Vec<String> {
    hits.iter().map(|(hit, status)| format!("{status} {}", hit.split(' ').take(2).collect::<Vec<_>>().join(" "))).collect()
}

/// The library listed (folders, windows of a long playlist, the collection's pages), each track
/// once with Spotify's details and its place in each playlist, only once YouTube has its recording,
/// a bounded number looked for each sync, and a track YouTube has none of kept from being asked again.
#[test]
fn the_library_becomes_matched_tracks_with_spotifys_details_and_playlists() {
    let rig = rig!("library");
    rig.sign_in();
    let first = rig.items(json!({"matches_per_sync": 3})).unwrap();
    // Liked Songs (newest first: YOASOBI, Bohemian Rhapsody), then the playlists, then the saved
    // album's tracks: the first three are looked for, and YouTube has no YOASOBI.
    assert_eq!(ids_of(&first), [item_id(1), item_id(3)]);
    let asked: Vec<Json> = std::mem::take(&mut rig.youtube.lock().unwrap().asked);
    assert_eq!(asked.len(), 3);
    assert_eq!(asked[1], json!(["findSong", [{"title": "Bohemian Rhapsody - Remastered 2011", "artists": ["Queen"],
        "album": "A Night At The Opera (2011 Remaster)", "duration_ms": 354_320, "isrc": "GBUM71029604"}]]));
    assert_eq!(rig.stored(&format!("m:{}", ids(5).0)).unwrap()["none"], true);

    let bohemian = &first[0];
    let album = ids(101).0;
    assert_eq!(bohemian["path"], format!("spotify/{album}/{}.m4a", ids(1).0));
    assert_eq!((&bohemian["kind"], &bohemian["disc"], &bohemian["track"], &bohemian["size"]), (&json!("track"), &json!(1), &json!(11), &json!(5_736_965)));
    assert_eq!(bohemian["display"], json!({
        "title": "Bohemian Rhapsody - Remastered 2011", "date": "1975-11-21",
        "artist": {"title": "Queen", "poster": format!("https://i.scdn.co/image/{}", &format!("ab6761610000e5eb{}", ids(505).1)[..40])},
        "album": {"title": "A Night At The Opera (2011 Remaster)", "date": "1975-11-21", "poster": format!("https://i.scdn.co/image/ab67616d0000b273{:024x}", 101)},
    }));
    // Its place in each list it is in: first of Road trip though it is there twice, second liked.
    assert_eq!(bohemian["playlists"], json!({"Liked Songs": 2, "Road trip": 1}));
    // What rex is told of the file: YouTube's AAC as the YouTube addon read it off its player answer.
    assert_eq!(bohemian["media_info"], json!({
        "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "nb_streams": 1, "duration": "354.720000", "size": "5736965", "bit_rate": "129532"},
        "streams": [{"index": 0, "codec_type": "audio", "codec_name": "aac", "profile": "LC", "codec_tag_string": "mp4a", "sample_rate": "44100",
                     "channels": 2, "channel_layout": "stereo", "duration": "354.720000", "bit_rate": "129532", "disposition": {"default": 1}}],
    }));

    // The next sync looks for the rest, and not for YOASOBI again; nothing listed twice is looked up twice.
    let seen_before = rig.site.lock().unwrap().account.seen.len();
    let second = rig.items(json!({})).unwrap();
    let asked: Vec<Json> = std::mem::take(&mut rig.youtube.lock().unwrap().asked);
    assert_eq!(asked.iter().map(|a| a[1][0]["title"].as_str().unwrap()).collect::<Vec<_>>(),
        ["Instant Crush (feat. Julian Casablancas)", "Love of My Life - Remastered 2011", "Giorgio by Moroder"]);
    assert_eq!(ids_of(&second), [item_id(1), item_id(3), item_id(4), item_id(2), item_id(6)]);
    // Two playlists of one name are two rex playlists; the second's revision unchanged, it is not read again.
    assert_eq!(second[3]["playlists"], json!({"Road trip (2)": 1}));
    // Places count what reaches rex's lists: the local file and the episode are left out.
    assert_eq!(second[2]["playlists"], json!({"Road trip": 4}));
    assert_eq!(second[4]["playlists"], json!({}));
    let seen: Vec<String> = rig.site.lock().unwrap().account.seen[seen_before..].to_vec();
    assert!(!seen.iter().any(|s| s.starts_with("GET /playlist/v2/playlist/")), "{seen:?}");
    assert_eq!(seen.iter().filter(|s| s.starts_with("GET /metadata/4/track/")).count(), 3, "{seen:?}");

    // Saved albums turned off: the album's own track leaves, and is forgotten.
    let third = rig.items(json!({"albums": false})).unwrap();
    assert_eq!(ids_of(&third), [item_id(1), item_id(3), item_id(4), item_id(2)]);
    assert!(rig.stored(&format!("m:{}", ids(6).0)).is_none() && rig.stored(&format!("t:{}", ids(6).0)).is_none());
    assert!(rig.stored(&format!("al:{}", ids(102).0)).is_none());

    // Played: a fresh link to the recording's sound each time.
    let opened = rig.call("open", &[json!(item_id(1)), json!({"config": {}})]).unwrap();
    assert!(opened["url"].as_str().unwrap().starts_with("https://rr1---sn-x.googlevideo.com/videoplayback?id=vid"), "{opened}");
    assert_eq!(rig.youtube.lock().unwrap().asked.last().unwrap()[0], "songAudio");
    let error = rig.call("open", &[json!(item_id(5)), json!({"config": {}})]).unwrap_err();
    assert!(error.contains("has no recording on YouTube"), "{error}");
}

/// The YouTube addon missing or signed out fails the sync with what to do, keeping what rex has;
/// any other YouTube trouble stops the looking and sends what is matched; a playlist Spotify cuts
/// short fails the sync rather than send half of it; a track Spotify no longer has is passed over.
#[test]
fn trouble_fails_the_sync_or_stops_the_looking() {
    let rig = rig!("trouble");
    rig.sign_in();
    rig.youtube.lock().unwrap().error = Some("the addon youtube is not installed: install it and sign in to it first".into());
    assert!(rig.items(json!({})).unwrap_err().starts_with("the addon youtube is not installed: install it and sign in to it first"));
    rig.youtube.lock().unwrap().error = Some("sign in to YouTube again: YouTube refused the sign-in".into());
    assert!(rig.items(json!({})).unwrap_err().starts_with("sign in to YouTube again"));

    rig.youtube.lock().unwrap().error = None;
    rig.items(json!({"matches_per_sync": 2})).unwrap();
    rig.youtube.lock().unwrap().error = Some("YouTube: YouTube answered 429 (too many requests from this address); the next sync tries again".into());
    let items = rig.items(json!({})).unwrap();
    assert_eq!(ids_of(&items), [item_id(1)]);
    // One ask each for the two failed syncs, two, then the one refused.
    assert_eq!(rig.youtube.lock().unwrap().asked.len(), 1 + 1 + 2 + 1);

    rig.youtube.lock().unwrap().error = None;
    rig.site.lock().unwrap().account.gone = Some(4);
    let items = rig.items(json!({})).unwrap();
    assert_eq!(ids_of(&items), [item_id(1), item_id(3), item_id(2), item_id(6)]);
    assert_eq!(rig.stored(&format!("m:{}", ids(4).0)).unwrap()["none"], true);

    // A playlist read again whose window comes back empty.
    rig.js(&format!("storage.delete('pl:{}'); return null;", ids(11).0));
    rig.site.lock().unwrap().account.gone = Some(99);
    let error = rig.items(json!({})).unwrap_err();
    assert!(error.contains("with nothing"), "{error}");
}

/// A sign-in: the code shown until the person enters it at spotify.com/pair, a slow_down obeyed
/// (RFC 8628: 5 s more between polls), then the tokens and the canonical username kept and the
/// account's name shown.
#[test]
fn a_code_signs_in_once_entered_and_the_tokens_and_username_are_kept() {
    let rig = rig!("sign-in");
    let started = rig.call("signIn", &[json!({"config": {}})]).unwrap();
    assert_eq!(started, json!({"code": "ABC123", "url": "https://spotify.com/pair", "expires_in": 3599, "interval": 5, "state": {"device_code": "SECRET-device-1"}}));
    let state = started["state"].clone();
    let poll = || rig.call("signInPoll", &[state.clone(), json!({"config": {}})]);

    assert_eq!(poll().unwrap(), json!({"done": false}));
    rig.site.lock().unwrap().device = "slow_down";
    assert_eq!(poll().unwrap(), json!({"done": false}));
    assert_eq!(rig.stored("poll").unwrap()["every"], 5000);
    let hits = rig.hits();
    assert_eq!(paths(&hits), ["200 POST /oauth2/device/authorize", "400 POST /api/token", "400 POST /api/token"]);
    assert!(hits[1].0.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"), "{hits:?}");
    // The next poll within those 5 s asks Spotify nothing.
    rig.site.lock().unwrap().device = "";
    assert_eq!(poll().unwrap(), json!({"done": false}));
    assert!(rig.hits().is_empty());

    rig.js("storage.set('poll', {...storage.get('poll'), next: 0}); return null;");
    assert_eq!(poll().unwrap(), json!({"done": true, "account": "Someone Listening"}));
    assert_eq!(paths(&rig.hits()), ["200 POST /api/token", "200 GET /?type=spclient", "200 GET /user-profile-view/v3/profile/someone"]);
    let tokens = rig.stored("tokens").unwrap();
    assert_eq!((&tokens["access"], &tokens["refresh"], &tokens["user"]), (&json!(access(1)), &json!("SECRET-refresh-1"), &json!("someone")));
    assert!(rig.stored("poll").is_none());

    // Signed out: the tokens are forgotten (Spotify has no revoke endpoint), and nothing is asked.
    rig.call("signOut", &[json!({"config": {}})]).unwrap();
    assert!(rig.stored("tokens").is_none());
    assert!(rig.hits().is_empty());
}

/// A code that expired, or a sign-in refused on Spotify, ends the sign-in with why; a 429 waits.
#[test]
fn an_expired_or_refused_code_ends_the_sign_in() {
    let rig = rig!("expired");
    let state = rig.call("signIn", &[json!({"config": {}})]).unwrap()["state"].clone();
    // A 429 from the sign-in is taken as a slow_down.
    rig.site.lock().unwrap().token_limited = true;
    assert_eq!(rig.call("signInPoll", &[state.clone(), json!({"config": {}})]).unwrap(), json!({"done": false}));
    assert_eq!(rig.stored("poll").unwrap()["every"], 5000);
    rig.js("storage.set('poll', {...storage.get('poll'), next: 0}); return null;");
    rig.site.lock().unwrap().token_limited = false;
    rig.site.lock().unwrap().device = "expired_token";
    let error = rig.call("signInPoll", &[state.clone(), json!({"config": {}})]).unwrap_err();
    assert!(error.starts_with("the code expired before it was entered"), "{error}");
    rig.site.lock().unwrap().device = "access_denied";
    let error = rig.call("signInPoll", &[state, json!({"config": {}})]).unwrap_err();
    assert!(error.starts_with("the sign-in was refused on Spotify"), "{error}");
    assert!(rig.stored("tokens").is_none());
}

/// The access token is refreshed before it expires and when Spotify refuses it, the refresh token
/// and username kept from the sign-in; a refresh refused with invalid_grant signs the addon out.
#[test]
fn the_token_is_refreshed_before_expiry_and_on_a_401_and_a_refused_refresh_signs_out() {
    let rig = rig!("refresh");
    rig.sign_in();
    rig.items(json!({"playlists": false, "albums": false})).unwrap();
    assert!(!rig.hits().iter().any(|(hit, _)| hit.starts_with("POST /api/token")));

    // Expired: refreshed before the first request.
    rig.js("storage.set('tokens', {...storage.get('tokens'), expires_at: 0}); return null;");
    rig.items(json!({"playlists": false, "albums": false})).unwrap();
    let hits = rig.hits();
    assert_eq!(paths(&hits)[..2], ["200 POST /api/token", "200 POST /collection/v2/paging"]);
    assert!(hits[0].0.contains("grant_type=refresh_token") && hits[0].0.contains("client_id=65b708073fc0480ea92a077233ca87bd"), "{hits:?}");
    let tokens = rig.stored("tokens").unwrap();
    assert_eq!((&tokens["access"], &tokens["refresh"], &tokens["user"]), (&json!(access(2)), &json!("SECRET-refresh-1"), &json!("someone")));
    assert!(tokens["expires_at"].as_u64().unwrap() > crate::app::now() * 1000 + 3500 * 1000);

    // Refused before it expired (Spotify moved on): refreshed once and asked again.
    rig.site.lock().unwrap().issued += 1;
    rig.items(json!({"playlists": false, "albums": false})).unwrap();
    assert_eq!(paths(&rig.hits())[..3], ["401 POST /collection/v2/paging", "200 POST /api/token", "200 POST /collection/v2/paging"]);

    // Spotify ended the sign-in: the refresh is refused, and the addon is signed out.
    rig.site.lock().unwrap().refuse_refresh = true;
    rig.js("storage.set('tokens', {...storage.get('tokens'), expires_at: 0}); return null;");
    let error = rig.items(json!({})).unwrap_err();
    assert!(error.starts_with("SIGN_IN_REQUIRED: Spotify ended the sign-in (Refresh token revoked)"), "{error}");
}

/// A short 429 is waited out within the sync; a long one, or one that says nothing, or a second,
/// ends it with RETRY_AFTER, which the host waits out.
#[test]
fn a_429_is_waited_out_or_ends_the_sync_with_retry_after() {
    let rig = rig!("429");
    rig.sign_in();
    let quick = json!({"playlists": false, "albums": false});
    rig.site.lock().unwrap().limited = VecDeque::from([Some("1")]);
    rig.items(quick.clone()).unwrap();
    assert_eq!(paths(&rig.hits())[..3], ["200 GET /?type=spclient", "429 POST /collection/v2/paging", "200 POST /collection/v2/paging"]);

    for (limited, wanted) in [
        (vec![Some("3600")], "RETRY_AFTER 3600: Spotify answered 429"),
        (vec![None], "RETRY_AFTER 60: Spotify answered 429"),
        (vec![Some("1"), Some("1")], "RETRY_AFTER 60: Spotify answered 429"),
    ] {
        rig.site.lock().unwrap().limited = limited.into();
        let error = rig.items(quick.clone()).unwrap_err();
        assert!(error.starts_with(wanted), "{error}");
        assert!(rig.site.lock().unwrap().limited.is_empty());
    }
}

/// apresolve's servers are tried in turn: one that cannot be reached or answers 5xx is passed
/// over, and the one that answered is asked first from then on; when none answers, apresolve is
/// asked again next time.
#[test]
fn a_spclient_server_that_fails_is_passed_over() {
    let rig = rig!("fallback");
    rig.sign_in();
    let closed = TcpListener::bind((rig.lan, 0)).unwrap().local_addr().unwrap().port();
    {
        let mut site = rig.site.lock().unwrap();
        site.spclients = vec![format!("{}:{closed}", rig.lan), rig.other.clone(), rig.main.clone()];
        site.down = Some(rig.other.clone());
    }
    let quick = json!({"playlists": false, "albums": false});
    rig.items(quick.clone()).unwrap();
    assert_eq!(paths(&rig.hits())[..3], ["200 GET /?type=spclient", "503 POST /collection/v2/paging", "200 POST /collection/v2/paging"]);
    assert_eq!(rig.stored("spclient").unwrap()["at"], 2);
    assert_eq!(rig.logged().iter().filter(|line| line.contains("Spotify's server failed")).count(), 2);

    rig.items(quick.clone()).unwrap();
    assert_eq!(paths(&rig.hits())[0], "200 POST /collection/v2/paging");
    assert_eq!(rig.logged().iter().filter(|line| line.contains("Spotify's server failed")).count(), 2);

    // None answers: the sync fails, and the list is asked for again.
    rig.js(&format!("storage.set('spclient', {{...storage.get('spclient'), hosts: ['{}:{closed}', '{}'], at: 0}}); return null;", rig.lan, rig.other));
    let error = rig.items(quick.clone()).unwrap_err();
    assert!(error.starts_with("no Spotify server answered /collection/v2/paging"), "{error}");
    assert!(rig.stored("spclient").is_none());
    rig.hits();
    rig.items(quick).unwrap();
    assert_eq!(paths(&rig.hits()).iter().filter(|hit| hit.ends_with("GET /?type=spclient")).count(), 1);
}

/// An install signed in by 1.x's pairing, whose sign-in the host kept: no tokens in storage, so the
/// first sync after the update signs it out with what to do, asking Spotify nothing and keeping
/// what the addon knows of the library.
#[test]
fn an_install_signed_in_by_pairing_signs_in_again_after_the_update() {
    let rig = rig!("update");
    rig.js("storage.set('m:abc', {video_id: 'v'}); return null;");
    let error = rig.items(json!({})).unwrap_err();
    assert_eq!(error.lines().next().unwrap(), "SIGN_IN_REQUIRED: this update signs in to Spotify with a code");
    assert!(rig.hits().is_empty());
    assert_eq!(rig.stored("m:abc").unwrap(), json!({"video_id": "v"}));
}

/// Through the host: the addon installs as a code sign-in, the code shows until approved, the
/// account is kept and synced, and no token or device code is in the log, the status or an answer.
#[test]
fn signed_in_through_the_host_with_no_secret_logged_or_shown() {
    let rig = rig!("host");
    rig.site.lock().unwrap().interval = 2;
    let rex = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let rex_url = format!("http://{}", rex.server_addr().to_ip().unwrap());
    std::thread::spawn(move || {
        for request in rex.incoming_requests() {
            let _ = request.respond(tiny_http::Response::from_string("{}"));
        }
    });
    let api = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let address = api.server_addr().to_ip().unwrap();
    let data = std::env::temp_dir().join(format!("rex-addons-spotify-host-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let app = App::new(data.clone(), rex_url, address, None, None);
    let served = app.clone();
    std::thread::spawn(move || crate::server::serve(served, api, None));
    let youtube = json!({"id": "youtube", "name": "YouTube", "version": "1.0.0", "host": "0.5", "exports": ["findSong", "songAudio"]});
    app.install(&youtube, "export default { items() { return []; }, open() {}, findSong() { return null; }, songAudio() { return null; } };", None).unwrap();
    let mut manifest: Json = serde_json::from_str(include_str!("../community/addons/spotify/manifest.json")).unwrap();
    assert_eq!((&manifest["version"], &manifest["sign_in"], &manifest["host"]), (&json!("2.0.0"), &json!({"label": "Sign in to Spotify"}), &json!("0.5")));
    // Its hosts reach Spotify's sign-in, apresolve and every spclient server apresolve named.
    let hosts = Hosts::from(serde_json::from_value::<Vec<String>>(manifest["hosts"].clone()).unwrap());
    let named = fixture("apresolve")["spclient"].as_array().unwrap().iter().map(|h| h.as_str().unwrap().trim_end_matches(":443").to_string()).collect::<Vec<_>>();
    for host in ["accounts.spotify.com", "apresolve.spotify.com"].into_iter().chain(named.iter().map(String::as_str)) {
        assert!(hosts.allows(host, 443), "{host}");
    }
    manifest["hosts"] = json!([rig.lan.to_string()]);
    app.install(&manifest, &rig.source, None).unwrap();

    let call = |method: &str, path: &str| -> Json {
        let answer = match ureq::request(method, &format!("http://{address}{path}")).call() {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{e}"),
        };
        let text = answer.into_string().unwrap();
        assert!(!text.contains(SECRET), "a secret in {path}: {text}");
        serde_json::from_str(&text).unwrap_or(Json::Null)
    };
    let until = |what: &str, done: &dyn Fn(&Json) -> bool| -> Json {
        for _ in 0..600 {
            let status = call("GET", "/api/addons/spotify");
            if done(&status) {
                return status;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{what} never happened: {}", call("GET", "/api/addons/spotify"));
    };
    let started = call("POST", "/api/addons/spotify/sign-in");
    assert_eq!((&started["sign_in"]["kind"], &started["sign_in"]["pending"]["code"], &started["sign_in"]["pending"]["url"]),
        (&json!("code"), &json!("ABC123"), &json!("https://spotify.com/pair")));
    until("a poll", &|_| rig.site.lock().unwrap().hits.iter().any(|(hit, _)| hit.starts_with("POST /api/token")));
    rig.site.lock().unwrap().device = "";
    let status = until("the sync after signing in", &|s| s["sign_in"]["account"].is_string() && s["state"] == "idle" && s["last_sync"].is_u64());
    assert_eq!((&status["sign_in"]["account"], &status["error"]), (&json!("Someone Listening"), &Json::Null));
    call("GET", "/api/status");
    call("GET", "/api/addons/spotify/log");
    let lines: Vec<String> = LOGGED.lock().unwrap().iter().filter(|line| line.contains("[addon spotify]")).cloned().collect();
    assert!(lines.iter().any(|line| line.contains("library: ")), "{lines:?}");
    assert!(!LOGGED.lock().unwrap().iter().any(|line| line.contains(SECRET)), "a secret in the log");
    let _ = std::fs::remove_dir_all(&data);
}
