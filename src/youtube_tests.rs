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
//! Real, recorded on 2026-11-11 from YouTube's own files of the public video l5OZu-IrXpw: the header
//! and index (`initRange` through `indexRange`) of its 1080p H.264 file (itag 137) and its AAC file
//! (itag 140), `*-header.bin`.
//!
//! Fabricated in the structure of what the signed-in account was answered on 2026-11-11, as no
//! answer from a real account is published: Liked videos and Watch later (`*.FABRICATED.json`),
//! the Samsung TV client's player answer (`tv-samsung-player-*.FABRICATED.json`, varied below per
//! video, its files' sizes and ranges those of the files FFmpeg makes here), and below: the TV
//! page, the OAuth answers, and the account's playlists and accounts.
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
/// A video with no H.264 picture (VP9 and AV1 only), so only its 360p file.
const NO_PAIR: &str = "_cMxraX_5RE";
/// A video whose picture and sound YouTube refuses (403) while its 360p file plays.
const BROKEN_PAIR: &str = "PVGeM40dABA";
/// The videos with files: its id and n challenge, and whether its links come as signatureCiphers.
const FILES: [(&str, &str, bool); 4] = [("l5OZu-IrXpw", N[0].0, false), ("u9lj-c29dxI", N[1].0, true), (NO_PAIR, N[0].0, false), (BROKEN_PAIR, N[0].0, false)];
/// A video whose answer has no file at all.
const NO_FILE: &str = "SkVqJ1SGeL0";
/// What the fabricated answer says of the 1080p picture and the sound: their average rates.
const RATES: (u64, u64) = (2_050_537, 128_227);

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
        "player" => include_str!("../tests/fixtures/youtube/tv-samsung-player-l5OZu-IrXpw.FABRICATED.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(text).unwrap()
}

/// Each top-level box of an ISO media file: (type, offset, size).
fn boxes(bytes: &[u8]) -> Vec<(String, usize, usize)> {
    let mut list = Vec::new();
    let mut at = 0;
    while at + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        list.push((String::from_utf8_lossy(&bytes[at + 4..at + 8]).into_owned(), at, size));
        at += size.max(8);
    }
    list
}

