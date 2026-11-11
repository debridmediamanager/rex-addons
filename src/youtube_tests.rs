//! The youtube addon against YouTube's answers, served from this machine's network address with
//! the addon's links pointed there (the sandbox never fetches loopback), and its HLS variants made
//! by FFmpeg and probed through this host's relay.
//!
//! Recorded on 2026-11-11, signed out, tracking values, the requesting address and stream links
//! taken out (tests/fixtures/youtube): the TV app's `browse` of a public playlist and its
//! continuation, and the iOS app's `player` answers for three of its videos. None of those player
//! answers carries an `hlsManifestUrl` (that morning's measurement from the same address had one).
//!
//! Fabricated, as no account or HLS answer could be recorded: Liked videos and Watch later
//! (`*.FABRICATED.json`, the recorded page's shape and tiles), the HLS master playlist
//! (`hls-master.FABRICATED.m3u8`, YouTube's shape), and below: the OAuth answers, the account's
//! playlists and name, the web player's upload date, and the `hlsManifestUrl` added to two
//! recorded player answers.
use std::net::{IpAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as Json};

use crate::app::App;
use crate::net::Hosts;
use crate::sandbox::{self, Env, Storage};

const PLAYLIST: &str = "PL6B3937A5D230E335";

fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn fixture(name: &str) -> Json {
    let text = match name {
        "page1" => include_str!("../tests/fixtures/youtube/tv-browse-PL6B3937A5D230E335.json"),
        "page2" => include_str!("../tests/fixtures/youtube/tv-browse-PL6B3937A5D230E335-page2.json"),
        "LL" => include_str!("../tests/fixtures/youtube/tv-browse-LL.FABRICATED.json"),
        "WL" => include_str!("../tests/fixtures/youtube/tv-browse-WL.FABRICATED.json"),
        "l5OZu-IrXpw" => include_str!("../tests/fixtures/youtube/ios-player-l5OZu-IrXpw.json"),
        "u9lj-c29dxI" => include_str!("../tests/fixtures/youtube/ios-player-u9lj-c29dxI.json"),
        "SkVqJ1SGeL0" => include_str!("../tests/fixtures/youtube/ios-player-SkVqJ1SGeL0.json"),
        _ => return Json::Null,
    };
    serde_json::from_str(text).unwrap()
}

/// One request the website was sent.
#[derive(Clone, Debug)]
struct Hit {
    path: String,
    /// The InnerTube client, else "".
    client: String,
    /// The browse id, "continuation", the video id or the OAuth grant.
    what: String,
    auth: Option<String>,
    agent: String,
}

#[derive(Default)]
struct Site {
    hits: Vec<Hit>,
    /// The person entered the code.
    approved: bool,
    refuse_refresh: bool,
    /// Access tokens handed out; the newest is the one accepted.
    issued: u32,
}

struct Rig {
    base: String,
    hosts: Hosts,
    site: Arc<Mutex<Site>>,
    storage: Arc<Storage>,
    app: Arc<App>,
    root: PathBuf,
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}

