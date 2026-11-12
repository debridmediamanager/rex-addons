//! The stremio addon, and fetch itself, against real addons' recorded answers (tests/fixtures/stremio.json), served
//! from this machine's network address: the sandbox never fetches loopback, so the test's
//! manifest names that address as its one host.
use std::collections::HashSet;
use std::net::{IpAddr, UdpSocket};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as Json};

use crate::net::Hosts;
use crate::probe::Prober;
use crate::sandbox::{self, Env, Storage};

fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

struct Fixture {
    base: String,
    hosts: Hosts,
    /// Links that answer 404, as an expired debrid link does.
    dead: Arc<Mutex<HashSet<String>>>,
    storage: Arc<Storage>,
    prober: Arc<Prober>,
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}

fn serve() -> Option<Fixture> {
    let lan = lan_ip()?;
    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let base = format!("http://{lan}:{}", server.server_addr().to_ip().unwrap().port());
    let routes: Json = serde_json::from_str(include_str!("../tests/fixtures/stremio.json")).unwrap();
    let dead: Arc<Mutex<HashSet<String>>> = Arc::default();
    let (served_base, served_dead) = (base.clone(), dead.clone());
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let path = percent_encoding::percent_decode_str(request.url()).decode_utf8_lossy().into_owned();
            let referer = request.headers().iter().any(|h| h.field.equiv("Referer"));
            let response = if let Some(answer) = routes["routes"].get(&path) {
                tiny_http::Response::from_string(answer.to_string().replace("{BASE}", &served_base))
                    .with_header(header("Content-Type", "application/json"))
            } else if served_dead.lock().unwrap().contains(&path) {
                tiny_http::Response::from_string("gone").with_status_code(404)
            } else if (path.starts_with("/video/") || path.starts_with("/hdr/")) && (path.starts_with("/video/") || referer) {
                // A 5 MB file answering a Range as a debrid host does.
                tiny_http::Response::from_data(vec![0u8])
                    .with_status_code(206)
                    .with_header(header("Content-Range", "bytes 0-0/5242880"))
                    .with_header(header("Content-Type", "video/x-matroska"))
            } else {
                tiny_http::Response::from_string("no").with_status_code(403)
            };
            let _ = request.respond(response);
        }
    });
    let path = std::env::temp_dir().join(format!("rex-addons-stremio-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let prober = Arc::new(Prober::new(None, "127.0.0.1:9".parse().unwrap()));
    Some(Fixture { hosts: Hosts::from(vec![lan.to_string()]), base, dead, storage: Arc::new(Storage::open(path)), prober })
}

impl Fixture {
    fn call(&self, method: &str, args: &[Json]) -> Result<Json, String> {
        let env = Env {
            addon: "stremio",
            source: include_str!("../addons/stremio/main.js"),
            hosts: self.hosts.clone(),
            storage: self.storage.clone(),
            prober: self.prober.clone(),
            spotify: None,
            calls: None,
        };
        sandbox::call(&env, method, args, sandbox::ITEMS)
    }

    fn config(&self, extra: Json) -> Json {
        let mut config = json!({
            "addons": [format!("{}/cinemeta/manifest.json", self.base), format!("{}/torrentio", self.base), format!("{}/eclipsia/manifest.json", self.base)],
            "catalogs": ["movie/top", "series/top"], "limit": 3, "max_resolution": 0, "max_size_gb": 0,
        });
        config.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        json!({"config": config})
    }

    fn kill(&self, path: &str, dead: bool) {
        let mut set = self.dead.lock().unwrap();
        if dead { set.insert(path.into()) } else { set.remove(path) };
    }
}

#[test]
fn stremio_catalogs_become_items_and_streams_are_pinned() {
    let Some(fx) = serve() else {
        eprintln!("SKIP: no network address to serve the fixtures from");
        return;
    };
    let items = fx.call("items", &[fx.config(json!({}))]).unwrap();
    let items = items.as_array().unwrap();
    let ids: Vec<&str> = items.iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [
        "st-com-linvo-cinemeta-movie-top-tt34384900",
        "st-com-linvo-cinemeta-movie-top-tt6262770",
        "st-com-linvo-cinemeta-series-top-tt11540908-1-1",
        "st-com-linvo-cinemeta-series-top-tt11540908-1-2",
        "st-com-linvo-cinemeta-series-top-tt11540908-1-3",
    ]);
    assert_eq!(items[0]["kind"], "movie");
    assert_eq!(items[0]["ids"], json!({"imdb": "tt34384900"}));
    assert_eq!(items[0]["display"]["title"], "Love of Your Life");
    let episode = &items[2];
    assert_eq!((episode["kind"].as_str(), episode["season"].as_i64(), episode["episode"].as_i64()), (Some("episode"), Some(1), Some(1)));
    assert_eq!(episode["ids"], json!({"imdb": "tt11540908"}));
    assert_eq!(episode["display"]["show"]["title"], "Carrie");
    assert_eq!(episode["path"], "stremio/com-linvo-cinemeta-series-top/st-com-linvo-cinemeta-series-top-tt11540908-1-1.mkv");

    let open = |id: &str, extra: Json| fx.call("open", &[json!(id), fx.config(extra)]);
    let first = "st-com-linvo-cinemeta-series-top-tt11540908-1-1";
    // Torrentio's three streams are all 2160p: its first one's link has expired, so the second plays and is pinned.
    fx.kill("/video/torrentio-1-0.mkv", true);
    assert_eq!(open(first, json!({})).unwrap()["url"], format!("{}/video/torrentio-1-1.mkv", fx.base));
    // The first link works again, but the item keeps the stream it was checked with.
    fx.kill("/video/torrentio-1-0.mkv", false);
    assert_eq!(open(first, json!({})).unwrap()["url"], format!("{}/video/torrentio-1-1.mkv", fx.base));
    // The pinned link stops opening: the next best stream plays, and is pinned from then on.
    fx.kill("/video/torrentio-1-1.mkv", true);
    assert_eq!(open(first, json!({})).unwrap()["url"], format!("{}/video/torrentio-1-0.mkv", fx.base));
    fx.kill("/video/torrentio-1-1.mkv", false);
    assert_eq!(open(first, json!({})).unwrap()["url"], format!("{}/video/torrentio-1-0.mkv", fx.base));
    // Nothing within the resolution limit.
    let error = open("st-com-linvo-cinemeta-series-top-tt11540908-1-2", json!({"max_resolution": 1080})).unwrap_err();
    assert!(error.contains("no stream addon has a stream that opens"), "{error}");
    // Streams over the size limit are left out (5.37 GB); of the rest, the addon's order decides.
    let small = open("st-com-linvo-cinemeta-series-top-tt11540908-1-3", json!({"max_size_gb": 5.2})).unwrap();
    assert_eq!(small["url"], format!("{}/video/torrentio-3-0.mkv", fx.base));

    // Eclipsia's links need their Referer: the link comes back with it, and without Accept-Encoding.
    let film = open("st-com-linvo-cinemeta-movie-top-tt34384900", json!({})).unwrap();
    assert_eq!(film["url"], format!("{}/hdr/eclipsia-0.mp4", fx.base));
    assert_eq!(film["headers"]["Referer"], "https://videodownloader.site/");
    assert!(film["headers"].get("Accept-Encoding").is_none());
}

