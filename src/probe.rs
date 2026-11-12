//! ffprobe: the host's own check of each new item, and the `ffprobe()` an addon calls. An
//! addon's probe never lets ffprobe reach the network itself: ffprobe reads a one-off relay on
//! this host, which fetches through the addon's sandboxed client (its hosts, no address on this
//! device or a private network, every redirect checked), and HLS playlists are rewritten so their
//! segments come through the relay too. Playlists an addon wrote itself are served by the relay
//! as they are checked, their links pointed through it.
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use crate::net::{self, Hosts};

/// How long one ffprobe may run.
pub const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct Relay {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub hosts: Hosts,
    /// Playlists the addon wrote, served by the relay instead of a link.
    pub playlists: Option<Arc<Playlists>>,
}

pub struct Prober {
    pub bin: Option<String>,
    base: String,
    relays: Mutex<HashMap<String, Relay>>,
    /// Whether this ffprobe's HLS reader takes `-extension_picky`.
    lenient: OnceLock<bool>,
}

fn not_available() -> String {
    "ffprobe() is not available: rex-addons runs without --ffprobe".into()
}

impl Prober {
    pub fn new(bin: Option<String>, listen: SocketAddr) -> Prober {
        Prober { bin, base: format!("http://{}", net::reachable(listen)), relays: Mutex::default(), lenient: OnceLock::new() }
    }

    /// An addon's `ffprobe(url, {headers})`.
    pub fn probe(&self, url: &str, headers: Vec<(String, String)>, hosts: &Hosts) -> Result<Json, String> {
        let bin = self.bin.as_deref().ok_or_else(not_available)?;
        let parsed = url::Url::parse(url).map_err(|e| format!("not a link: {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!("{}: only http and https", parsed.scheme()));
        }
        let host = parsed.host_str().unwrap_or("");
        if !hosts.allows(host) {
            return Err(format!("{host} is not in the addon's hosts"));
        }
        // The relay URL ends in the link's own file name, so ffprobe resolves a playlist's
        // relative segments under the relay.
        let name = parsed.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty()).unwrap_or("media").to_string();
        self.through_relay(bin, &name, Relay { url: url.to_string(), headers, hosts: hosts.clone(), playlists: None })
    }

    /// An addon's `ffprobe({playlist, playlists})`: playlists it wrote, as `open()` returns them.
    pub fn probe_playlists(&self, answer: &Json, hosts: &Hosts) -> Result<Json, String> {
        let bin = self.bin.as_deref().ok_or_else(not_available)?;
        let playlists = Playlists::parse(answer, hosts)?;
        self.through_relay(bin, MASTER, Relay { url: String::new(), headers: Vec::new(), hosts: hosts.clone(), playlists: Some(Arc::new(playlists)) })
    }