/// What the website answers: status, body, content type.
fn answer(site: &Mutex<Site>, base: &str, root: &Path, path: &str, body: &Json, auth: Option<String>) -> (u16, Vec<u8>, &'static str) {
    let json = |status: u16, value: Json| (status, value.to_string().into_bytes(), "application/json");
    let mut site = site.lock().unwrap();
    let signed = auth.as_deref() == Some(format!("Bearer access-{}", site.issued).as_str()) && site.issued > 0;
    match path {
        "/youtubei/v1/browse" => {
            let page1 = fixture("page1");
            let token = page1.pointer("/contents/tvBrowseRenderer/content/tvSurfaceContentRenderer/content/twoColumnRenderer/rightColumn/playlistVideoListRenderer/continuations/0/nextContinuationData/continuation").cloned();
            match body["browseId"].as_str() {
                Some(id) if id == format!("VL{PLAYLIST}") => json(200, page1),
                Some("VLLL" | "VLWL" | "VLPLown") if !signed => json(401, json!({"error": {"code": 401}})),
                Some("VLLL") => json(200, fixture("LL")),
                Some("VLWL") => json(200, fixture("WL")),
                Some("VLPLown") => json(200, page1),
                None if body.get("continuation") == token.as_ref() => json(200, fixture("page2")),
                _ => json(404, json!({})),
            }
        }
        "/youtubei/v1/player" => {
            let video = body["videoId"].as_str().unwrap_or("");
            if body["context"]["client"]["clientName"] == "WEB" {
                // Fabricated: the web player's microformat, for the upload date alone.
                let date = match video { "l5OZu-IrXpw" => "2026-05-11T05:00:01-07:00", _ => "2021-01-26T04:00:00-08:00" };
                return json(200, json!({"playabilityStatus": {"status": "OK"}, "microformat": {"playerMicroformatRenderer": {"publishDate": date, "uploadDate": date}}}));
            }
            let mut player = fixture(video);
            if player.is_null() {
                return json(404, json!({}));
            }
            if video != "SkVqJ1SGeL0" {
                // Fabricated: the iOS answer's HLS master, as measured on 2026-11-11.
                player["streamingData"]["hlsManifestUrl"] = json!(format!("{base}/api/manifest/hls_variant/id/{video}/source/youtube/file/index.m3u8"));
            }
            json(200, player)
        }
        "/youtubei/v1/playlist/get_add_to_playlist" if signed => json(200, json!({"contents": [{"addToPlaylistRenderer": {"playlists": [
            {"playlistAddToOptionRenderer": {"playlistId": "WL", "title": {"simpleText": "Watch later"}}},
            {"playlistAddToOptionRenderer": {"playlistId": "PLown", "title": {"simpleText": "My films"}}},
        ]}}]})),
        "/youtubei/v1/account/accounts_list" if signed => json(200, json!({"contents": [{"accountSectionListRenderer": {"contents": [
            {"accountItemSectionRenderer": {"contents": [{"accountItem": {"accountName": {"simpleText": "Rex Tester"}}}]}},
        ]}}]})),
        "/youtubei/v1/playlist/get_add_to_playlist" | "/youtubei/v1/account/accounts_list" => json(401, json!({})),
        "/o/oauth2/device/code" => json(200, json!({
            "device_code": "DEVICE-1", "user_code": "ABC-DEF-GHI", "expires_in": 1800, "interval": 5,
            "verification_url": "https://www.google.com/device",
        })),
        "/o/oauth2/token" => match (body["grant_type"].as_str(), site.approved, site.refuse_refresh) {
            // As measured: the device grant's waiting answers are a 200 with an `error`.
            (Some("http://oauth.net/grant_type/device/1.0"), false, _) => json(200, json!({"error": "authorization_pending"})),
            (Some("http://oauth.net/grant_type/device/1.0"), true, _) if body["code"] == "DEVICE-1" => {
                site.issued += 1;
                json(200, json!({"access_token": format!("access-{}", site.issued), "refresh_token": "refresh-1", "expires_in": 3600, "token_type": "Bearer"}))
            }
            (Some("refresh_token"), _, false) if body["refresh_token"] == "refresh-1" => {
                site.issued += 1;
                json(200, json!({"access_token": format!("access-{}", site.issued), "expires_in": 3600, "token_type": "Bearer"}))
            }
            _ => json(400, json!({"error": "invalid_grant", "error_description": "Token has been expired or revoked."})),
        },
        "/o/oauth2/revoke" => json(200, json!({})),
        _ if path.starts_with("/api/manifest/hls_variant/id/") => {
            let video = path.split('/').nth(5).unwrap_or("");
            let master = include_str!("../tests/fixtures/youtube/hls-master.FABRICATED.m3u8").replace("{BASE}", base).replace("{VIDEO}", video);
            (200, master.into_bytes(), "application/vnd.apple.mpegurl")
        }
        _ if path.starts_with("/api/manifest/hls_playlist/") => {
            let file = path.rsplit('/').next().unwrap_or("");
            match std::fs::read(root.join(file)) {
                Ok(bytes) => (200, bytes, if file.ends_with(".m3u8") { "application/vnd.apple.mpegurl" } else { "video/mp4" }),
                Err(_) => (404, Vec::new(), "text/plain"),
            }
        }
        _ => json(404, json!({})),
    }
}

