//! Spotify for an addon whose manifest says `"sign_in": {"kind": "spotify"}`: signing in by
//! Spotify Connect pairing, and the session it lists the person's library with. librespot does
//! the protocol (the key exchange, the credentials, the session); its tokio runtime lives in this
//! module only. Nothing here plays audio, asks for audio keys or registers this device as a
//! Connect player: once paired, it is no device at all.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use librespot_core::authentication::Credentials;
use librespot_core::diffie_hellman::DhLocalKeys;
use librespot_core::error::ErrorKind;
use librespot_core::http_client::HttpClientError;
use librespot_core::{Session, SessionConfig};
use serde_json::{json, Value as Json};

/// A pairing in progress: dropping it ends it, and with it the endpoint on the local network
/// and the mDNS record.
pub struct Pairing(Option<Box<dyn FnOnce() + Send + Sync>>);

impl Pairing {
    pub fn new(stop: impl FnOnce() + Send + Sync + 'static) -> Pairing {
        Pairing(Some(Box::new(stop)))
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            stop();
        }
    }
}

/// An addon signed in: the account's canonical username (what Spotify's paths take), the name
/// to show, and the credentials to keep from now on.
#[derive(Clone)]
pub struct Signed {
    pub username: String,
    pub account: String,
    pub credentials: Json,
}

/// What the host needs of Spotify; tests stand in a stub for it. Errors that start with
/// `SIGN_IN_REQUIRED` mean Spotify no longer takes the credentials.
pub trait Spotify: Send + Sync {
    /// Makes this device discoverable as `name` on the local network until `lifetime` passes
    /// or the pairing is dropped. `done` gets the credentials the Spotify app sent
    /// (librespot's JSON), or why pairing ended without them; it is not called once dropped.
    fn pair(&self, name: &str, lifetime: Duration, done: Box<dyn FnOnce(Result<Json, String>) + Send>) -> Result<Pairing, String>;
    /// Signs `addon` in with `credentials`. Its session is kept and reused while it lasts.
    fn connect(&self, addon: &str, credentials: &Json) -> Result<Signed, String>;
    /// A request to Spotify's own API (spclient) on the addon's session, connected first.
    fn request(&self, addon: &str, method: &str, path: &str, body: Option<&Json>) -> Result<Json, String>;
    /// Ends the addon's session.
    fn forget(&self, addon: &str);
}

/// The paths `spotify.get()` and `spotify.post()` take: absolute, plain, on spclient's host.
pub fn valid_path(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//") && path.len() <= 4096 && path.bytes().all(|b| b.is_ascii_graphic())
}

/// The longest a sign-in or a request waits on Spotify.
const WAIT: Duration = Duration::from_secs(60);

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    // One worker: it keeps each session's connection answered between calls.
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).thread_name("spotify").enable_all().build().expect("cannot start Spotify's runtime")
    })
}

const SERVICE: &str = "_spotify-connect._tcp";
const TXT: [&str; 2] = ["VERSION=1.0", "CPath=/"];

/// The pairing endpoint's side of the key exchange.
struct Endpoint {
    name: String,
    device: String,
    keys: DhLocalKeys,
}