/// The Samsung TV client's answer for `video`, fabricated from the l5OZu-IrXpw one, its files'
/// sizes and ranges those of the files in `root`.
fn player_answer(base: &str, root: &Path, video: &str) -> Option<Json> {
    let mut template = fixture("player").to_string().replace("{BASE}", base).replace("o-FABRICATED", &format!("o-{video}"));
    for (kind, file) in [("V", "dash-video.mp4"), ("A", "dash-audio.mp4")] {
        let bytes = std::fs::read(root.join(file)).unwrap();
        let (_, sidx, size) = boxes(&bytes).into_iter().find(|(name, ..)| name == "sidx").unwrap();
        for (key, value) in [("INIT_END", sidx - 1), ("INDEX_START", sidx), ("INDEX_END", sidx + size - 1), ("LEN", bytes.len())] {
            template = template.replace(&format!("{{{kind}{key}}}"), &value.to_string());
        }
    }
    let mut answer: Json = serde_json::from_str(&template).unwrap();
    answer["videoDetails"]["videoId"] = json!(video);
    if video == NO_FILE {
        answer["streamingData"]["formats"] = json!([]);
        answer["streamingData"]["adaptiveFormats"] = json!([]);
        return Some(answer);
    }
    let &(_, challenge, ciphered) = FILES.iter().find(|(id, ..)| *id == video)?;
    if video == NO_PAIR {
        answer["streamingData"]["adaptiveFormats"].as_array_mut().unwrap().retain(|f| !f["mimeType"].as_str().unwrap().contains("avc1"));
    }
    let formats = answer["streamingData"].as_object_mut().unwrap();
    for format in formats.values_mut().filter_map(Json::as_array_mut).flatten() {
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
    /// The InnerTube client and its version, else "".
    client: String,
    /// The browse id, "continuation", the video id, the OAuth grant, or a file's itag.
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
        "/youtubei/v1/player" => match player_answer(base, root, body["videoId"].as_str().unwrap_or("")) {
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
            let itag = params.get("itag").map_or("", String::as_str);
            let n_right = FILES.iter().any(|(id, challenge, _)| *id == video && params.get("n").map(String::as_str) == N.iter().find(|(c, _)| c == challenge).map(|(_, solved)| *solved));
            let sig_right = FILES.iter().any(|(id, _, ciphered)| *id == video && (!ciphered || params.get("sig").map(String::as_str) == Some(SIG.1)));
            if site.refuse_files || !n_right || !sig_right || (video == BROKEN_PAIR && itag != "18") {
                return (403, Vec::new(), "text/plain", None);
            }
            let file = match itag {
                "18" => "muxed.mp4",
                "140" | "251" => "dash-audio.mp4",
                _ => "dash-video.mp4",
            };
            let bytes = std::fs::read(root.join(file)).unwrap();
            let total = bytes.len();
            let Some((start, end)) = range.and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split_once('-')) else {
                return (200, bytes, "video/mp4", None);
            };
            let start: usize = start.parse().unwrap_or(0);
            let end = end.parse::<usize>().map_or(total - 1, |end| end.min(total - 1));
            (206, bytes[start..=end].to_vec(), "video/mp4", Some(format!("bytes {start}-{end}/{total}")))
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
    // As YouTube's: the 360p file, H.264 + AAC in one MP4; and picture and sound apart, each an
    // MP4 of 2 s fragments with its index (`sidx`) after its header.
    for args in [
        "-v error -f lavfi -i testsrc=size=640x268:rate=24 -f lavfi -i sine=frequency=440 -t 4 -c:v libx264 -profile:v baseline -pix_fmt yuv420p -preset ultrafast -c:a aac -movflags +faststart muxed.mp4",
        "-v error -f lavfi -i testsrc=size=320x134:rate=24 -t 6 -c:v libx264 -preset ultrafast -g 48 -keyint_min 48 -sc_threshold 0 -pix_fmt yuv420p -f mp4 -movflags dash+global_sidx -frag_duration 2000000 dash-video.mp4",
        "-v error -f lavfi -i sine=frequency=440 -t 6 -c:a aac -f mp4 -movflags dash+global_sidx -frag_duration 2000000 dash-audio.mp4",
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
            let itag = url::form_urlencoded::parse(url.split_once('?').map_or("", |(_, q)| q).as_bytes()).find(|(k, _)| k == "itag").map(|(_, v)| v.into_owned());
            let what = ["browseId", "videoId", "grant_type"].iter().find_map(|k| body[k].as_str().map(str::to_string))
                .or(body.get("continuation").map(|_| "continuation".to_string()))
                .or(itag)
                .unwrap_or_default();
            let client = &body["context"]["client"];
            let hit = Hit {
                path: path.clone(),
                client: client["clientName"].as_str().map_or(String::new(), |name| format!("{name} {}", client["clientVersion"].as_str().unwrap_or(""))),
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
        let env = Env { addon: "youtube", source, hosts: &self.hosts, storage: self.storage.clone(), prober: self.app.prober.clone(), spotify: None };
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
fn a_public_playlist_becomes_a_playlist_of_its_channels_episodes_played_up_to_1080p() {
    let Some(rig) = serve("public") else { return eprintln!("{SKIP}") };
    rig.sign_in();
    // 16 videos: the first page's 15 and one of the continuation's.
    let playlist = json!([format!("https://www.youtube.com/playlist?list={PLAYLIST}")]);
    let at = |quality: u32| config(json!({"count": 16, "max_height": quality, "liked": false, "watch_later": false, "own": false, "playlists": playlist}));
    let ctx = at(1080);
    let items = rig.call("items", std::slice::from_ref(&ctx)).unwrap();
    let hits = rig.hits();

    let listed: Vec<Json> = rig.stored("listing")["lists"][0]["videos"].as_array().unwrap().iter().map(|v| v["id"].clone()).collect();
    assert_eq!(Json::from(listed), json!([
        "l5OZu-IrXpw", "u9lj-c29dxI", "UXqq0ZvbOnk", "_cMxraX_5RE", "PVGeM40dABA", "WhWc3b3KhnY", "pKmSdY56VtY", "apiu3pTIwuY",
        "mN0zPOpADL4", "SkVqJ1SGeL0", "lqiN98z6Dak", "Y-rmzh0PI3c", "Z4C82eyhwgU", "R6MlUcmOul8", "eRsGyueVLvQ", "YE7VzlLtp-4",
    ]));
    // Every InnerTube call is signed in: the playlist listed as the current TV app, each video's
    // player answer asked for once as the Samsung TV app, with the player script's signature
    // timestamp, which was fetched once.
    let innertube: Vec<&Hit> = hits.iter().filter(|h| h.path.starts_with("/youtubei/")).collect();
    assert!(innertube.iter().all(|h| h.auth.as_deref() == Some("Bearer access-1")), "{innertube:?}");
    let browses: Vec<(&str, &str)> = innertube.iter().filter(|h| h.path == "/youtubei/v1/browse").map(|h| (h.what.as_str(), h.client.as_str())).collect();
    assert_eq!(browses, [(format!("VL{PLAYLIST}").as_str(), "TVHTML5 7.20250923.13.00"), ("continuation", "TVHTML5 7.20250923.13.00")]);
    let players: Vec<&Hit> = innertube.iter().copied().filter(|h| h.path == "/youtubei/v1/player").collect();
    assert_eq!(players.len(), 16);
    assert!(players.iter().all(|h| h.sts == Some(20761) && h.client == "TVHTML5 5.20260707"), "{players:?}");
    assert_eq!(hits.iter().filter(|h| h.path == "/tv" || h.path == PLAYER).count(), 2);

    // Two videos play from their 1080p H.264 picture and AAC sound (one with signatures to
    // transform too); one without H.264 and one whose pair YouTube refuses from their 360p files.
    let items = items.as_array().unwrap();
    assert_eq!(items.iter().map(|i| i["id"].as_str().unwrap()).collect::<Vec<_>>(), ["yt-l5OZu-IrXpw", "yt-u9lj-c29dxI", "yt-_cMxraX_5RE", "yt-PVGeM40dABA"]);
    let paths: Vec<&str> = items.iter().map(|i| i["path"].as_str().unwrap()).collect();
    assert_eq!(paths, [
        "youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/l5OZu-IrXpw.m3u8", "youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/u9lj-c29dxI.m3u8",
        "youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/_cMxraX_5RE.mp4", "youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/PVGeM40dABA.mp4",
    ]);
    let first = &items[0];
    assert_eq!((first["kind"].as_str(), first["season"].as_i64(), first["episode"].as_i64()), (Some("episode"), Some(1), Some(1)));
    assert_eq!(items.iter().map(|i| i["episode"].as_i64().unwrap()).collect::<Vec<_>>(), [1, 2, 3, 4]);
    // Titled from the playlist's tile; the channel's name from its first video's tile, for every episode.
    assert_eq!(first["release"], "youtube:UCz75RVbH8q2jdBJ4SnwuZZQ");
    assert_eq!(first["display"]["title"], "SINGULARITY - Painterly Space Adventure");
    assert_eq!(items[1]["display"]["title"], "WING IT! - Blender Open Movie");
    assert_eq!(first["display"]["date"], "2026-05-11");
    assert_eq!(first["display"]["thumb"], "https://i.ytimg.com/vi/l5OZu-IrXpw/hqdefault.jpg");
    for item in items {
        assert_eq!(item["display"]["show"], json!({"title": "Blender Studio and Blender", "poster": "https://i.ytimg.com/vi/l5OZu-IrXpw/hqdefault.jpg"}));
    }
    assert_eq!(first["playlists"], json!({"Official Blender Open Movies": 1}));
    // The pair probed as the HLS presentation rex reads, through the relay, with YouTube's rates;
    // the 360p file as the file.
    let streams = |item: &Json| item["media_info"]["streams"].as_array().unwrap().iter().map(|s| (s["codec_name"].as_str().unwrap().to_string(), s["bit_rate"].as_str().unwrap_or("").to_string())).collect::<Vec<_>>();
    assert_eq!(first["media_info"]["format"]["format_name"], "hls");
    assert_eq!(first["media_info"]["format"]["bit_rate"], (RATES.0 + RATES.1).to_string());
    assert_eq!(streams(first), [("aac".to_string(), RATES.1.to_string()), ("h264".to_string(), RATES.0.to_string())]);
    assert!(first["media_info"]["format"]["duration"].as_str().unwrap().parse::<f64>().unwrap() > 5.0);
    assert_eq!(items[1]["media_info"]["format"]["format_name"], "hls");
    assert!(items[2]["media_info"]["format"]["format_name"].as_str().unwrap().contains("mp4"));
    assert_eq!(streams(&items[2]).iter().map(|(codec, _)| codec.as_str()).collect::<Vec<_>>(), ["h264", "aac"]);
    let checked = rig.stored("v:l5OZu-IrXpw");
    assert_eq!((&checked["kind"], &checked["itags"], &checked["quality"], &checked["height"]), (&json!("pair"), &json!([137, 140]), &json!(1080), &json!(804)));
    assert_eq!(rig.stored("v:_cMxraX_5RE")["kind"], "mp4");
    // Each pair's files were read for their index and by the probe; the refused pair's not past its index.
    let reads = |video_itag: &str| hits.iter().filter(|h| h.path == "/videoplayback" && h.what == video_itag).count();
    assert!(reads("137") >= 4 && reads("140") >= 4, "{hits:?}");
    assert_eq!(rig.stored(&format!("skip:{NO_FILE}"))["why"], "YouTube offered neither H.264 with AAC nor a 360p file");

    // The next sync lists nothing and asks about nothing: what was checked is kept, what failed waits.
    assert_eq!(rig.call("items", std::slice::from_ref(&ctx)).unwrap().as_array().unwrap().len(), 4);
    assert!(rig.hits().is_empty());

    // Playing asks again for the files, their links fixed as the player fixes them, and writes the
    // presentation: its two indexes read again, nothing else.
    let opened = rig.call("open", &["yt-l5OZu-IrXpw".into(), ctx.clone()]).unwrap();
    let hits = rig.hits();
    assert_eq!(hits.iter().map(|h| (h.path.as_str(), h.what.as_str())).collect::<Vec<_>>(), [("/youtubei/v1/player", "l5OZu-IrXpw"), ("/videoplayback", "137"), ("/videoplayback", "140")]);
    let master = opened["playlist"].as_str().unwrap();
    assert!(master.contains(r#"#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="Original",DEFAULT=YES,AUTOSELECT=YES,URI="audio.m3u8""#), "{master}");
    assert!(master.contains(&format!(r#"#EXT-X-STREAM-INF:BANDWIDTH={},CODECS="avc1.640028,mp4a.40.2",RESOLUTION=1920x804,FRAME-RATE=24,AUDIO="audio""#, 3_228_918 + 130_672)), "{master}");
    assert!(master.ends_with("\nvideo.m3u8\n"), "{master}");
    let video = opened["playlists"]["video.m3u8"].as_str().unwrap();
    let link = format!("{}/videoplayback?", rig.base);
    let map = video.lines().find(|l| l.starts_with("#EXT-X-MAP:")).unwrap();
    assert!(map.starts_with(&format!(r#"#EXT-X-MAP:URI="{link}"#)) && map.contains("itag=137") && map.contains(&format!("&n={}&", N[0].1)), "{map}");
    // 6 s of 2 s fragments, the first said to be 0.25 s shorter (EARLY).
    let durations: Vec<&str> = video.lines().filter_map(|l| l.strip_prefix("#EXTINF:")).collect();
    assert_eq!(durations, ["1.750000,", "2.000000,", "2.000000,"]);
    assert!(video.ends_with("#EXT-X-ENDLIST\n"));
    assert!(opened["playlists"]["audio.m3u8"].as_str().unwrap().contains("itag=140"));
    // The link from the index on, every segment of it.
    let ranges: Vec<&str> = video.lines().filter_map(|l| l.strip_prefix("#EXT-X-BYTERANGE:")).collect();
    let bytes = std::fs::read(rig.root.join("dash-video.mp4")).unwrap();
    let after_index = boxes(&bytes).iter().find(|(name, ..)| name == "sidx").map(|(_, at, size)| at + size).unwrap();
    let (first_size, first_at) = ranges[0].split_once('@').unwrap();
    assert_eq!(first_at.parse::<usize>().unwrap(), after_index);
    // FFmpeg ends its file with an `mfra` box YouTube's have not; the index covers the media.
    let media_end = boxes(&bytes).iter().filter(|(name, ..)| name == "mdat").map(|(_, at, size)| at + size).max().unwrap();
    let (last_size, last_at) = ranges[2].split_once('@').unwrap();
    assert_eq!(last_at.parse::<usize>().unwrap() + last_size.parse::<usize>().unwrap(), media_end, "{first_size}");
    let opened = rig.call("open", &["yt-u9lj-c29dxI".into(), ctx.clone()]).unwrap();
    assert!(opened["playlists"]["video.m3u8"].as_str().unwrap().contains(&format!("&sig={}", SIG.1)), "{opened}");
    let opened = rig.call("open", &[format!("yt-{NO_PAIR}").into(), ctx.clone()]).unwrap();
    assert!(opened["url"].as_str().unwrap().contains("itag=18") && opened.get("playlist").is_none(), "{opened}");
    let error = rig.call("open", &[format!("yt-{NO_FILE}").into(), ctx.clone()]).unwrap_err();
    assert!(error.contains(&format!("{NO_FILE} does not play: YouTube offered neither H.264 with AAC nor a 360p file")), "{error}");

    // 720 checks every video again, at 720p; the numbers stay.
    rig.hits();
    let items = rig.call("items", &[at(720)]).unwrap();
    assert_eq!(items.as_array().unwrap().iter().map(|i| i["episode"].as_i64().unwrap()).collect::<Vec<_>>(), [1, 2, 3, 4]);
    assert_eq!(rig.hits().iter().filter(|h| h.path == "/youtubei/v1/player").count(), 4);
    assert_eq!(rig.stored("v:l5OZu-IrXpw")["itags"], json!([136, 140]));
    // 360 plays each video's 360p file as it is.
    let items = rig.call("items", &[at(360)]).unwrap();
    assert_eq!(items[0]["path"], "youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/l5OZu-IrXpw.mp4");
    assert_eq!(streams(&items[0]).iter().map(|(codec, _)| codec.as_str()).collect::<Vec<_>>(), ["h264", "aac"]);
    assert_eq!(rig.call("open", &["yt-l5OZu-IrXpw".into(), at(360)]).unwrap()["url"].as_str().map(|u| u.contains("itag=18")), Some(true));
}

#[test]
fn the_sharpest_h264_picture_within_the_setting_and_the_plain_aac_sound() {
    let Some(rig) = serve("choice") else { return eprintln!("{SKIP}") };
    let answer = player_answer(&rig.base, &rig.root, "l5OZu-IrXpw").unwrap();
    let pick = |answer: &Json, most: &str| rig.inside(&format!(
        "const p = pair({answer}, {most}); return p && [p.video.itag, p.audio.itag, p.audio.isDrc ?? false, p.audio.xtags ?? null];"
    )).unwrap();
    // VP9, AV1 and 4K are passed over; 140 with its loudness evened out or its voices raised too.
    assert_eq!(pick(&answer, "1080"), json!([137, 140, false, null]));
    assert_eq!(pick(&answer, "Infinity"), json!([137, 140, false, null]));
    assert_eq!(pick(&answer, "1079"), json!([136, 140, false, null]));
    assert_eq!(pick(&answer, "720"), json!([136, 140, false, null]));
    assert_eq!(pick(&answer, "480"), json!([135, 140, false, null]));
    assert_eq!(pick(&answer, "360"), json!([134, 140, false, null]));
    assert_eq!(pick(&answer, "240"), Json::Null);
    // A dub in another language is never the sound; the video's own is.
    let mut dubbed = answer.clone();
    let formats = dubbed["streamingData"]["adaptiveFormats"].as_array_mut().unwrap();
    let mut dub = formats.iter().find(|f| f["itag"] == 140).unwrap().clone();
    dub["audioTrack"] = json!({"displayName": "German", "id": "de.4", "audioIsDefault": false});
    formats.insert(0, dub);
    for f in formats.iter_mut().filter(|f| f["itag"] == 140 && f.get("audioTrack").is_none()) {
        f["audioTrack"] = json!({"displayName": "English original", "id": "en.4", "audioIsDefault": true});
    }
    assert_eq!(rig.inside(&format!("return pair({dubbed}, 1080).audio.audioTrack.id;")).unwrap(), "en.4");
    // No H.264, or no AAC: no pair.
    let without = |keep: &str| {
        let mut answer = answer.clone();
        answer["streamingData"]["adaptiveFormats"].as_array_mut().unwrap().retain(|f| !f["mimeType"].as_str().unwrap().contains(keep));
        answer
    };
    assert_eq!(pick(&without("avc1"), "1080"), Json::Null);
    assert_eq!(pick(&without("mp4a"), "1080"), Json::Null);
    // As checked: exactly those files, whatever the setting.
    assert_eq!(rig.inside(&format!("const p = pair({answer}, 360, [137, 140]); return [p.video.itag, p.audio.itag];")).unwrap(), json!([137, 140]));
}

#[test]
fn a_real_youtube_files_index_cuts_it_into_its_segments_and_playlist() {
    let Some(rig) = serve("sidx") else { return eprintln!("{SKIP}") };
    // l5OZu-IrXpw's 1080p H.264 file (100,218,102 bytes, its index at 741-1660) and its AAC file
    // (6,329,139 bytes, its index at 723-1234), as YouTube's indexRange gave them.
    // Their lengths are YouTube's own (approxDurationMs 390958 and 391024).
    for (header, index, total, count, init, length, shortest, longest) in [
        (&include_bytes!("../tests/fixtures/youtube/l5OZu-IrXpw-137-header.bin")[..], 741, 100_218_102u64, 74, 741, "390.958", "3.250", "5.750"),
        (&include_bytes!("../tests/fixtures/youtube/l5OZu-IrXpw-140-header.bin")[..], 723, 6_329_139, 40, 723, "391.024", "1.625", "9.985"),
    ] {
        let got = rig.inside(&format!(
            "const bytes = Uint8Array.from(atob({:?}), (c) => c.charCodeAt(0)); const list = segments(bytes, {index});
             const playlist = mediaPlaylist('https://r1.googlevideo.com/videoplayback?itag=1', {init}, list);
             return [list.length, list[0].offset, list.at(-1).offset + list.at(-1).size, list.reduce((sum, s) => sum + s.duration, 0).toFixed(3),
               Math.min(...list.map((s) => s.duration)).toFixed(3), Math.max(...list.map((s) => s.duration)).toFixed(3), playlist.split('\\n').slice(0, 9)];",
            base64(header)
        )).unwrap();
        let header_end = header.len() as u64;
        assert_eq!(got[0], count);
        // The first segment starts right after the index, the last ends the file.
        assert_eq!(got[1], header_end);
        assert_eq!(got[2], total);
        assert_eq!((&got[3], &got[4], &got[5]), (&json!(length), &json!(shortest), &json!(longest)), "{got}");
        let lines: Vec<&str> = got[6].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        assert_eq!(lines[..6], ["#EXTM3U", "#EXT-X-VERSION:7", &format!("#EXT-X-TARGETDURATION:{}", got[5].as_str().unwrap().parse::<f64>().unwrap().ceil()), "#EXT-X-PLAYLIST-TYPE:VOD",
            "#EXT-X-MEDIA-SEQUENCE:0", &format!(r#"#EXT-X-MAP:URI="https://r1.googlevideo.com/videoplayback?itag=1",BYTERANGE="{init}@0""#)]);
        assert!(lines[7].starts_with("#EXT-X-BYTERANGE:") && lines[7].ends_with(&format!("@{header_end}")), "{lines:?}");
        assert_eq!(lines[8], "https://r1.googlevideo.com/videoplayback?itag=1");
    }
    // Not an index, or one cut short.
    for (js, want) in [("new Uint8Array(64)", "no index where YouTube says the file's index is"), ("Uint8Array.from(atob(HEADER), (c) => c.charCodeAt(0)).subarray(0, 800)", "the file's index is cut short")] {
        let header = base64(include_bytes!("../tests/fixtures/youtube/l5OZu-IrXpw-137-header.bin"));
        let error = rig.inside(&format!("const HEADER = {header:?}; return segments({js}, 741);")).unwrap_err();
        assert!(error.starts_with(want), "{error}");
    }
}

/// Standard base64, for handing bytes to the addon's JavaScript.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    bytes.chunks(3).map(|c| {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        (0..4).map(|i| if i <= c.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' }).collect::<String>()
    }).collect()
}

#[test]
fn an_update_from_1_0_keeps_what_it_checked_until_it_is_checked_again() {
    let Some(rig) = serve("update") else { return eprintln!("{SKIP}") };
    rig.sign_in();
    // What 1.0.0 kept: a video checked at 360p (no quality), one given YouTube's HLS stream.
    let old_probe = json!({"format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2"}, "streams": [{"index": 0, "codec_name": "h264", "width": 640}, {"index": 1, "codec_name": "aac"}]});
    rig.store("v:l5OZu-IrXpw", json!({"title": "SINGULARITY", "channel": "Blender", "channel_id": "UCz75RVbH8q2jdBJ4SnwuZZQ", "kind": "mp4", "height": 268, "probe": old_probe, "episode": 7}));
    rig.store("v:u9lj-c29dxI", json!({"title": "WING IT!", "channel": "Blender", "channel_id": "UCz75RVbH8q2jdBJ4SnwuZZQ", "kind": "hls", "height": 720, "most": 1080, "probe": old_probe, "episode": 8}));
    rig.store("numbers", json!({"UCz75RVbH8q2jdBJ4SnwuZZQ": 8}));
    let ctx = config(json!({"count": 2, "liked": false, "watch_later": false, "own": false, "playlists": [format!("https://www.youtube.com/playlist?list={PLAYLIST}")]}));
    // With no checks left this sync, the 360p video plays on as it did, its probe its file's; the HLS one waits.
    let none_left = rig.source().replace("const CHECKS_PER_SYNC = 30;", "const CHECKS_PER_SYNC = 0;");
    let items = rig.run(&none_left, "items", std::slice::from_ref(&ctx)).unwrap();
    assert_eq!(items.as_array().unwrap().len(), 1);
    assert_eq!((&items[0]["path"], &items[0]["media_info"], &items[0]["episode"]), (&json!("youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/l5OZu-IrXpw.mp4"), &old_probe, &json!(7)));
    assert!(!rig.hits().iter().any(|h| h.path == "/youtubei/v1/player"));
    // Checked again, both play from their pair, with probes of it, keeping their numbers.
    let items = rig.call("items", std::slice::from_ref(&ctx)).unwrap();
    let got: Vec<(&str, &str, i64)> = items.as_array().unwrap().iter()
        .map(|i| (i["path"].as_str().unwrap(), i["media_info"]["format"]["format_name"].as_str().unwrap(), i["episode"].as_i64().unwrap())).collect();
    assert_eq!(got, [("youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/l5OZu-IrXpw.m3u8", "hls", 7), ("youtube/UCz75RVbH8q2jdBJ4SnwuZZQ/u9lj-c29dxI.m3u8", "hls", 8)]);
    // Still signed in with what 1.0.0 kept.
    assert_eq!(rig.stored("tokens")["refresh"], "refresh-1");
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