fn serve(name: &str) -> Option<Rig> {
    let tools = ["ffmpeg", "ffprobe"].iter().all(|t| std::process::Command::new(t).arg("-version").output().is_ok_and(|o| o.status.success()));
    let lan = lan_ip().filter(|_| tools)?;
    let root = std::env::temp_dir().join(format!("rex-addons-youtube-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // One variant's media, served for every variant: muxed H.264 + AAC in fMP4 segments, as YouTube's.
    let status = std::process::Command::new("ffmpeg")
        .args("-v error -f lavfi -i testsrc=size=320x180:rate=24 -f lavfi -i sine=frequency=440 -t 4 -c:v libx264 -preset ultrafast -c:a aac -map 1:a -map 0:v -f hls -hls_time 2 -hls_playlist_type vod -hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4 -hls_segment_filename seg%03d.m4s index.m3u8".split(' '))
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(status.success());

    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let base = format!("http://{lan}:{}", server.server_addr().to_ip().unwrap().port());
    let site: Arc<Mutex<Site>> = Arc::default();
    let (served_site, served_base, served_root) = (site.clone(), base.clone(), root.clone());
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut text = String::new();
            let _ = request.as_reader().read_to_string(&mut text);
            let body: Json = serde_json::from_str(&text).unwrap_or(Json::Null);
            let path = request.url().split('?').next().unwrap_or("").to_string();
            let value = |name: &'static str| request.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.to_string());
            let what = ["browseId", "videoId", "grant_type"].iter().find_map(|k| body[k].as_str().map(str::to_string))
                .or(body.get("continuation").map(|_| "continuation".to_string()))
                .unwrap_or_default();
            let hit = Hit {
                path: path.clone(),
                client: body["context"]["client"]["clientName"].as_str().unwrap_or("").into(),
                what,
                auth: value("Authorization"),
                agent: value("User-Agent").unwrap_or_default(),
            };
            served_site.lock().unwrap().hits.push(hit.clone());
            let (status, bytes, kind) = answer(&served_site, &served_base, &served_root, &path, &body, hit.auth);
            let _ = request.respond(tiny_http::Response::from_data(bytes).with_status_code(status).with_header(header("Content-Type", kind)));
        }
    });

    // The host, for the relay ffprobe() reads through.
    let listener = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let app = App::new(root.join("data"), "http://127.0.0.1:9".into(), listener.server_addr().to_ip().unwrap(), Some("ffprobe".into()), None);
    let served = app.clone();
    std::thread::spawn(move || crate::server::serve(served, listener, None));
    let storage = Arc::new(Storage::open(root.join("storage.json")));
    Some(Rig { hosts: Hosts(vec![lan.to_string()]), base, site, storage, app, root })
}

impl Rig {
    fn source(&self) -> String {
        let source = include_str!("../addons/youtube/main.js");
        for constant in ["const YT = \"https://www.youtube.com\";", "const CHECK_PAUSE_MS = 1500;"] {
            assert!(source.contains(constant), "main.js no longer has {constant}");
        }
        source.replace("https://www.youtube.com", &self.base).replace("const CHECK_PAUSE_MS = 1500;", "const CHECK_PAUSE_MS = 0;")
    }

    fn call(&self, method: &str, args: &[Json]) -> Result<Json, String> {
        let source = self.source();
        let env = Env { addon: "youtube", source: &source, hosts: &self.hosts, storage: self.storage.clone(), prober: self.app.prober.clone() };
        sandbox::call(&env, method, args, sandbox::ITEMS)
    }

    /// The addon's storage, read with a call of its own.
    fn stored(&self, key: &str) -> Json {
        let source = format!("export default {{ items() {{ return storage.get({key:?}) ?? null; }}, open() {{}} }};");
        let env = Env { addon: "t", source: &source, hosts: &self.hosts, storage: self.storage.clone(), prober: self.app.prober.clone() };
        sandbox::call(&env, "items", &[], sandbox::ITEMS).unwrap()
    }