impl Endpoint {
    /// Answers one request from the Spotify app: the credentials, once `addUser` brought them.
    fn answer(&self, mut request: tiny_http::Request) -> Option<Json> {
        let mut body = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::Read::take(request.as_reader(), 64 << 10), &mut body);
        let query = request.url().split_once('?').map_or("", |(_, query)| query).to_string();
        let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().chain(url::form_urlencoded::parse(&body).into_owned()).collect();
        let (status, answer, credentials) = match (request.method(), params.get("action").map(String::as_str)) {
            (tiny_http::Method::Get, Some("getInfo")) => (200, self.info(), None),
            (tiny_http::Method::Post, Some("addUser")) => match self.add_user(&params) {
                Ok(credentials) => (200, json!({"status": 101, "spotifyError": 0, "statusString": "OK"}), Some(credentials)),
                Err(error) => (400, json!({"status": 102, "spotifyError": 1, "statusString": error}), None),
            },
            _ => (404, json!({"status": 102, "spotifyError": 0, "statusString": "ERROR-NOT-FOUND"}), None),
        };
        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
        let _ = request.respond(tiny_http::Response::from_string(answer.to_string()).with_status_code(status).with_header(header));
        credentials
    }

    /// What librespot answers, but no speaker to play to: `activeUser` stays empty.
    fn info(&self) -> Json {
        json!({
            "status": 101, "statusString": "OK", "spotifyError": 0, "version": "2.9.0",
            "deviceID": self.device, "deviceType": "Speaker", "remoteName": self.name,
            "publicKey": base64::engine::general_purpose::STANDARD.encode(self.keys.public_key()),
            "brandDisplayName": "rex", "modelDisplayName": "rex-addons", "libraryVersion": env!("CARGO_PKG_VERSION"),
            "resolverVersion": "1", "groupStatus": "NONE", "tokenType": "default",
            "clientID": SessionConfig::default().client_id, "productID": 0, "scope": "streaming", "availability": "",
            "supported_drm_media_formats": [], "supported_capabilities": 1, "accountReq": "PREMIUM", "activeUser": "", "aliases": [],
        })
    }

    /// `blob` is 16 bytes of IV, the credentials in AES-128-CTR and a 20-byte HMAC-SHA1, keyed
    /// from the key exchange with `clientKey`. The credentials inside are librespot's to read.
    fn add_user(&self, params: &HashMap<String, String>) -> Result<Json, String> {
        let field = |key: &str| params.get(key).ok_or_else(|| format!("ERROR-MISSING-{key}"));
        let base64 = |key: &str| base64::engine::general_purpose::STANDARD.decode(field(key)?).map_err(|_| format!("ERROR-BAD-{key}"));
        let (username, blob, client_key) = (field("userName")?, base64("blob")?, base64("clientKey")?);
        if blob.len() < 16 + 20 {
            return Err("ERROR-BAD-blob".into());
        }
        let sha1 = |bytes: &[u8]| ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, bytes);
        let hmac = |key: &[u8], bytes: &[u8]| ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key), bytes);
        let base = sha1(&self.keys.shared_secret(&client_key));
        let base = &base.as_ref()[..16];
        let (iv, rest) = blob.split_at(16);
        let (encrypted, mac) = rest.split_at(rest.len() - 20);
        let checksum = hmac(base, b"checksum");
        ring::hmac::verify(&ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, checksum.as_ref()), encrypted, mac)
            .map_err(|_| "ERROR-MAC".to_string())?;
        let encryption = hmac(base, b"encryption");
        let mut decrypted = encrypted.to_vec();
        use ctr::cipher::{KeyIvInit, StreamCipher};
        ctr::Ctr128BE::<aes::Aes128>::new(encryption.as_ref()[..16].into(), iv.into()).apply_keystream(&mut decrypted);
        // librespot reads the inside as base64 of whole AES blocks, and panics on less than one.
        let inside = base64::engine::general_purpose::STANDARD.decode(&decrypted).map_err(|_| "ERROR-BAD-blob".to_string())?;
        if inside.len() < 16 || inside.len() % 16 != 0 {
            return Err("ERROR-BAD-blob".into());
        }
        let credentials = Credentials::with_blob(username.as_str(), decrypted, &self.device).map_err(|_| "ERROR-BAD-blob".to_string())?;
        serde_json::to_value(credentials).map_err(|e| e.to_string())
    }
}

/// librespot's own device id for a name: SHA-1 of it, in hex.
fn device_id(name: &str) -> String {
    crate::git::hex(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, name.as_bytes()).as_ref())
}

/// What a librespot error says, with the HTTP status it carried, if any.
fn describe(error: &librespot_core::Error) -> String {
    match error.error.downcast_ref::<HttpClientError>() {
        Some(HttpClientError::StatusCode(code)) => format!("Spotify answered {}", code.as_u16()),
        None => error.to_string(),
    }
}