    fn through_relay(&self, bin: &str, name: &str, relay: Relay) -> Result<Json, String> {
        let token = token();
        self.relays.lock().unwrap().insert(token.clone(), relay);
        let mut args = vec!["-protocol_whitelist", "http,tcp,crypto"];
        // FFmpeg 7.1+ refuses an HLS segment whose URL does not end in its format's extension,
        // and the relay names a segment as its link does (`videoplayback`).
        let lenient = *self.lenient.get_or_init(|| {
            Command::new(bin).args(["-hide_banner", "-h", "demuxer=hls"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("extension_picky"))
        });
        if lenient {
            args.extend(["-extension_picky", "0"]);
        }
        let probed = run(bin, &format!("{}/relay/{token}/{name}", self.base), &args);
        self.relays.lock().unwrap().remove(&token);
        probed
    }

    /// A relay held open, as during a probe, for a test to read through.
    #[cfg(test)]
    pub fn hold(&self, relay: Relay) -> String {
        let token = token();
        self.relays.lock().unwrap().insert(token.clone(), relay);
        token
    }

    pub fn relay(&self, token: &str) -> Option<Relay> {
        self.relays.lock().unwrap().get(token).cloned()
    }
}

fn token() -> String {
    let mut bytes = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("no randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// ffprobe's JSON of `url`: format, streams and chapters. Killed after TIMEOUT.
pub fn run(bin: &str, url: &str, extra: &[&str]) -> Result<Json, String> {
    let mut child = Command::new(bin)
        .args(["-v", "error", "-rw_timeout", "30000000"])
        .args(extra)
        .args(["-print_format", "json", "-show_format", "-show_streams", "-show_chapters", url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{bin}: {e}"))?;
    let started = Instant::now();
    // Read its output on threads, so a large one cannot block it while we wait.
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr, &mut bytes);
        bytes
    });
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("ffprobe took longer than {} s", TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out.join().unwrap().map_err(|e| e.to_string())?;
    if !status.success() {
        let stderr = err.join().unwrap();
        let error = String::from_utf8_lossy(&stderr);
        let error = error.trim();
        return Err(error[error.len().saturating_sub(300)..].to_string());
    }
    let mut probe: Json = serde_json::from_slice(&stdout).map_err(|e| e.to_string())?;
    if !probe["format"].is_object() || probe["streams"].as_array().is_none_or(Vec::is_empty) {
        return Err("ffprobe found no streams".into());
    }
    // It names this host's own URL, which means nothing to rex.
    probe["format"].as_object_mut().unwrap().remove("filename");
    Ok(probe)
}

/// Every URI of an HLS playlist (each line that is no tag, and a tag's `URI="..."`) replaced by
/// what `each` makes of it; `each`'s first error is the answer.
pub fn map_uris(text: &str, mut each: impl FnMut(&str) -> Result<String, String>) -> Result<String, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        out.push(if line.trim().is_empty() {
            line.to_string()
        } else if !line.starts_with('#') {
            each(line.trim())?
        } else if let Some(at) = line.find("URI=\"") {
            let start = at + 5;
            match line[start..].find('"') {
                Some(len) => format!("{}{}{}", &line[..start], each(&line[start..start + len])?, &line[start + len..]),
                None => line.to_string(),
            }
        } else {
            line.to_string()
        });
    }
    Ok(out.join("\n"))
}

/// `abs/<the absolute URI, percent-encoded>/<its file name>` under the relay: FFmpeg's HLS reader
/// goes by the name's extension.
fn through(absolute: &url::Url, relay_base: &str) -> String {
    let name = absolute.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty()).unwrap_or("media");
    let encoded = percent_encoding::utf8_percent_encode(absolute.as_str(), percent_encoding::NON_ALPHANUMERIC);
    format!("{relay_base}/abs/{encoded}/{name}")
}

/// An HLS playlist with every URI in it, relative or not, pointed back through the relay.
pub fn rewrite_playlist(text: &str, playlist: &url::Url, relay_base: &str) -> String {
    map_uris(text, |uri| Ok(playlist.join(uri).map_or_else(|_| uri.to_string(), |absolute| through(&absolute, relay_base)))).unwrap_or_default()
}

/// The name the relay serves an addon's master playlist by.
pub const MASTER: &str = "master.m3u8";
/// The most an addon's playlists for one item may hold, all together: a three-hour video's two
/// media playlists of YouTube's links (about 1.4 KB a segment) are about 4 MB.
pub const MAX_PLAYLISTS: usize = 8 << 20;

/// An HLS presentation an addon wrote (`open()`'s `{playlist, playlists}`): a master playlist,
/// and the media playlists it names by their plain names.
#[derive(Debug)]
pub struct Playlists {
    pub master: String,
    pub children: BTreeMap<String, String>,
}

impl Playlists {
    /// Checked: HLS text, at most MAX_PLAYLISTS in all, children named `<letters, digits, - or
    /// _>.m3u8`, and every URI a child's name (in the master) or an http(s) link on `hosts`, which
    /// is all FFmpeg may then read.
    pub fn parse(answer: &Json, hosts: &Hosts) -> Result<Playlists, String> {
        let hls = |text: &str| text.trim_start().starts_with("#EXTM3U");
        let master = answer["playlist"].as_str().filter(|t| hls(t)).ok_or("playlist must be an HLS playlist's text, starting with #EXTM3U")?;
        let mut children = BTreeMap::new();
        match &answer["playlists"] {
            Json::Null => {}
            Json::Object(map) => {
                for (name, text) in map {
                    let plain = name.strip_suffix(".m3u8").is_some_and(|stem| {
                        !stem.is_empty() && stem.len() <= 64 && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    });
                    if !plain || name == MASTER {
                        return Err(format!("playlists: {name:?} is not a name such as video.m3u8 (nor {MASTER})"));
                    }
                    let text = text.as_str().filter(|t| hls(t)).ok_or(format!("playlists: {name} must be an HLS playlist's text"))?;
                    children.insert(name.clone(), text.to_string());
                }
            }
            _ => return Err("playlists must be an object of names to playlists' text".into()),
        }
        if master.len() + children.values().map(String::len).sum::<usize>() > MAX_PLAYLISTS {
            return Err(format!("the playlists hold more than {} MB", MAX_PLAYLISTS >> 20));
        }
        let on_hosts = |uri: &str| -> Result<String, String> {
            let link = url::Url::parse(uri).map_err(|_| format!("{uri:.80} is no playlist of its own and no http(s) link"))?;
            let host = link.host_str().unwrap_or("");
            if matches!(link.scheme(), "http" | "https") && hosts.allows(host) {
                Ok(uri.to_string())
            } else {
                Err(format!("a link in its playlists, {}://{host}/..., is not on the addon's hosts", link.scheme()))
            }
        };
        map_uris(master, |uri| if children.contains_key(uri) { Ok(uri.to_string()) } else { on_hosts(uri) })?;
        for text in children.values() {
            map_uris(text, on_hosts)?;
        }
        Ok(Playlists { master: master.to_string(), children })
    }