    fn store(&self, key: &str, value: Json) {
        let source = format!("export default {{ items() {{ storage.set({key:?}, {value}); return []; }}, open() {{}} }};");
        let env = Env { addon: "t", source: &source, hosts: &self.hosts, storage: self.storage.clone(), prober: self.app.prober.clone() };
        sandbox::call(&env, "items", &[], sandbox::ITEMS).unwrap();
    }

    fn hits(&self) -> Vec<Hit> {
        std::mem::take(&mut self.site.lock().unwrap().hits)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn config(extra: Json) -> Json {
    let mut config = json!({"count": 200, "max_height": 1080, "liked": true, "watch_later": true, "own": true, "playlists": []});
    config.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    json!({"config": config})
}

#[test]
fn a_public_playlist_becomes_a_playlist_of_its_channels_episodes() {
    let Some(rig) = serve("public") else {
        eprintln!("SKIP: needs ffmpeg, ffprobe and a network address");
        return;
    };
    // 16 videos: the first page's 15 and one of the continuation's.
    let ctx = config(json!({"count": 16, "playlists": [format!("https://www.youtube.com/playlist?list={PLAYLIST}")]}));
    let items = rig.call("items", std::slice::from_ref(&ctx)).unwrap();
    let hits = rig.hits();

    let listed = rig.stored("listing")["lists"][0]["videos"].clone();
    assert_eq!(listed, json!([
        "l5OZu-IrXpw", "u9lj-c29dxI", "UXqq0ZvbOnk", "_cMxraX_5RE", "PVGeM40dABA", "WhWc3b3KhnY", "pKmSdY56VtY", "apiu3pTIwuY",
        "mN0zPOpADL4", "SkVqJ1SGeL0", "lqiN98z6Dak", "Y-rmzh0PI3c", "Z4C82eyhwgU", "R6MlUcmOul8", "eRsGyueVLvQ", "YE7VzlLtp-4",
    ]));
    let browses: Vec<&Hit> = hits.iter().filter(|h| h.path == "/youtubei/v1/browse").collect();
    assert_eq!(browses.iter().map(|h| h.what.as_str()).collect::<Vec<_>>(), [format!("VL{PLAYLIST}").as_str(), "continuation"]);
    assert!(browses.iter().all(|h| h.client == "TVHTML5" && h.auth.is_none() && h.agent.contains("Cobalt")), "{browses:?}");
    // Each listed video was asked about once, as the iOS app, signed out.
    let players: Vec<&Hit> = hits.iter().filter(|h| h.path == "/youtubei/v1/player" && h.client == "IOS").collect();
    assert_eq!(players.len(), 16);
    assert!(players.iter().all(|h| h.auth.is_none() && h.agent.starts_with("com.google.ios.youtube/")));
    // Of those, two have recorded answers (the rest answer 404 here, and wait a day): both reach rex.
    let items = items.as_array().unwrap();
    assert_eq!(items.iter().map(|i| i["id"].as_str().unwrap()).collect::<Vec<_>>(), ["yt-l5OZu-IrXpw", "yt-u9lj-c29dxI"]);
    let first = &items[0];
    assert_eq!(first["path"], "youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/l5OZu-IrXpw.m3u8");
    assert_eq!((first["kind"].as_str(), first["season"].as_i64(), first["episode"].as_i64()), (Some("episode"), Some(1), Some(1)));
    assert_eq!(items[1]["episode"], 2);
    assert_eq!((first["name"].as_str(), first["release"].as_str()), (Some("Blender Studio"), Some("youtube:UCz75RVbH8q2jdBJ4SnwuZZQ")));
    let display = &first["display"];
    assert_eq!(display["title"], "SINGULARITY - Painterly Space Adventure");
    assert!(display["summary"].as_str().unwrap().starts_with("Our story takes place in a distant age of the universe"));
    assert_eq!(display["date"], "2026-05-11");
    assert_eq!(display["thumb"], "https://i.ytimg.com/vi/l5OZu-IrXpw/sddefault.jpg");
    assert_eq!(display["show"], json!({"title": "Blender Studio", "poster": "https://i.ytimg.com/vi/l5OZu-IrXpw/sddefault.jpg"}));
    assert_eq!(items[1]["display"]["show"]["poster"], display["show"]["poster"]);
    assert_eq!(first["playlists"], json!({"Official Blender Open Movies": 1}));
    assert_eq!(items[1]["playlists"], json!({"Official Blender Open Movies": 2}));
    // Probed through the relay: the 1080p variant, its segments read too.
    assert_eq!(first["media_info"]["format"]["format_name"], "hls");
    let kinds: Vec<&str> = first["media_info"]["streams"].as_array().unwrap().iter().map(|s| s["codec_name"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["aac", "h264"]);
    assert!(hits.iter().any(|h| h.path.contains("/itag/96/") && h.path.ends_with(".m4s")), "{hits:?}");
    assert!(!hits.iter().any(|h| h.path.contains("/itag/620/") || h.path.contains("/itag/270/")));

    // The next sync lists nothing (the listing is an hour old at most) and asks about nothing:
    // what was checked is kept, what failed waits.
    assert_eq!(rig.call("items", std::slice::from_ref(&ctx)).unwrap().as_array().unwrap().len(), 2);
    assert!(rig.hits().is_empty());

    // Playing asks again for the stream, at the resolution it was probed at.
    let opened = rig.call("open", &["yt-l5OZu-IrXpw".into(), ctx.clone()]).unwrap();
    assert_eq!(opened["url"], format!("{}/api/manifest/hls_playlist/id/l5OZu-IrXpw/itag/96/source/youtube/playlist/index.m3u8", rig.base));
    assert!(opened.get("headers").is_none());
    // The recorded answer with no HLS stream: a clear refusal.
    let error = rig.call("open", &["yt-SkVqJ1SGeL0".into(), ctx.clone()]).unwrap_err();
    assert!(error.contains("SkVqJ1SGeL0 does not play: YouTube offered no HLS stream"), "{error}");

    // A lower limit checks the two again, at 720p; their numbers stay.
    let lower = config(json!({"count": 16, "max_height": 720, "playlists": [format!("https://www.youtube.com/playlist?list={PLAYLIST}")]}));
    let items = rig.call("items", std::slice::from_ref(&lower)).unwrap();
    assert_eq!(items[1]["episode"], 2);
    assert!(rig.hits().iter().any(|h| h.path.contains("/itag/95/") && h.path.ends_with(".m4s")));
    let opened = rig.call("open", &["yt-u9lj-c29dxI".into(), lower]).unwrap();
    assert!(opened["url"].as_str().unwrap().contains("/itag/95/"), "{opened}");
    // Under the smallest variant, the smallest plays.
    rig.store("v:u9lj-c29dxI", json!({"height": 100}));
    let opened = rig.call("open", &["yt-u9lj-c29dxI".into(), ctx]).unwrap();
    assert!(opened["url"].as_str().unwrap().contains("/itag/91/"), "{opened}");
}

#[test]
fn signed_in_the_accounts_playlists_come_with_one_item_per_video() {
    let Some(rig) = serve("signed") else {
        eprintln!("SKIP: needs ffmpeg, ffprobe and a network address");
        return;
    };
    // Signed in before, its access token since expired.
    rig.site.lock().unwrap().issued = 1;
    rig.store("tokens", json!({"access": "access-1", "refresh": "refresh-1", "expires_at": 0}));
    let ctx = config(json!({"count": 2}));
    let items = rig.call("items", std::slice::from_ref(&ctx)).unwrap();
    let hits = rig.hits();

    // Refreshed once, then every account call carries the new token; the player is asked signed out.
    assert_eq!(hits.iter().filter(|h| h.what == "refresh_token").count(), 1);
    let signed: Vec<(&str, &str)> = hits.iter().filter(|h| h.auth.is_some()).map(|h| (h.path.as_str(), h.what.as_str())).collect();
    assert_eq!(signed, [
        ("/youtubei/v1/playlist/get_add_to_playlist", ""),
        ("/youtubei/v1/browse", "VLLL"),
        ("/youtubei/v1/browse", "VLWL"),
        ("/youtubei/v1/browse", "VLPLown"),
    ]);
    assert!(hits.iter().filter(|h| h.auth.is_some()).all(|h| h.auth.as_deref() == Some("Bearer access-2")));
    assert!(hits.iter().filter(|h| h.path == "/youtubei/v1/player").all(|h| h.auth.is_none()));

    // Liked videos skips its live stream and private video; SkVqJ1SGeL0 has no HLS stream, so waits.
    let listing = rig.stored("listing");
    assert_eq!(listing["lists"], json!([
        {"title": "Liked videos", "videos": ["l5OZu-IrXpw", "SkVqJ1SGeL0"]},
        {"title": "Watch later", "videos": ["u9lj-c29dxI"]},
        {"title": "My films", "videos": ["l5OZu-IrXpw", "u9lj-c29dxI"]},
    ]));
    assert_eq!(rig.stored("skip:SkVqJ1SGeL0")["why"], "YouTube offered no HLS stream");
    let items = items.as_array().unwrap();
    let got: Vec<(&str, &Json)> = items.iter().map(|i| (i["id"].as_str().unwrap(), &i["playlists"])).collect();
    assert_eq!(got, [
        ("yt-l5OZu-IrXpw", &json!({"Liked videos": 1, "My films": 1})),
        ("yt-u9lj-c29dxI", &json!({"Watch later": 1, "My films": 2})),
    ]);

    // Turning Liked videos off forgets what only it held.
    rig.call("items", &[config(json!({"count": 2, "liked": false}))]).unwrap();
    assert_eq!(rig.stored("skip:SkVqJ1SGeL0"), Json::Null);
    assert!(rig.stored("v:l5OZu-IrXpw").is_object());

    // YouTube ends the session: the refresh is refused, and the addon asks to be signed in again.
    rig.hits();
    {
        let mut site = rig.site.lock().unwrap();
        site.refuse_refresh = true;
        site.issued += 1;
    }
    let error = rig.call("items", &[config(json!({"count": 3}))]).unwrap_err();
    assert!(error.starts_with("SIGN_IN_REQUIRED: YouTube refused the sign-in (Token has been expired or revoked.)"), "{error}");
}

#[test]
fn signing_in_with_a_code_keeps_the_tokens_and_signing_out_revokes_them() {
    let Some(rig) = serve("sign-in") else {
        eprintln!("SKIP: needs ffmpeg, ffprobe and a network address");
        return;
    };
    let ctx = config(json!({}));
    let started = rig.call("signIn", std::slice::from_ref(&ctx)).unwrap();
    assert_eq!(started, json!({"code": "ABC-DEF-GHI", "url": "https://www.google.com/device", "expires_in": 1800, "interval": 5, "state": {"device_code": "DEVICE-1"}}));
    assert_eq!(rig.call("signInPoll", &[started["state"].clone(), ctx.clone()]).unwrap(), json!({"done": false}));
    rig.site.lock().unwrap().approved = true;
    assert_eq!(rig.call("signInPoll", &[started["state"].clone(), ctx.clone()]).unwrap(), json!({"done": true, "account": "Rex Tester"}));
    assert_eq!(rig.stored("tokens")["refresh"], "refresh-1");
    let hits = rig.hits();
    let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(paths, ["/o/oauth2/device/code", "/o/oauth2/token", "/o/oauth2/token", "/youtubei/v1/account/accounts_list"]);
    assert_eq!(hits[3].auth.as_deref(), Some("Bearer access-1"));

    rig.call("signOut", &[ctx]).unwrap();
    assert_eq!(rig.stored("tokens"), Json::Null);
    assert_eq!(rig.hits().iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["/o/oauth2/revoke"]);
}
