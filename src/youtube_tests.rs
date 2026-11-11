//! The youtube addon against YouTube's answers, served from this machine's network address with
//! the addon's links pointed there (the sandbox never fetches loopback), its videos made by FFmpeg
//! and probed through this host's relay.
//!
//! Recorded on 2026-11-11, signed out, tracking values taken out (tests/fixtures/youtube): the TV
//! app's `browse` of a public playlist and its continuation.
//!
//! Real, downloaded by the tests: the TV app's player script f5321ed3, the one YouTube's TV page
//! named on 2026-11-11 (YouTube keeps old player scripts online, as yt-dlp's own tests rely on; the
//! tests skip without it). What its n and signature functions answer below was computed by yt-dlp's
//! own solver (yt-dlp-ejs 0.8.0, run in Node) on that script, not by this addon.
//!
//! Fabricated in the structure of what the signed-in account was answered on 2026-11-11, as no
//! answer from a real account is published: Liked videos and Watch later (`*.FABRICATED.json`),
//! the TV player's answer (`tv-player-*.FABRICATED.json`, varied below per video), and below: the
//! TV page, the OAuth answers, the account's playlists and accounts, and the HLS master playlist
//! (`hls-master.FABRICATED.m3u8`) of the one video given an HLS stream.
use std::net::{IpAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as Json};

use crate::app::App;
use crate::net::Hosts;
use crate::sandbox::{self, Env, Storage};

const PLAYLIST: &str = "PL6B3937A5D230E335";
const PLAYER: &str = "/s/player/f5321ed3/tv-player-ias.vflset/tv-player-ias.js";
/// yt-dlp-ejs 0.8.0's answers for player f5321ed3.
const N: [(&str, &str); 2] = [("ZdZIqFPQK-Ty8wId", "vbisBPXSXnof"), ("abcdefghijklmnop", "k15_hiWZkeX-")];
const SIG: (&str, &str) = (
    "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz",
    "456789abcdefghij1lmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789azcdebghijklmnopqrstuvwxyf",
);
/// The videos with a file: its id and n challenge, and whether its link comes as a signatureCipher.
const FILES: [(&str, &str, bool); 2] = [("l5OZu-IrXpw", N[0].0, false), ("u9lj-c29dxI", N[1].0, true)];
const HLS_VIDEO: &str = "_cMxraX_5RE";
/// A video whose answer has no file with picture and sound together (SABR only).
const NO_FILE: &str = "SkVqJ1SGeL0";

fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// The TV player script f5321ed3, downloaded once into the temporary directory.
fn player_script() -> Option<Vec<u8>> {
    let cached = std::env::temp_dir().join("rex-addons-youtube-f5321ed3-tv-player-ias.js");
    if let Ok(bytes) = std::fs::read(&cached) {
        return Some(bytes);
    }
    let mut bytes = Vec::new();
    let response = ureq::get(&format!("https://www.youtube.com{PLAYER}")).call().ok()?;
    std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes).ok()?;
    if !String::from_utf8_lossy(&bytes).contains("signatureTimestamp:20761") {
        return None;
    }
    std::fs::write(&cached, &bytes).ok()?;
    Some(bytes)
}