#[test]
fn unread_fetches_are_closed_past_the_cap() {
    let Some(fx) = serve() else {
        eprintln!("SKIP: no network address to serve the fixtures from");
        return;
    };
    let source = format!(
        "export default {{ async items() {{
           const answers = [];
           for (let i = 0; i < 40; i++) answers.push(await fetch('{}/cinemeta/manifest.json'));
           let first;
           try {{ await answers[0].text(); first = 'read'; }} catch (error) {{ first = error.message; }}
           return [first, (await answers[39].json()).name];
         }}, open() {{}} }};",
        fx.base
    );
    let env = Env { addon: "t", source: &source, hosts: fx.hosts.clone(), storage: fx.storage.clone(), prober: fx.prober.clone(), spotify: None, calls: None };
    let got = sandbox::call(&env, "items", &[], sandbox::ITEMS).unwrap();
    assert_eq!(got, json!(["the body was already read, or closed after 32 newer unread ones", "Cinemeta"]));
}

/// A file server on this machine's network address that answers Range, logging each path read.
fn media_server(root: std::path::PathBuf) -> Option<(String, Arc<Mutex<Vec<String>>>)> {
    let lan = lan_ip()?;
    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let base = format!("http://{lan}:{}", server.server_addr().to_ip().unwrap().port());
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = log.clone();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let path = request.url().split('?').next().unwrap_or("").to_string();
            seen.lock().unwrap().push(path.clone());
            let Ok(bytes) = std::fs::read(root.join(path.trim_start_matches('/'))) else {
                let _ = request.respond(tiny_http::Response::from_string("no").with_status_code(404));
                continue;
            };
            let kind = if path.ends_with(".m3u8") { "application/vnd.apple.mpegurl" } else if path.ends_with(".ts") { "video/mp2t" } else { "video/mp4" };
            let range = request.headers().iter().find(|h| h.field.equiv("Range")).and_then(|h| {
                let (start, end) = h.value.as_str().strip_prefix("bytes=")?.split_once('-')?;
                let start: usize = start.parse().ok()?;
                let end = end.parse::<usize>().map_or(bytes.len() - 1, |e| e.min(bytes.len() - 1));
                (start <= end).then_some((start, end))
            });
            let response = match range {
                Some((start, end)) => tiny_http::Response::from_data(bytes[start..=end].to_vec())
                    .with_status_code(206)
                    .with_header(header("Content-Range", &format!("bytes {start}-{end}/{}", bytes.len()))),
                None => tiny_http::Response::from_data(bytes),
            };
            let _ = request.respond(response.with_header(header("Content-Type", kind)).with_header(header("Accept-Ranges", "bytes")));
        }
    });
    Some((base, log))
}

