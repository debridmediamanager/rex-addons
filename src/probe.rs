//! ffprobe: the host's own check of each new item, and the `ffprobe()` an addon calls. An
//! addon's probe never lets ffprobe reach the network itself: ffprobe reads a one-off relay on
//! this host, which fetches through the addon's sandboxed client (its hosts, no address on this
//! device or a private network, every redirect checked), and HLS playlists are rewritten so their
//! segments come through the relay too.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Mutex;
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
}

pub struct Prober {
    pub bin: Option<String>,
    base: String,
    relays: Mutex<HashMap<String, Relay>>,
}

impl Prober {
    pub fn new(bin: Option<String>, listen: SocketAddr) -> Prober {
        Prober { bin, base: format!("http://{}", net::reachable(listen)), relays: Mutex::default() }
    }

    /// An addon's `ffprobe(url, {headers})`.
    pub fn probe(&self, url: &str, headers: Vec<(String, String)>, hosts: &Hosts) -> Result<Json, String> {
        let bin = self.bin.as_deref().ok_or("ffprobe() is not available: rex-addons runs without --ffprobe")?;
        let parsed = url::Url::parse(url).map_err(|e| format!("not a link: {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!("{}: only http and https", parsed.scheme()));
        }
        let host = parsed.host_str().unwrap_or("");
        if !hosts.allows(host) {
            return Err(format!("{host} is not in the addon's hosts"));
        }
        let token = token();
        self.relays.lock().unwrap().insert(token.clone(), Relay { url: url.to_string(), headers, hosts: hosts.clone() });
        // The relay URL ends in the link's own file name, so ffprobe resolves a playlist's
        // relative segments under the relay.
        let name = parsed.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty()).unwrap_or("media");
        let probed = run(bin, &format!("{}/relay/{token}/{name}", self.base), &["-protocol_whitelist", "http,tcp,crypto"]);
        self.relays.lock().unwrap().remove(&token);
        probed
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

/// An HLS playlist with every URI in it, relative or not, pointed back through the relay as
/// `abs/<the absolute URI, percent-encoded>/<its file name>`: FFmpeg's HLS reader goes by the
/// name's extension.
pub fn rewrite_playlist(text: &str, playlist: &url::Url, relay_base: &str) -> String {
    let relayed = |uri: &str| match playlist.join(uri.trim()) {
        Ok(absolute) => {
            let name = absolute.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty()).unwrap_or("media");
            let encoded = percent_encoding::utf8_percent_encode(absolute.as_str(), percent_encoding::NON_ALPHANUMERIC);
            format!("{relay_base}/abs/{encoded}/{name}")
        }
        Err(_) => uri.to_string(),
    };
    text.lines()
        .map(|line| {
            if line.trim().is_empty() {
                line.to_string()
            } else if !line.starts_with('#') {
                relayed(line)
            } else if let Some(at) = line.find("URI=\"") {
                let start = at + 5;
                match line[start..].find('"') {
                    Some(len) => format!("{}{}{}", &line[..start], relayed(&line[start..start + len]), &line[start + len..]),
                    None => line.to_string(),
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn without_ffprobe_or_off_the_hosts() {
        let prober = Prober::new(None, "127.0.0.1:32410".parse().unwrap());
        let hosts = Hosts(vec!["cdn.example.com".into()]);
        assert_eq!(prober.probe("https://cdn.example.com/a.mp4", vec![], &hosts).unwrap_err(), "ffprobe() is not available: rex-addons runs without --ffprobe");
        let prober = Prober::new(Some("ffprobe".into()), "127.0.0.1:32410".parse().unwrap());
        assert_eq!(prober.probe("https://elsewhere.example.org/a.mp4", vec![], &hosts).unwrap_err(), "elsewhere.example.org is not in the addon's hosts");
        assert_eq!(prober.probe("file:///etc/passwd", vec![], &hosts).unwrap_err(), "file: only http and https");
    }
}