fn fixture(name: &str) -> Json {
    let text = match name {
        "page1" => include_str!("../tests/fixtures/youtube/tv-browse-PL6B3937A5D230E335.json"),
        "page2" => include_str!("../tests/fixtures/youtube/tv-browse-PL6B3937A5D230E335-page2.json"),
        "LL" => include_str!("../tests/fixtures/youtube/tv-browse-LL.FABRICATED.json"),
        "WL" => include_str!("../tests/fixtures/youtube/tv-browse-WL.FABRICATED.json"),
        "player" => include_str!("../tests/fixtures/youtube/tv-player-l5OZu-IrXpw.FABRICATED.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(text).unwrap()
}

/// The TV player's answer for `video`, fabricated from the l5OZu-IrXpw one.
fn player_answer(base: &str, video: &str) -> Option<Json> {
    let template = fixture("player").to_string().replace("{BASE}", base).replace("o-FABRICATED", &format!("o-{video}"));
    let mut answer: Json = serde_json::from_str(&template).unwrap();
    answer["videoDetails"]["videoId"] = json!(video);
    if video == HLS_VIDEO {
        answer["streamingData"]["formats"] = json!([]);
        answer["streamingData"]["hlsManifestUrl"] = json!(format!("{base}/api/manifest/hls_variant/id/{video}/source/youtube/file/index.m3u8"));
    } else if video == NO_FILE {
        answer["streamingData"]["formats"] = json!([]);
    } else {
        let &(_, challenge, ciphered) = FILES.iter().find(|(id, ..)| *id == video)?;
        let format = &mut answer["streamingData"]["formats"][0];
        let url = format["url"].as_str().unwrap().replace("{N}", challenge);
        if ciphered {
            let url = url.replace("&sig=FABRICATED", "");
            let cipher: String = url::form_urlencoded::Serializer::new(String::new()).append_pair("s", SIG.0).append_pair("sp", "sig").append_pair("url", &url).finish();
            format.as_object_mut().unwrap().remove("url");
            format["signatureCipher"] = json!(cipher);
        } else {
            format["url"] = json!(url);
        }
    }
    Some(answer)
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
    sts: Option<u64>,
}

#[derive(Default)]
struct Site {
    hits: Vec<Hit>,
    /// The person entered the code.
    approved: bool,
    refuse_refresh: bool,
    /// Every file link answers 403, as for a player script YouTube no longer accepts.
    refuse_files: bool,
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

/// What the website answers: status, body, content type, Content-Range.
type Answer = (u16, Vec<u8>, &'static str, Option<String>);

fn answer(site: &Mutex<Site>, base: &str, root: &Path, url: &str, range: Option<&str>, body: &Json, auth: Option<String>) -> Answer {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let json = |status: u16, value: Json| (status, value.to_string().into_bytes(), "application/json", None);
    let mut site = site.lock().unwrap();
    let signed = auth.as_deref() == Some(format!("Bearer access-{}", site.issued).as_str()) && site.issued > 0;
    if path.starts_with("/youtubei/") && !signed {
        return json(401, json!({"error": {"code": 401}}));
    }
    match path {
        "/youtubei/v1/browse" => {
            let page1 = fixture("page1");
            let token = page1.pointer("/contents/tvBrowseRenderer/content/tvSurfaceContentRenderer/content/twoColumnRenderer/rightColumn/playlistVideoListRenderer/continuations/0/nextContinuationData/continuation").cloned();
            match body["browseId"].as_str() {
                Some(id) if id == format!("VL{PLAYLIST}") => json(200, page1),
                Some("VLLL") => json(200, fixture("LL")),
                Some("VLWL") => json(200, fixture("WL")),
                Some("VLPLown") => json(200, page1),
                None if body.get("continuation") == token.as_ref() => json(200, fixture("page2")),
                _ => json(404, json!({})),
            }
        }
        "/youtubei/v1/player" => match player_answer(base, body["videoId"].as_str().unwrap_or("")) {
            Some(player) => json(200, player),
            None => json(404, json!({})),
        },
        "/youtubei/v1/playlist/get_add_to_playlist" => json(200, json!({"contents": [{"addToPlaylistRenderer": {"playlists": [
            {"playlistAddToOptionRenderer": {"playlistId": "WL", "title": {"simpleText": "Watch later"}, "privacy": "PRIVATE"}},
            {"playlistAddToOptionRenderer": {"playlistId": "PLown", "title": {"simpleText": "My films"}, "privacy": "PUBLIC"}},
        ]}}]})),
        // As measured: every account the sign-in may act as, the one signed in with isSelected.
        "/youtubei/v1/account/accounts_list" => json(200, json!({"contents": [{"accountSectionListRenderer": {"contents": [
            {"accountItemSectionRenderer": {"contents": [
                {"accountItem": {"accountName": {"simpleText": "A Family Member"}, "isSelected": false, "hasChannel": false}},
                {"accountItem": {"accountName": {"simpleText": "Rex Tester"}, "isSelected": true, "hasChannel": true}},
            ]}},
        ]}}]})),
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
        "/tv" => (200, format!(r#"<html><script>ytcfg.set({{"WEB_PLAYER_CONTEXT_CONFIGS":{{"WEB_PLAYER_CONTEXT_CONFIG_ID_LIVING_ROOM_WATCH":{{"jsUrl":"{PLAYER}"}}}}}});</script></html>"#).into_bytes(), "text/html", None),
        PLAYER => (200, player_script().unwrap(), "text/javascript", None),
        "/videoplayback" => {
            let params: std::collections::HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
            let video = params.get("id").map_or("", |id| id.trim_start_matches("o-"));
            let n_right = FILES.iter().zip(N).any(|((id, ..), (_, solved))| *id == video && params.get("n").map(String::as_str) == Some(solved));
            let sig_right = FILES.iter().any(|(id, _, ciphered)| *id == video && (!ciphered || params.get("sig").map(String::as_str) == Some(SIG.1)));
            if site.refuse_files || !n_right || !sig_right {
                return (403, Vec::new(), "text/plain", None);
            }
            let bytes = std::fs::read(root.join("video.mp4")).unwrap();
            let total = bytes.len();
            let Some((start, end)) = range.and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split_once('-')) else {
                return (200, bytes, "video/mp4", None);
            };
            let start: usize = start.parse().unwrap_or(0);
            let end = end.parse::<usize>().map_or(total - 1, |end| end.min(total - 1));
            (206, bytes[start..=end].to_vec(), "video/mp4", Some(format!("bytes {start}-{end}/{total}")))
        }
        _ if path.starts_with("/api/manifest/hls_variant/id/") => {
            let video = path.split('/').nth(5).unwrap_or("");
            let master = include_str!("../tests/fixtures/youtube/hls-master.FABRICATED.m3u8").replace("{BASE}", base).replace("{VIDEO}", video);
            (200, master.into_bytes(), "application/vnd.apple.mpegurl", None)
        }
        _ if path.starts_with("/api/manifest/hls_playlist/") => {
            let file = path.rsplit('/').next().unwrap_or("");
            match std::fs::read(root.join(file)) {
                Ok(bytes) => (200, bytes, if file.ends_with(".m3u8") { "application/vnd.apple.mpegurl" } else { "video/mp4" }, None),
                Err(_) => (404, Vec::new(), "text/plain", None),
            }
        }
        _ => json(404, json!({})),
    }
}

fn serve(name: &str) -> Option<Rig> {
    let tools = ["ffmpeg", "ffprobe"].iter().all(|t| std::process::Command::new(t).arg("-version").output().is_ok_and(|o| o.status.success()));
    let lan = lan_ip().filter(|_| tools)?;
    player_script()?;
    let root = std::env::temp_dir().join(format!("rex-addons-youtube-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // The 360p file, H.264 + AAC in one MP4, and one HLS variant's media, muxed in fMP4 segments, as YouTube's.
    for args in [
        "-v error -f lavfi -i testsrc=size=640x268:rate=24 -f lavfi -i sine=frequency=440 -t 4 -c:v libx264 -profile:v baseline -pix_fmt yuv420p -preset ultrafast -c:a aac -movflags +faststart video.mp4",
        "-v error -f lavfi -i testsrc=size=320x180:rate=24 -f lavfi -i sine=frequency=440 -t 4 -c:v libx264 -preset ultrafast -c:a aac -map 1:a -map 0:v -f hls -hls_time 2 -hls_playlist_type vod -hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4 -hls_segment_filename seg%03d.m4s index.m3u8",
    ] {
        assert!(std::process::Command::new("ffmpeg").args(args.split(' ')).current_dir(&root).status().unwrap().success());
    }

    let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
    let base = format!("http://{lan}:{}", server.server_addr().to_ip().unwrap().port());
    let site: Arc<Mutex<Site>> = Arc::default();
    let (served_site, served_base, served_root) = (site.clone(), base.clone(), root.clone());
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut text = String::new();
            let _ = request.as_reader().read_to_string(&mut text);
            let body: Json = serde_json::from_str(&text).unwrap_or(Json::Null);
            let url = request.url().to_string();
            let path = url.split('?').next().unwrap_or("").to_string();
            let value = |name: &'static str| request.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.to_string());
            let what = ["browseId", "videoId", "grant_type"].iter().find_map(|k| body[k].as_str().map(str::to_string))
                .or(body.get("continuation").map(|_| "continuation".to_string()))
                .unwrap_or_default();
            let hit = Hit {
                path: path.clone(),
                client: body["context"]["client"]["clientName"].as_str().unwrap_or("").into(),
                what,
                auth: value("Authorization"),
                sts: body.pointer("/playbackContext/contentPlaybackContext/signatureTimestamp").and_then(Json::as_u64),
            };
            served_site.lock().unwrap().hits.push(hit.clone());
            let (status, bytes, kind, range) = answer(&served_site, &served_base, &served_root, &url, value("Range").as_deref(), &body, hit.auth);
            let mut response = tiny_http::Response::from_data(bytes).with_status_code(status).with_header(header("Content-Type", kind));
            if let Some(range) = range {
                response = response.with_header(header("Content-Range", &range));
            }
            let _ = request.respond(response);
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

    fn run(&self, source: &str, method: &str, args: &[Json]) -> Result<Json, String> {
        let env = Env { addon: "youtube", source, hosts: &self.hosts, storage: self.storage.clone(), prober: self.app.prober.clone() };
        sandbox::call(&env, method, args, sandbox::ITEMS)
    }

    fn call(&self, method: &str, args: &[Json]) -> Result<Json, String> {
        self.run(&self.source(), method, args)
    }

    /// `js` run inside the addon's module, with its functions in reach.
    fn inside(&self, js: &str) -> Result<Json, String> {
        let source = self.source().replace("export default {", "const addon = {") + &format!("\nexport default {{ async items() {{ {js} }}, open() {{}} }};");
        self.run(&source, "items", &[])
    }

    fn stored(&self, key: &str) -> Json {
        self.inside(&format!("return storage.get({key:?}) ?? null;")).unwrap()
    }

    fn store(&self, key: &str, value: Json) {
        self.inside(&format!("storage.set({key:?}, {value}); return null;")).unwrap();
    }

    /// Signed in before, its access token still good.
    fn sign_in(&self) {
        self.site.lock().unwrap().issued = 1;
        self.store("tokens", json!({"access": "access-1", "refresh": "refresh-1", "expires_at": 4_000_000_000_000u64}));
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

const SKIP: &str = "SKIP: needs ffmpeg, ffprobe, a network address and YouTube's player script f5321ed3";

#[test]
fn the_tv_players_n_and_signature_functions_are_cut_out_of_its_script_and_kept() {
    let Some(rig) = serve("player") else { return eprintln!("{SKIP}") };
    let solved = rig
        .inside(&format!(
            "const p = await playerScript(); const f = transformer(p.code);
             return [p.id, p.sts, f({{n: {:?}}}).n, f({{n: {:?}}}).n, f({{sig: {:?}}}).sig, p.code.length];",
            N[0].0, N[1].0, SIG.0
        ))
        .unwrap();
    assert_eq!(solved.as_array().unwrap()[..5], json!(["f5321ed3", 20761, N[0].1, N[1].1, SIG.1]).as_array().unwrap()[..]);
    // A small part of a 2.5 MB script.
    assert!(solved[5].as_u64().unwrap() < 100_000, "{solved}");
    let hits: Vec<String> = rig.hits().into_iter().map(|h| h.path).collect();
    assert_eq!(hits, ["/tv", PLAYER]);

    // Kept: within the hour nothing is asked; after it, the TV page is, and the same script is not fetched again.
    rig.inside("await playerScript(); return null;").unwrap();
    assert!(rig.hits().is_empty());
    rig.inside("storage.set('player', {...storage.get('player'), checked: 0}); await playerScript(); return null;").unwrap();
    assert_eq!(rig.hits().into_iter().map(|h| h.path).collect::<Vec<_>>(), ["/tv"]);
}

#[test]
fn a_public_playlist_becomes_a_playlist_of_its_channels_episodes_played_from_360p_files() {
    let Some(rig) = serve("public") else { return eprintln!("{SKIP}") };
    rig.sign_in();
    // 16 videos: the first page's 15 and one of the continuation's.
    let ctx = config(json!({"count": 16, "liked": false, "watch_later": false, "own": false, "playlists": [format!("https://www.youtube.com/playlist?list={PLAYLIST}")]}));
    let items = rig.call("items", std::slice::from_ref(&ctx)).unwrap();
    let hits = rig.hits();

    let listed: Vec<Json> = rig.stored("listing")["lists"][0]["videos"].as_array().unwrap().iter().map(|v| v["id"].clone()).collect();
    assert_eq!(Json::from(listed), json!([
        "l5OZu-IrXpw", "u9lj-c29dxI", "UXqq0ZvbOnk", "_cMxraX_5RE", "PVGeM40dABA", "WhWc3b3KhnY", "pKmSdY56VtY", "apiu3pTIwuY",
        "mN0zPOpADL4", "SkVqJ1SGeL0", "lqiN98z6Dak", "Y-rmzh0PI3c", "Z4C82eyhwgU", "R6MlUcmOul8", "eRsGyueVLvQ", "YE7VzlLtp-4",
    ]));
    // Every InnerTube call is the signed-in TV app's; each video's player answer was asked for
    // once, with the player script's signature timestamp, which was fetched once.
    let innertube: Vec<&Hit> = hits.iter().filter(|h| h.path.starts_with("/youtubei/")).collect();
    assert!(innertube.iter().all(|h| h.client == "TVHTML5" && h.auth.as_deref() == Some("Bearer access-1")), "{innertube:?}");
    let browses: Vec<&str> = innertube.iter().filter(|h| h.path == "/youtubei/v1/browse").map(|h| h.what.as_str()).collect();
    assert_eq!(browses, [format!("VL{PLAYLIST}").as_str(), "continuation"]);
    let players: Vec<&Hit> = innertube.iter().copied().filter(|h| h.path == "/youtubei/v1/player").collect();
    assert_eq!(players.len(), 16);
    assert!(players.iter().all(|h| h.sts == Some(20761)));
    assert_eq!(hits.iter().filter(|h| h.path == "/tv" || h.path == PLAYER).count(), 2);

    // The two files (one with a signature to transform too) and the HLS stream reach rex.
    let items = items.as_array().unwrap();
    assert_eq!(items.iter().map(|i| i["id"].as_str().unwrap()).collect::<Vec<_>>(), ["yt-l5OZu-IrXpw", "yt-u9lj-c29dxI", "yt-_cMxraX_5RE"]);
    let first = &items[0];
    assert_eq!(first["path"], "youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/l5OZu-IrXpw.mp4");
    assert_eq!(items[2]["path"], "youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/_cMxraX_5RE.m3u8");
    assert_eq!((first["kind"].as_str(), first["season"].as_i64(), first["episode"].as_i64()), (Some("episode"), Some(1), Some(1)));
    assert_eq!((items[1]["episode"].as_i64(), items[2]["episode"].as_i64()), (Some(2), Some(3)));
    // Titled from the playlist's tile; the channel's name from its first video's tile, for every episode.
    assert_eq!(first["release"], "youtube:UCz75RVbH8q2jdBJ4SnwuZZQ");
    let display = &first["display"];
    assert_eq!(display["title"], "SINGULARITY - Painterly Space Adventure");
    assert_eq!(items[1]["display"]["title"], "WING IT! - Blender Open Movie");
    assert_eq!(display["date"], "2026-05-11");
    assert_eq!(display["thumb"], "https://i.ytimg.com/vi/l5OZu-IrXpw/hqdefault.jpg");
    for item in items {
        assert_eq!(item["display"]["show"], json!({"title": "Blender Studio and Blender", "poster": "https://i.ytimg.com/vi/l5OZu-IrXpw/hqdefault.jpg"}));
        assert_eq!(item["name"], "Blender Studio and Blender");
    }
    assert_eq!(first["playlists"], json!({"Official Blender Open Movies": 1}));
    assert_eq!(items[2]["playlists"], json!({"Official Blender Open Movies": 4}));
    // Probed through the relay at the fixed link: the file itself, and the HLS stream's 1080p variant.
    assert!(first["media_info"]["format"]["format_name"].as_str().unwrap().contains("mp4"), "{}", first["media_info"]);
    let kinds: Vec<&str> = first["media_info"]["streams"].as_array().unwrap().iter().map(|s| s["codec_name"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["h264", "aac"]);
    assert_eq!(items[2]["media_info"]["format"]["format_name"], "hls");
    assert!(hits.iter().any(|h| h.path.contains("/itag/96/") && h.path.ends_with(".m4s")), "{hits:?}");
    assert_eq!(rig.stored(&format!("skip:{NO_FILE}"))["why"], "YouTube offered no file with picture and sound together");

    // The next sync lists nothing and asks about nothing: what was checked is kept, what failed waits.
    assert_eq!(rig.call("items", std::slice::from_ref(&ctx)).unwrap().as_array().unwrap().len(), 3);
    assert!(rig.hits().is_empty());

    // Playing asks again for the file, its link fixed as the player fixes it.
    let opened = rig.call("open", &["yt-l5OZu-IrXpw".into(), ctx.clone()]).unwrap();
    let url = opened["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{}/videoplayback?", rig.base)) && url.contains(&format!("&n={}&", N[0].1)), "{url}");
    assert!(opened.get("headers").is_none());
    assert_eq!(rig.hits().iter().map(|h| (h.path.as_str(), h.what.as_str())).collect::<Vec<_>>(), [("/youtubei/v1/player", "l5OZu-IrXpw")]);
    let opened = rig.call("open", &["yt-u9lj-c29dxI".into(), ctx.clone()]).unwrap();
    assert!(opened["url"].as_str().unwrap().contains(&format!("&sig={}", SIG.1)), "{opened}");
    // The HLS stream plays at the variant it was probed at.
    let opened = rig.call("open", &[format!("yt-{HLS_VIDEO}").into(), ctx.clone()]).unwrap();
    assert!(opened["url"].as_str().unwrap().contains("/itag/96/"), "{opened}");
    let error = rig.call("open", &[format!("yt-{NO_FILE}").into(), ctx.clone()]).unwrap_err();
    assert!(error.contains(&format!("{NO_FILE} does not play: YouTube offered no file with picture and sound together")), "{error}");

    // A lower limit checks the HLS stream again, at 720p, and the files not at all; numbers stay.
    rig.hits();
    let lower = config(json!({"count": 16, "max_height": 720, "liked": false, "watch_later": false, "own": false, "playlists": [format!("https://www.youtube.com/playlist?list={PLAYLIST}")]}));
    let items = rig.call("items", std::slice::from_ref(&lower)).unwrap();
    assert_eq!(items[2]["episode"], 3);
    let hits = rig.hits();
    assert_eq!(hits.iter().filter(|h| h.path == "/youtubei/v1/player").map(|h| h.what.as_str()).collect::<Vec<_>>(), [HLS_VIDEO]);
    assert!(hits.iter().any(|h| h.path.contains("/itag/95/") && h.path.ends_with(".m4s")));
}

#[test]
fn signed_in_the_accounts_playlists_come_with_one_item_per_video() {
    let Some(rig) = serve("signed") else { return eprintln!("{SKIP}") };
    // Signed in before, its access token since expired.
    rig.site.lock().unwrap().issued = 1;
    rig.store("tokens", json!({"access": "access-1", "refresh": "refresh-1", "expires_at": 0}));
    let ctx = config(json!({"count": 2}));
    let items = rig.call("items", std::slice::from_ref(&ctx)).unwrap();
    let hits = rig.hits();

    // Refreshed once, then every account call carries the new token.
    assert_eq!(hits.iter().filter(|h| h.what == "refresh_token").count(), 1);
    let listing: Vec<(&str, &str)> = hits.iter().filter(|h| h.auth.is_some() && h.path != "/youtubei/v1/player").map(|h| (h.path.as_str(), h.what.as_str())).collect();
    assert_eq!(listing, [
        ("/youtubei/v1/playlist/get_add_to_playlist", ""),
        ("/youtubei/v1/browse", "VLLL"),
        ("/youtubei/v1/browse", "VLWL"),
        ("/youtubei/v1/browse", "VLPLown"),
    ]);
    assert!(hits.iter().filter(|h| h.auth.is_some()).all(|h| h.auth.as_deref() == Some("Bearer access-2")));

    // Liked videos skips its live stream and its private video; SkVqJ1SGeL0 has no file, so waits.
    let listing = rig.stored("listing");
    let ids = |list: &Json| list["videos"].as_array().unwrap().iter().map(|v| v["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let lists: Vec<(&str, Vec<String>)> = listing["lists"].as_array().unwrap().iter().map(|l| (l["title"].as_str().unwrap(), ids(l))).collect();
    assert_eq!(lists, [
        ("Liked videos", vec!["l5OZu-IrXpw".to_string(), NO_FILE.to_string()]),
        ("Watch later", vec!["u9lj-c29dxI".to_string()]),
        ("My films", vec!["l5OZu-IrXpw".to_string(), "u9lj-c29dxI".to_string()]),
    ]);
    let items = items.as_array().unwrap();
    let got: Vec<(&str, &Json)> = items.iter().map(|i| (i["id"].as_str().unwrap(), &i["playlists"])).collect();
    assert_eq!(got, [
        ("yt-l5OZu-IrXpw", &json!({"Liked videos": 1, "My films": 1})),
        ("yt-u9lj-c29dxI", &json!({"Watch later": 1, "My films": 2})),
    ]);

    // Turning Liked videos off forgets what only it held.
    rig.call("items", &[config(json!({"count": 2, "liked": false}))]).unwrap();
    assert_eq!(rig.stored(&format!("skip:{NO_FILE}")), Json::Null);
    assert!(rig.stored("v:l5OZu-IrXpw").is_object());

    // YouTube refuses a file's link: the player script is looked up again, once, and the video waits.
    rig.inside("storage.delete('v:l5OZu-IrXpw'); return null;").unwrap();
    rig.hits();
    rig.site.lock().unwrap().refuse_files = true;
    rig.call("items", &[config(json!({"count": 2, "liked": false}))]).unwrap();
    let hits = rig.hits();
    assert_eq!(hits.iter().filter(|h| h.path == "/tv").count(), 1, "{hits:?}");
    assert_eq!(hits.iter().filter(|h| h.path == "/youtubei/v1/player").count(), 2);
    assert_eq!(rig.stored("skip:l5OZu-IrXpw")["why"], "YouTube refuses the file's link, also with the newest player script");

    // YouTube ends the session: the refresh is refused, and the addon asks to be signed in again.
    {
        let mut site = rig.site.lock().unwrap();
        site.refuse_refresh = true;
        site.issued += 1;
    }
    let error = rig.call("items", &[config(json!({"count": 4}))]).unwrap_err();
    assert!(error.starts_with("SIGN_IN_REQUIRED: YouTube refused the sign-in (Token has been expired or revoked.)"), "{error}");
}

#[test]
fn signing_in_with_a_code_keeps_the_tokens_and_names_the_selected_account() {
    let Some(rig) = serve("sign-in") else { return eprintln!("{SKIP}") };
    let ctx = config(json!({}));
    let started = rig.call("signIn", std::slice::from_ref(&ctx)).unwrap();
    assert_eq!(started, json!({"code": "ABC-DEF-GHI", "url": "https://www.google.com/device", "expires_in": 1800, "interval": 5, "state": {"device_code": "DEVICE-1"}}));
    assert_eq!(rig.call("signInPoll", &[started["state"].clone(), ctx.clone()]).unwrap(), json!({"done": false}));
    rig.site.lock().unwrap().approved = true;
    // Of the accounts YouTube lists, the selected one, which is not the first.
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