#[test]
fn ffprobe_reads_through_the_relay() {
    let tools = ["ffmpeg", "ffprobe"].iter().all(|t| std::process::Command::new(t).arg("-version").output().is_ok_and(|o| o.status.success()));
    let root = std::env::temp_dir().join(format!("rex-addons-media-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("hls")).unwrap();
    let Some((base, log)) = media_server(root.clone()).filter(|_| tools) else {
        eprintln!("SKIP: needs ffmpeg, ffprobe and a network address");
        return;
    };
    // An MP4, and an HLS playlist of three segments.
    let make = |args: &str| {
        let status = std::process::Command::new("ffmpeg").args(args.split(' ')).current_dir(&root).status().unwrap();
        assert!(status.success(), "ffmpeg {args}");
    };
    make("-v error -f lavfi -i testsrc=size=160x120:rate=10 -t 2 -c:v mpeg4 clip.mp4");
    make("-v error -f lavfi -i testsrc=size=160x120:rate=10 -t 3 -c:v mpeg2video -f hls -hls_time 1 -hls_list_size 0 hls/index.m3u8");

    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let listen = server.server_addr().to_ip().unwrap();
    let data = root.join("data");
    let app = crate::app::App::new(data, "http://127.0.0.1:9".into(), listen, Some("ffprobe".into()), None);
    let served = app.clone();
    std::thread::spawn(move || crate::server::serve(served, server, None));

    let lan = base.trim_start_matches("http://").split(':').next().unwrap().to_string();
    let source = "export default { async items(ctx) {
        const film = await ffprobe(ctx.config.film, { headers: { Referer: 'https://example.com/' } });
        const live = await ffprobe(ctx.config.live);
        return [film.format.format_name, film.streams[0].codec_name, film.format.filename ?? null, live.format.format_name, live.streams.length];
    }, open() {} };";
    let env = Env {
        addon: "t",
        source,
        hosts: Hosts::from(vec![lan]),
        storage: Arc::new(Storage::open(root.join("storage.json"))),
        prober: app.prober.clone(),
        spotify: None,
        calls: None,
    };
    let config = json!({"config": {"film": format!("{base}/clip.mp4"), "live": format!("{base}/hls/index.m3u8")}});
    let got = sandbox::call(&env, "items", &[config], sandbox::ITEMS).unwrap();
    assert_eq!(got, json!(["mov,mp4,m4a,3gp,3g2,mj2", "mpeg4", null, "hls", 1]));
    // Every read came through the relay, segments included: the playlist was rewritten.
    let seen = log.lock().unwrap();
    assert!(seen.iter().any(|p| p == "/clip.mp4"), "{seen:?}");
    assert!(seen.iter().any(|p| p.starts_with("/hls/index") && p.ends_with(".ts")), "{seen:?}");
    let _ = std::fs::remove_dir_all(&root);
}