    /// The master with its children's names under `prefix`, its links as they are.
    pub fn master_under(&self, prefix: &str) -> String {
        map_uris(&self.master, |uri| Ok(if self.children.contains_key(uri) { format!("{prefix}{uri}") } else { uri.to_string() })).unwrap_or_default()
    }

    /// What the relay serves by `name`: the master as it is (its children beside it), a child with
    /// its links pointed through the relay; None for any other name.
    pub fn relayed(&self, name: &str, relay_base: &str) -> Option<String> {
        let relay = |uri: &str| Ok(url::Url::parse(uri).map_or_else(|_| uri.to_string(), |link| through(&link, relay_base)));
        if name == MASTER {
            return map_uris(&self.master, |uri| if self.children.contains_key(uri) { Ok(uri.to_string()) } else { relay(uri) }).ok();
        }
        map_uris(self.children.get(name)?, relay).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn playlists_point_back_through_the_relay() {
        let playlist = url::Url::parse("https://cdn.example.com/live/master.m3u8?sig=1").unwrap();
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:6,\nseg1.ts\nhttps://other.example.com/seg2.ts\n";
        let out = rewrite_playlist(text, &playlist, "http://127.0.0.1:32410/relay/T");
        assert_eq!(
            out,
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"http://127.0.0.1:32410/relay/T/abs/https%3A%2F%2Fcdn%2Eexample%2Ecom%2Flive%2Fkey%2Ebin/key.bin\"\n#EXTINF:6,\n\
             http://127.0.0.1:32410/relay/T/abs/https%3A%2F%2Fcdn%2Eexample%2Ecom%2Flive%2Fseg1%2Ets/seg1.ts\n\
             http://127.0.0.1:32410/relay/T/abs/https%3A%2F%2Fother%2Eexample%2Ecom%2Fseg2%2Ets/seg2.ts"
        );
    }

    fn media(url: &str) -> String {
        format!("#EXTM3U\n#EXT-X-MAP:URI=\"{url}\",BYTERANGE=\"10@0\"\n#EXTINF:5.0,\n#EXT-X-BYTERANGE:100@10\n{url}\n#EXT-X-ENDLIST\n")
    }

    const MASTER_TEXT: &str = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Original\",URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=9,AUDIO=\"a\"\nvideo.m3u8\n";

    #[test]
    fn an_addons_playlists_are_served_as_written_their_children_beside_them() {
        let hosts = Hosts(vec!["*.cdn.example.com".into()]);
        let link = "https://r1.cdn.example.com/videoplayback?itag=137&n=x";
        let answer = json!({"playlist": MASTER_TEXT, "playlists": {"video.m3u8": media(link), "audio.m3u8": media(link)}});
        let playlists = Playlists::parse(&answer, &hosts).unwrap();
        // At the item's path, its children under it: relative to `.../a%20film.m3u8`, FFmpeg reads `.../a%20film.m3u8/video.m3u8`.
        let master = playlists.master_under("a%20film.m3u8/");
        assert_eq!(master, MASTER_TEXT.trim_end().replace("URI=\"audio.m3u8\"", "URI=\"a%20film.m3u8/audio.m3u8\"").replace("\nvideo.m3u8", "\na%20film.m3u8/video.m3u8"));
        let base = url::Url::parse("http://192.168.1.5:32410/bytes/demo/films/a%20film.m3u8").unwrap();
        assert_eq!(base.join("a%20film.m3u8/video.m3u8").unwrap().as_str(), "http://192.168.1.5:32410/bytes/demo/films/a%20film.m3u8/video.m3u8");
        // Through the relay: the master as written, each child's links through the relay.
        assert_eq!(playlists.relayed(MASTER, "http://127.0.0.1:9/relay/T").unwrap(), MASTER_TEXT.trim_end());
        let child = playlists.relayed("video.m3u8", "http://127.0.0.1:9/relay/T").unwrap();
        let relayed = "http://127.0.0.1:9/relay/T/abs/https%3A%2F%2Fr1%2Ecdn%2Eexample%2Ecom%2Fvideoplayback%3Fitag%3D137%26n%3Dx/videoplayback";
        assert_eq!(child, media(relayed).trim_end());
        assert_eq!(playlists.relayed("other.m3u8", "http://127.0.0.1:9/relay/T"), None);
    }

    #[test]
    fn an_addons_playlists_are_refused_unless_every_link_is_on_its_hosts() {
        let hosts = Hosts(vec!["cdn.example.com".into()]);
        let good = media("https://cdn.example.com/v.mp4");
        let refused = |answer: Json| Playlists::parse(&answer, &hosts).unwrap_err();
        let cases = [
            (json!({"playlist": MASTER_TEXT, "playlists": {"video.m3u8": media("https://evil.example/v.mp4"), "audio.m3u8": good}}), "https://evil.example/..., is not on the addon's hosts"),
            (json!({"playlist": MASTER_TEXT, "playlists": {"video.m3u8": media("http://127.0.0.1:32400/rex/sources"), "audio.m3u8": good}}), "http://127.0.0.1/..., is not on the addon's hosts"),
            (json!({"playlist": MASTER_TEXT, "playlists": {"video.m3u8": media("file:///etc/passwd"), "audio.m3u8": good}}), "file:///..., is not on the addon's hosts"),
            // A relative link would be read from rex-addons itself.
            (json!({"playlist": MASTER_TEXT, "playlists": {"video.m3u8": media("v.mp4"), "audio.m3u8": good}}), "v.mp4 is no playlist of its own and no http(s) link"),
            // The master names a playlist it was not given.
            (json!({"playlist": MASTER_TEXT, "playlists": {"video.m3u8": good}}), "audio.m3u8 is no playlist of its own"),
            (json!({"playlist": MASTER_TEXT, "playlists": {"../video.m3u8": good}}), "\"../video.m3u8\" is not a name such as video.m3u8"),
            (json!({"playlist": MASTER_TEXT, "playlists": {"master.m3u8": good}}), "is not a name such as video.m3u8 (nor master.m3u8)"),
            (json!({"playlist": "<html>", "playlists": {}}), "playlist must be an HLS playlist's text"),
            (json!({"playlist": MASTER_TEXT, "playlists": {"video.m3u8": "{}"}}), "video.m3u8 must be an HLS playlist's text"),
            (json!({"playlist": MASTER_TEXT, "playlists": ["video.m3u8"]}), "playlists must be an object"),
            (json!({"playlist": format!("{MASTER_TEXT}{}", "#".repeat(MAX_PLAYLISTS)), "playlists": {"video.m3u8": good, "audio.m3u8": good}}), "more than 8 MB"),
        ];
        for (answer, want) in cases {
            let error = refused(answer);
            assert!(error.contains(want), "{error} lacks {want}");
        }
    }

    #[test]
    fn without_ffprobe_or_off_the_hosts() {
        let prober = Prober::new(None, "127.0.0.1:32410".parse().unwrap());
        let hosts = Hosts(vec!["cdn.example.com".into()]);
        assert_eq!(prober.probe("https://cdn.example.com/a.mp4", vec![], &hosts).unwrap_err(), "ffprobe() is not available: rex-addons runs without --ffprobe");
        let prober = Prober::new(Some("ffprobe".into()), "127.0.0.1:32410".parse().unwrap());
        assert_eq!(prober.probe("https://elsewhere.example.org/a.mp4", vec![], &hosts).unwrap_err(), "elsewhere.example.org is not in the addon's hosts");
        assert_eq!(prober.probe("file:///etc/passwd", vec![], &hosts).unwrap_err(), "file: only http and https");
        let playlists = json!({"playlist": MASTER_TEXT, "playlists": {"video.m3u8": media("https://elsewhere.example.org/v.mp4"), "audio.m3u8": media("https://cdn.example.com/a.mp4")}});
        assert!(prober.probe_playlists(&playlists, &hosts).unwrap_err().contains("https://elsewhere.example.org/..., is not on the addon's hosts"));
    }
}