/// librespot's sessions, one per addon.
#[derive(Default)]
pub struct Librespot {
    sessions: Mutex<HashMap<String, (Session, Signed)>>,
    /// The port the pairing endpoint listens on; 0 picks a free one.
    pub port: u16,
}

impl Librespot {
    fn session(&self, addon: &str) -> Option<(Session, Signed)> {
        self.sessions.lock().unwrap().get(addon).filter(|(session, _)| !session.is_invalid()).cloned()
    }
}

impl Spotify for Librespot {
    /// Spotify Connect's zeroconf pairing, as a speaker answers it: an mDNS record for
    /// `_spotify-connect._tcp`, and an HTTP endpoint on the local network the Spotify app asks
    /// `getInfo` (this device's public key) and then sends `addUser` (the account's credentials,
    /// encrypted for that key). librespot-discovery does the same, but its mDNS responder names
    /// this device's host after the system's (`localhost` on Android) and drops the goodbye that
    /// takes it off phones' lists.
    fn pair(&self, name: &str, lifetime: Duration, done: Box<dyn FnOnce(Result<Json, String>) + Send>) -> Result<Pairing, String> {
        let server = tiny_http::Server::http(("::", self.port))
            .or_else(|_| tiny_http::Server::http(("0.0.0.0", self.port)))
            .map_err(|e| format!("cannot listen on the local network: {e}"))?;
        let port = server.server_addr().to_ip().map_or(self.port, |addr| addr.port());
        let endpoint = Endpoint { name: name.to_string(), device: device_id(name), keys: DhLocalKeys::random(&mut rand::rng()) };
        let hostname = format!("rex-{}", &endpoint.device[..12]);
        // Its sockets register with the runtime they are made in.
        let _inside = runtime().enter();
        let responder = libmdns::Responder::spawn_with_ip_list_and_hostname(runtime().handle(), Vec::new(), hostname)
            .map_err(|e| format!("cannot be announced on the local network: {e}"))?;
        let service = responder.register(SERVICE, name, port, &TXT);
        let (server, stopped) = (Arc::new(server), Arc::new(AtomicBool::new(false)));
        let stop = (Arc::downgrade(&server), stopped.clone());
        std::thread::spawn(move || {
            let deadline = Instant::now() + lifetime;
            let outcome = loop {
                if stopped.load(Ordering::SeqCst) {
                    break None;
                }
                match server.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Some(request)) => {
                        // Whoever is on the local network may send anything: nothing it sends ends the pairing.
                        let answered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| endpoint.answer(request)));
                        if let Ok(Some(credentials)) = answered {
                            break Some(Ok(credentials));
                        }
                    }
                    Ok(None) if Instant::now() >= deadline => break Some(Err(format!("nobody picked {} in the Spotify app in time", endpoint.name))),
                    Ok(None) => {}
                    Err(e) => break Some(Err(format!("pairing stopped: {e}"))),
                }
            };
            drop(server);
            drop(service);
            // The goodbye that takes it off phones' lists, out before the responder stops.
            std::thread::sleep(Duration::from_millis(500));
            drop(responder);
            if let Some(outcome) = outcome.filter(|_| !stopped.load(Ordering::SeqCst)) {
                done(outcome);
            }
        });
        Ok(Pairing::new(move || {
            stop.1.store(true, Ordering::SeqCst);
            if let Some(server) = stop.0.upgrade() {
                server.unblock();
            }
        }))
    }

    fn connect(&self, addon: &str, credentials: &Json) -> Result<Signed, String> {
        if let Some((_, signed)) = self.session(addon) {
            return Ok(signed);
        }
        let credentials: Credentials = serde_json::from_value(credentials.clone()).map_err(|e| format!("the saved Spotify sign-in is unreadable: {e}"))?;
        // The timer is made inside the runtime: outside it there is none.
        let connected = runtime().block_on(async { tokio::time::timeout(WAIT, async {
            let session = Session::new(SessionConfig::default(), None);
            session.connect(credentials.clone(), false).await.map_err(|e| match e.kind {
                // Spotify refused them: Bad credentials, a password changed, access removed.
                ErrorKind::PermissionDenied | ErrorKind::Unauthenticated => format!("SIGN_IN_REQUIRED: Spotify no longer accepts this sign-in ({e})"),
                _ => format!("Spotify is not reachable: {e}"),
            })?;
            let username = session.username();
            let profile = session.spclient().get_user_profile(&username, Some(0), Some(0)).await;
            let name = profile.ok().and_then(|bytes| serde_json::from_slice::<Json>(&bytes).ok()).and_then(|p| p["name"].as_str().map(str::to_string));
            // What reconnects from now on: the session's own reusable credentials.
            let kept = Credentials { username: Some(username.clone()), auth_data: session.auth_data(), ..credentials };
            let kept = serde_json::to_value(&kept).map_err(|e| e.to_string())?;
            let account = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| username.clone());
            Ok::<_, String>((session.clone(), Signed { username, account, credentials: kept }))
        }).await });
        let (session, signed) = connected.map_err(|_| format!("Spotify did not answer in {} s", WAIT.as_secs()))??;
        if let Some((old, _)) = self.sessions.lock().unwrap().insert(addon.to_string(), (session, signed.clone())) {
            old.shutdown();
        }
        Ok(signed)
    }

    fn request(&self, addon: &str, method: &str, path: &str, body: Option<&Json>) -> Result<Json, String> {
        let (session, _) = self.session(addon).ok_or("not connected to Spotify")?;
        let method = match method {
            "GET" => http::Method::GET,
            "POST" => http::Method::POST,
            _ => return Err(format!("{method} is not GET or POST")),
        };
        let body = body.map(Json::to_string);
        let answer = runtime().block_on(async { tokio::time::timeout(WAIT, async {
            let mut headers = http::HeaderMap::new();
            if body.is_some() {
                headers.insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
            }
            session.spclient().request_as_json(&method, path, Some(headers), body.as_deref()).await
        }).await });
        let answer = answer.map_err(|_| format!("Spotify did not answer {} in {} s", path.split('?').next().unwrap_or(path), WAIT.as_secs()))?;
        let bytes = answer.map_err(|e| match e.kind {
            ErrorKind::Unauthenticated if e.error.downcast_ref::<HttpClientError>().is_none() => {
                format!("SIGN_IN_REQUIRED: Spotify no longer accepts this sign-in ({e})")
            }
            _ => format!("{} to {}", describe(&e), path.split('?').next().unwrap_or(path)),
        })?;
        if bytes.is_empty() {
            return Ok(Json::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| format!("Spotify's answer to {} is not JSON", path.split('?').next().unwrap_or(path)))
    }

    fn forget(&self, addon: &str) {
        if let Some((session, _)) = self.sessions.lock().unwrap().remove(addon) {
            session.shutdown();
        }
    }
}

