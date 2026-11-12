//! The spotify addon against a stand-in for Spotify's own API (the `spotify` global) and for the
//! YouTube addon (`addons.call`), in the sandbox it runs in.
//!
//! Every Spotify answer here is FABRICATED, built in the structure the owner's account was answered
//! in on 2026-11-11 (rootlist, playlist, collection paging, track, album and artist metadata) from
//! public catalogue data: no answer from a real account is published. The ids are made up.
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as Json};

use crate::net::Hosts;
use crate::probe::Prober;
use crate::sandbox::{self, Bridge, Env, Storage};

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

/// Spotify's answer to `GET path`, or to `POST path` with `body`: what the account holds, FABRICATED.
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
    fn answer(&mut self, method: &str, path: &str, body: Option<&Json>) -> Result<Json, String> {
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
            ("GET", ["", "playlist", "v2", "playlist", id]) if *id == ids(14).0 => Err(format!("Spotify answered 403 to {route}")),
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
                let n = by_gid(gid).ok_or("no such track")?;
                if self.gone == Some(n) {
                    return Err(format!("Spotify answered 404 to {route}"));
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
                let n = by_gid(gid).ok_or("no such album")?;
                let tracks: Vec<Json> = TRACKS.iter().filter(|t| t.album == n).map(|t| json!({"gid": ids(t.n).1})).collect();
                Ok(json!({"gid": gid, "name": "an album", "disc": [{"number": 1, "track": tracks}], "cover_group": cover(n)}))
            }
            ("GET", ["", "metadata", "4", "artist", gid]) => {
                Ok(json!({"gid": gid, "name": "an artist", "portrait_group": {"image": [{"file_id": format!("ab6761610000e5eb{gid}")[..40].to_string(), "size": "LARGE", "width": 640, "height": 640}]}}))
            }
            _ => Err(format!("Spotify answered 404 to {route}")),
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

struct Rig {
    account: Arc<Mutex<Account>>,
    youtube: Arc<Mutex<YouTube>>,
    storage: Arc<Storage>,
    source: String,
}

fn rig(name: &str) -> Rig {
    let storage = std::env::temp_dir().join(format!("rex-addons-spotify-{name}-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&storage);
    let source = include_str!("../addons/spotify/main.js");
    for constant in ["const SPOTIFY_PAUSE_MS = 250;", "const MATCH_PAUSE_MS = 1500;"] {
        assert!(source.contains(constant), "main.js no longer has {constant}");
    }
    Rig {
        account: Arc::new(Mutex::new(Account {
            // Two tracks, a local file, a podcast episode, the first track again and one more.
            playlist_one: vec![uri("track", 1), uri("track", 3), "spotify:local:Artist:Album:Home+demo:241".into(), uri("episode", 40), uri("track", 1), uri("track", 4)],
            collection: vec![(uri("album", 102), "1700000000"), (uri("track", 1), "1700000200"), (uri("track", 5), "1700000300")],
            gone: None,
            seen: Vec::new(),
        })),
        youtube: Arc::default(),
        storage: Arc::new(Storage::open(storage)),
        source: source.replace("const SPOTIFY_PAUSE_MS = 250;", "const SPOTIFY_PAUSE_MS = 0;").replace("const MATCH_PAUSE_MS = 1500;", "const MATCH_PAUSE_MS = 0;"),
    }
}

impl Rig {
    fn call(&self, method: &str, args: &[Json]) -> Result<Json, String> {
        self.run(&self.source, method, args)
    }

    fn run(&self, source: &str, method: &str, args: &[Json]) -> Result<Json, String> {
        let account = self.account.clone();
        let youtube = self.youtube.clone();
        let spotify: Bridge = Arc::new(move |method: &str, path: &str, body: Vec<Json>| match method {
            "USER" => Ok(json!("someone")),
            _ => account.lock().unwrap().answer(method, path, body.first()),
        });
        let calls: Bridge = Arc::new(move |id: &str, function: &str, args: Vec<Json>| {
            assert_eq!(id, "youtube");
            youtube.lock().unwrap().answer(function, args)
        });
        let env = Env {
            addon: "spotify", source, hosts: Hosts::default(), storage: self.storage.clone(),
            prober: Arc::new(Prober::new(None, "127.0.0.1:9".parse().unwrap())), spotify: Some(spotify), calls: Some(calls),
        };
        sandbox::call(&env, method, args, sandbox::ITEMS)
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
}

fn ids_of(items: &[Json]) -> Vec<String> {
    items.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect()
}

fn item_id(n: u128) -> String {
    format!("sp-{}", ids(n).0)
}

/// The library listed (folders, windows of a long playlist, the collection's pages), each track
/// once with Spotify's details and its place in each playlist, only once YouTube has its recording,
/// a bounded number looked for each sync, and a track YouTube has none of kept from being asked again.
#[test]
fn the_library_becomes_matched_tracks_with_spotifys_details_and_playlists() {
    let rig = rig("library");
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
    let seen_before = rig.account.lock().unwrap().seen.len();
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
    let seen: Vec<String> = rig.account.lock().unwrap().seen[seen_before..].to_vec();
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
    let rig = rig("trouble");
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
    rig.account.lock().unwrap().gone = Some(4);
    let items = rig.items(json!({})).unwrap();
    assert_eq!(ids_of(&items), [item_id(1), item_id(3), item_id(2), item_id(6)]);
    assert_eq!(rig.stored(&format!("m:{}", ids(4).0)).unwrap()["none"], true);

    // A playlist read again whose window comes back empty.
    rig.js(&format!("storage.delete('pl:{}'); return null;", ids(11).0));
    rig.account.lock().unwrap().gone = Some(99);
    let error = rig.items(json!({})).unwrap_err();
    assert!(error.contains("with nothing"), "{error}");
}