/// For end-to-end tests only: Spotify's API answered by a server of the test's own
/// (`REX_ADDONS_SPOTIFY_STUB`), a request's path appended to its address. A saved sign-in is taken
/// as it is, its `username` the account's; pairing is refused.
pub struct Stub {
    pub url: String,
}

impl Spotify for Stub {
    fn pair(&self, _: &str, _: Duration, _: Box<dyn FnOnce(Result<Json, String>) + Send>) -> Result<Pairing, String> {
        Err("pairing is off: REX_ADDONS_SPOTIFY_STUB is set".into())
    }

    fn connect(&self, _: &str, credentials: &Json) -> Result<Signed, String> {
        let username = credentials["username"].as_str().ok_or("SIGN_IN_REQUIRED: the saved sign-in has no username")?.to_string();
        Ok(Signed { account: username.clone(), username, credentials: credentials.clone() })
    }

    fn request(&self, _: &str, method: &str, path: &str, body: Option<&Json>) -> Result<Json, String> {
        let url = format!("{}{path}", self.url.trim_end_matches('/'));
        let answer = match body {
            Some(body) => ureq::request(method, &url).send_string(&body.to_string()),
            None => ureq::request(method, &url).call(),
        };
        let route = path.split('?').next().unwrap_or(path);
        match answer {
            Ok(answer) => serde_json::from_str(&answer.into_string().map_err(|e| e.to_string())?).map_err(|_| format!("Spotify's answer to {route} is not JSON")),
            Err(ureq::Error::Status(status, _)) => Err(format!("Spotify answered {status} to {route}")),
            Err(error) => Err(format!("Spotify is not reachable: {error}")),
        }
    }

    fn forget(&self, _: &str) {}
}

/// The JSON a stub stands in for librespot's credentials with, in tests.
#[cfg(test)]
pub fn fake_credentials(username: &str, secret: &str) -> Json {
    serde_json::json!({"username": username, "auth_type": 1, "auth_data": secret})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_on_spotifys_host() {
        for good in ["/playlist/v2/user/me/rootlist?decorate=length", "/collection/v2/paging", "/metadata/4/track/0123abcd?market=from_token"] {
            assert!(valid_path(good), "{good}");
        }
        // `https://spclient...:443` + `@evil.example/` would send the token to evil.example.
        for bad in ["@evil.example/x", "//evil.example/x", "evil", "/a b", "/a\r\nHost: x", ""] {
            assert!(!valid_path(bad), "{bad:?}");
        }
    }

    /// The end-to-end tests' stand-in: requests go to its server as they are, its refusals are
    /// Spotify's, and it never pairs.
    #[test]
    fn the_stub_answers_from_the_tests_server() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let status = if request.url().starts_with("/missing") { 404 } else { 200 };
                let answer = json!({"method": request.method().to_string(), "url": request.url(), "body": body});
                let _ = request.respond(tiny_http::Response::from_string(answer.to_string()).with_status_code(status));
            }
        });
        let stub = Stub { url };
        let signed = stub.connect("a", &fake_credentials("someone", "x")).unwrap();
        assert_eq!((signed.username.as_str(), signed.account.as_str()), ("someone", "someone"));
        assert!(stub.connect("a", &json!({})).is_err_and(|e| e.starts_with("SIGN_IN_REQUIRED")));
        assert_eq!(stub.request("a", "GET", "/metadata/4/track/ab?market=from_token", None).unwrap(),
            json!({"method": "GET", "url": "/metadata/4/track/ab?market=from_token", "body": ""}));
        assert_eq!(stub.request("a", "POST", "/collection/v2/paging", Some(&json!({"set": "collection"}))).unwrap()["body"], "{\"set\":\"collection\"}");
        assert_eq!(stub.request("a", "GET", "/missing?x=1", None).unwrap_err(), "Spotify answered 404 to /missing");
        assert!(stub.pair("rex", Duration::from_secs(1), Box::new(|_| {})).is_err());
    }

    #[test]
    fn device_id_is_librespots() {
        // librespot's main: hex(sha1(name)).
        assert_eq!(device_id("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    /// The pairing endpoint answers getInfo on the local network while pairing, as the Spotify
    /// app asks it, and is gone once the pairing is dropped.
    #[test]
    fn the_pairing_endpoint_answers_until_dropped() {
        let port = std::net::TcpListener::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
        let spotify = Librespot { port, ..Librespot::default() };
        // `[::]` takes IPv4 too, as on Android.
        let (sender, outcome) = std::sync::mpsc::channel();
        let pairing = spotify.pair("rex-test", Duration::from_secs(60), Box::new(move |o| { let _ = sender.send(o); })).unwrap();
        let info: Json = serde_json::from_str(&ureq::get(&format!("http://127.0.0.1:{port}/?action=getInfo")).call().unwrap().into_string().unwrap()).unwrap();
        assert_eq!((info["remoteName"].as_str(), info["status"].as_u64()), (Some("rex-test"), Some(101)));
        assert_eq!(info["deviceID"], device_id("rex-test"));
        assert!(info["publicKey"].as_str().is_some_and(|key| !key.is_empty()));
        // An addUser that is not one is refused, and the endpoint stays up.
        let refused = ureq::post(&format!("http://127.0.0.1:{port}/")).send_form(&[("action", "addUser"), ("userName", "x"), ("blob", "AAAA"), ("clientKey", "AAAA")]);
        assert!(matches!(refused, Err(ureq::Error::Status(400, _))), "{refused:?}");
        // One whose MAC is wrong, as from another key exchange: 40 bytes of zeros.
        let zeros = base64::engine::general_purpose::STANDARD.encode([0u8; 40]);
        let client = base64::engine::general_purpose::STANDARD.encode(DhLocalKeys::random(&mut rand::rng()).public_key());
        let refused = ureq::post(&format!("http://127.0.0.1:{port}/")).send_form(&[("action", "addUser"), ("userName", "x"), ("blob", &zeros), ("clientKey", &client)]);
        let Err(ureq::Error::Status(400, answer)) = refused else { panic!("{refused:?}") };
        assert_eq!(serde_json::from_str::<Json>(&answer.into_string().unwrap()).unwrap()["statusString"], "ERROR-MAC");
        assert!(ureq::get(&format!("http://127.0.0.1:{port}/?action=getInfo")).call().is_ok());
        drop(pairing);
        let gone = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            ureq::get(&format!("http://127.0.0.1:{port}/?action=getInfo")).timeout(Duration::from_secs(1)).call().is_err()
        });
        assert!(gone, "the endpoint still answers after the pairing was dropped");
        // Dropped, `done` is never called.
        assert!(outcome.recv_timeout(Duration::from_millis(300)).is_err());
    }

    /// What the Spotify app sends to `addUser`, made as librespot reads it: `credentials`
    /// encrypted for the device `name`'s public key `server` (base64), from a key exchange of its own.
    fn add_user(name: &str, server: &str, username: &str, auth_data: &[u8]) -> Vec<(&'static str, String)> {
        use aes::cipher::{BlockEncrypt, KeyInit};
        let b64 = base64::engine::general_purpose::STANDARD;
        // Inside: tag, username, tag, auth type 1 (stored credentials), tag, the data.
        let mut plain = vec![0x49, username.len() as u8];
        plain.extend(username.as_bytes());
        plain.extend([0x50, 1, 0x51, auth_data.len() as u8]);
        plain.extend(auth_data);
        plain.resize(plain.len().div_ceil(16) * 16, 0);
        // Keyed by PBKDF2 of the device id and the username; each block chained to the one before.
        let secret = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, device_id(name).as_bytes());
        let mut key = [0u8; 24];
        ring::pbkdf2::derive(ring::pbkdf2::PBKDF2_HMAC_SHA1, 256.try_into().unwrap(), username.as_bytes(), secret.as_ref(), &mut key[..20]);
        let hash = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &key[..20]);
        key[..20].copy_from_slice(hash.as_ref());
        key[20..].copy_from_slice(&20u32.to_be_bytes());
        for at in 16..plain.len() {
            plain[at] ^= plain[at - 16];
        }
        let cipher = aes::Aes192::new((&key).into());
        for block in plain.as_chunks_mut::<16>().0 {
            cipher.encrypt_block(block.into());
        }
        seal(server, username, b64.encode(&plain).into_bytes())
    }

    /// The outside of an addUser: `inside` in AES-128-CTR and HMAC-SHA1, keyed from a key
    /// exchange with the public key `server`.
    fn seal(server: &str, username: &str, inside: Vec<u8>) -> Vec<(&'static str, String)> {
        use ctr::cipher::{KeyIvInit, StreamCipher};
        let b64 = base64::engine::general_purpose::STANDARD;
        let client = DhLocalKeys::random(&mut rand::rng());
        let shared = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &client.shared_secret(&b64.decode(server).unwrap()));
        let hmac = |key: &[u8], bytes: &[u8]| ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key), bytes);
        let iv = [7u8; 16];
        let mut encrypted = inside;
        ctr::Ctr128BE::<aes::Aes128>::new(hmac(&shared.as_ref()[..16], b"encryption").as_ref()[..16].into(), (&iv).into()).apply_keystream(&mut encrypted);
        let mac = hmac(hmac(&shared.as_ref()[..16], b"checksum").as_ref(), &encrypted);
        let blob = [&iv[..], &encrypted, mac.as_ref()].concat();
        vec![("action", "addUser".into()), ("userName", username.into()), ("blob", b64.encode(blob)), ("clientKey", b64.encode(client.public_key()))]
    }

    /// The endpoint's status and JSON.
    fn post(port: u16, form: &[(&'static str, String)]) -> (u16, Json) {
        let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        match ureq::post(&format!("http://127.0.0.1:{port}/")).send_form(&form) {
            Ok(answer) | Err(ureq::Error::Status(_, answer)) => (answer.status(), serde_json::from_str(&answer.into_string().unwrap()).unwrap()),
            Err(error) => panic!("{error}"),
        }
    }

    fn info(port: u16) -> Json {
        serde_json::from_str(&ureq::get(&format!("http://127.0.0.1:{port}/?action=getInfo")).call().unwrap().into_string().unwrap()).unwrap()
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port()
    }

    /// The credentials an addUser brings are read as librespot-discovery, which paired with the
    /// Spotify app on 2026-11-11, reads them, and end the pairing.
    #[test]
    fn add_user_brings_the_credentials_librespot_reads() {
        let secret = b"stored credentials, FABRICATED".to_vec();
        // librespot's own endpoint, as the reference.
        let port = free_port();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let theirs = runtime.block_on(async {
            use futures_util::StreamExt;
            let mut discovery = librespot_discovery::Discovery::builder(device_id("rex-reference"), SessionConfig::default().client_id)
                .name("rex-reference").port(port).launch().unwrap();
            let form = tokio::task::spawn_blocking(move || {
                let form = add_user("rex-reference", info(port)["publicKey"].as_str().unwrap(), "someone", b"stored credentials, FABRICATED");
                assert_eq!(post(port, &form).0, 200);
            });
            let credentials = discovery.next().await.unwrap();
            form.await.unwrap();
            discovery.shutdown().await;
            serde_json::to_value(credentials).unwrap()
        });
        assert_eq!(theirs["auth_data"], base64::engine::general_purpose::STANDARD.encode(&secret));
        // This endpoint reads the same from the same kind of request, and stops.
        let port = free_port();
        let spotify = Librespot { port, ..Librespot::default() };
        let (sender, outcome) = std::sync::mpsc::channel();
        let _pairing = spotify.pair("rex-test-add", Duration::from_secs(60), Box::new(move |o| { let _ = sender.send(o); })).unwrap();
        let form = add_user("rex-test-add", info(port)["publicKey"].as_str().unwrap(), "someone", &secret);
        assert_eq!(post(port, &form), (200, json!({"status": 101, "spotifyError": 0, "statusString": "OK"})));
        assert_eq!(outcome.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(), theirs);
        let closed = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            ureq::get(&format!("http://127.0.0.1:{port}/?action=getInfo")).timeout(Duration::from_secs(1)).call().is_err()
        });
        assert!(closed, "the endpoint still answers after pairing");
    }

    /// Credentials that decrypt to less than librespot can read are refused, not a panic.
    #[test]
    fn a_short_inside_is_refused() {
        let port = free_port();
        let spotify = Librespot { port, ..Librespot::default() };
        let _pairing = spotify.pair("rex-test-short", Duration::from_secs(60), Box::new(|_| {})).unwrap();
        // A MAC that holds, around 3 bytes: librespot's reader would run past their start.
        let form = seal(info(port)["publicKey"].as_str().unwrap(), "someone", b"AAAA".to_vec());
        assert_eq!(post(port, &form), (400, json!({"status": 102, "spotifyError": 1, "statusString": "ERROR-BAD-blob"})));
        assert_eq!(info(port)["status"], 101);
    }

    #[test]
    fn a_pairing_nobody_finishes_expires() {
        let spotify = Librespot::default();
        let (sender, outcome) = std::sync::mpsc::channel();
        let _pairing = spotify.pair("rex-expiry", Duration::from_millis(300), Box::new(move |o| { let _ = sender.send(o); })).unwrap();
        let got = outcome.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(got.unwrap_err(), "nobody picked rex-expiry in the Spotify app in time");
    }
}
