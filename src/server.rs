//! The HTTP side: the management API (this device only) and the byte paths rex reads.
use std::io::{Cursor, Read};
use std::sync::Arc;

use serde_json::{json, Value as Json};
use tiny_http::{Header, Method, Request, Response, StatusCode};

use crate::app::{self, App};
use crate::net;

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}

fn json_response(status: u16, body: Json) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(serde_json::to_vec(&body).unwrap())
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json"))
}

fn error(status: u16, message: &str) -> Response<Cursor<Vec<u8>>> {
    json_response(status, json!({"error": message}))
}

/// `token`, when given, is what every `/api/` request must carry as `Authorization: Bearer`.
pub fn serve(app: Arc<App>, server: tiny_http::Server, token: Option<String>) {
    let token: Option<Arc<str>> = token.map(Arc::from);
    for request in server.incoming_requests() {
        let (app, token) = (app.clone(), token.clone());
        let spawned = std::thread::Builder::new().stack_size(4 << 20).spawn(move || handle(&app, request, token.as_deref()));
        if let Err(e) = spawned {
            app::warn("host", &format!("cannot start a request thread: {e}"));
        }
    }
}

/// Equal in a time that does not depend on where they first differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A `Host` header naming anything but this host is a web page that got here through DNS
/// rebinding; no header (an HTTP/1.0 client) is this device's own program.
fn host_is_ours(request: &Request, listen: std::net::SocketAddr) -> bool {
    let Some(host) = request.headers().iter().find(|h| h.field.equiv("Host")) else { return true };
    let port = listen.port();
    let host = host.value.as_str().to_ascii_lowercase();
    [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}"), listen.to_string()].contains(&host)
}

fn handle(app: &Arc<App>, mut request: Request, token: Option<&str>) {
    let peer = request.remote_addr().map(|addr| addr.ip());
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or_default().to_string();
    if let Some(rest) = path.strip_prefix("/bytes/") {
        // rex reads these; like rex's own feed, only from this device or a private network.
        if !peer.is_some_and(net::is_private) {
            let _ = request.respond(error(403, "only rex on a private network reads bytes here"));
            return;
        }
        return bytes(app, request, rest);
    }
    if !path.starts_with("/api/") && !path.starts_with("/relay/") {
        let _ = request.respond(error(404, "not found"));
        return;
    }
    // The socket's own peer, never a header: the API answers only this device.
    if !peer.is_some_and(net::is_loopback) {
        let _ = request.respond(error(403, "the management API answers only this device"));
        return;
    }
    if let Some(rest) = url.strip_prefix("/relay/") {
        return relay(app, request, rest);
    }
    if !host_is_ours(&request, app.listen) {
        let _ = request.respond(error(403, "the management API answers only requests addressed to this host"));
        return;
    }
    if let Some(token) = token {
        let given = request.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.as_str().to_string());
        if !given.and_then(|v| v.strip_prefix("Bearer ").map(str::to_string)).is_some_and(|v| same(v.as_bytes(), token.as_bytes())) {
            let _ = request.respond(error(401, "give the API token as Authorization: Bearer <token>"));
            return;
        }
    }
    let mut body = String::new();
    if request.as_reader().take(1 << 20).read_to_string(&mut body).is_err() {
        let _ = request.respond(error(400, "the body is not UTF-8"));
        return;
    }
    let method = request.method().clone();
    let query = url.split_once('?').map_or("", |(_, query)| query);
    let response = match api(app, &method, &path, query, &body) {
        Ok((status, answer)) => json_response(status, answer),
        Err((status, message)) => error(status, &message),
    };
    let _ = request.respond(response);
}

type Answer = Result<(u16, Json), (u16, String)>;

fn parse(body: &str) -> Result<Json, (u16, String)> {
    serde_json::from_str(body).map_err(|e| (400, format!("the body is not JSON: {e}")))
}

fn detail(app: &App, id: &str) -> Answer {
    let mut answer = app.status(id).ok_or((404, "no such addon".to_string()))?;
    let addons = app.addons.lock().unwrap();
    let addon = addons.get(id).ok_or((404, "no such addon".to_string()))?;
    answer["manifest"] = addon.manifest.raw.clone();
    answer["config"] = Json::Object(addon.manifest.masked(&addon.config));
    answer["deleted"] = json!(addon.deleted.len());
    Ok((200, answer))
}

fn api(app: &Arc<App>, method: &Method, path: &str, query: &str, body: &str) -> Answer {
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').skip(2).collect();
    let text = |body: &Json, key: &str| body[key].as_str().map(str::to_string).ok_or((400, format!("give {key}")));
    match (method, parts.as_slice()) {
        (Method::Get, ["status"]) => Ok((200, json!({
            "version": env!("CARGO_PKG_VERSION"),
            "rex": {"url": app.rex, "reachable": app.rex_reachable()},
            "addons": app.statuses(),
        }))),
        (Method::Get, ["repos"]) => Ok((200, app.list_repos())),
        (Method::Post, ["repos"]) => app.add_repo(&text(&parse(body)?, "url")?),
        (Method::Delete, ["repos", id]) => {
            app.remove_repo(id).map_err(|e| (404, e))?;
            Ok((200, json!({"removed": true})))
        }
        (Method::Post, ["repos", id, "refresh"]) => app.refresh_repo(id).map(|repo| (200, repo)),
        (Method::Get, ["repos", id, "addons"]) => app.repo_addons_json(id).map(|list| (200, list)).ok_or((404, "no such repository".into())),
        (Method::Post, ["addons"]) => {
            let body = parse(body)?;
            if body.get("url").is_some() && body.get("repo").is_none() {
                return Err((400, "addons install from a repository: add it with POST /api/repos, then send {\"repo\", \"addon\"}".into()));
            }
            let (status, id) = app.install_from_repo(&text(&body, "repo")?, &text(&body, "addon")?)?;
            app.start_sync(&id, false, "install");
            detail(app, &id).map(|(_, answer)| (status, answer))
        }
        (Method::Get, ["addons", id]) => detail(app, id),
        (Method::Get, ["addons", id, "log"]) => {
            if !app.addons.lock().unwrap().contains_key(*id) {
                return Err((404, "no such addon".into()));
            }
            let lines = match query.split('&').find_map(|pair| pair.strip_prefix("lines=")) {
                None => app::LOG_LINES,
                Some(n) => n.parse().map_err(|_| (400, "lines must be a whole number".to_string()))?,
            };
            Ok((200, json!({"lines": app::recent(id, lines)})))
        }
        (Method::Delete, ["addons", id]) => {
            let answer = app.remove(id).map_err(|e| (if e == "no such addon" { 404 } else { 500 }, e))?;
            Ok((200, answer))
        }
        (Method::Put, ["addons", id, "config"]) => {
            let config = app.set_config(id, &parse(body)?).map_err(|e| (if e == "no such addon" { 404 } else { 400 }, e))?;
            Ok((200, json!({"config": config})))
        }
        (Method::Post, ["addons", id, "sync"]) => {
            let force = !body.trim().is_empty() && parse(body)?["force"].as_bool().unwrap_or(false);
            let enabled = app.addons.lock().unwrap().get(*id).map(|a| a.enabled);
            match enabled {
                None => Err((404, "no such addon".into())),
                Some(false) => Err((409, "the addon is disabled".into())),
                Some(true) => {
                    app.start_sync(id, force, "manual");
                    Ok((202, json!({"syncing": true})))
                }
            }
        }
        (Method::Post, ["addons", id, "restore"]) => {
            app.restore(id).map_err(|e| (404, e))?;
            detail(app, id)
        }
        (Method::Post, ["addons", id, "sign-in"]) => {
            app.sign_in(id)?;
            detail(app, id)
        }
        (Method::Delete, ["addons", id, "sign-in"]) => {
            app.sign_out(id)?;
            detail(app, id)
        }
        (Method::Post, ["addons", id, "enabled"]) => {
            let enabled = parse(body)?["enabled"].as_bool().ok_or((400, "give enabled: true or false".to_string()))?;
            app.set_enabled(id, enabled).map_err(|e| (404, e))?;
            detail(app, id)
        }
        _ => Err((404, "not found".into())),
    }
}

/// `/bytes/{addon}/{path}`: the item's link from `open()`, as a redirect, or relayed when
/// the link needs headers of its own; or the playlists `open()` wrote, its master at the item's
/// path and the playlists it names under it (`{path}/{name}`). rex's DELETE is a person deleting
/// the item in a Plex app.
fn bytes(app: &Arc<App>, request: Request, rest: &str) {
    let Some((id, encoded)) = rest.split_once('/') else {
        let _ = request.respond(error(404, "no such item"));
        return;
    };
    let decoded: Vec<String> = encoded.split('/').map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
    let item = {
        let addons = app.addons.lock().unwrap();
        let addon = addons.get(id).filter(|a| a.enabled);
        let find = |path: &[String]| addon.and_then(|a| Some((a.paths.get(&path.join("/"))?.clone(), a.manifest.hosts.clone())));
        // An item's own path, or a playlist under it.
        find(&decoded).map(|(item, hosts)| (item, hosts, None))
            .or_else(|| find(&decoded[..decoded.len() - 1]).filter(|_| decoded.len() > 1).map(|(item, hosts)| (item, hosts, decoded.last().cloned())))
    };
    let Some((item, hosts, child)) = item else {
        let _ = request.respond(error(404, "no such item"));
        return;
    };
    match request.method() {
        Method::Delete if child.is_none() => {
            app.delete_item(id, &item);
            let _ = request.respond(Response::empty(204));
        }
        Method::Get | Method::Head => {
            let opened = match app.open(id, &item) {
                Ok(opened) => opened,
                Err(e) => {
                    app::warn(id, &format!("open({item}): {e}"));
                    let _ = request.respond(error(502, &e));
                    return;
                }
            };
            if let Some(playlists) = &opened.playlists {
                let file = encoded.split('/').next_back().unwrap_or_default();
                let text = match &child {
                    None => Some(playlists.master_under(&format!("{file}/"))),
                    Some(name) => playlists.children.get(name).cloned(),
                };
                let _ = request.respond(match text {
                    Some(text) => Response::from_string(text).with_header(header("Content-Type", "application/vnd.apple.mpegurl")),
                    None => error(404, "no such playlist"),
                });
                return;
            }
            if child.is_some() {
                let _ = request.respond(error(404, "no such item"));
                return;
            }
            if opened.headers.is_empty() {
                let _ = request.respond(Response::empty(302).with_header(header("Location", &opened.url)));
                return;
            }
            if let Err(e) = proxy(request, &hosts, &opened.url, &opened.headers, None) {
                app.forget_open(id, &item);
                app::warn(id, &format!("{item}: {e}"));
            }
        }
        _ => {
            let _ = request.respond(error(405, "GET, HEAD or DELETE"));
        }
    }
}

/// Relays a link with the client's Range, through the addon's sandboxed client. With
/// `playlists` (the relay's own base), an HLS playlist comes back with its URIs pointed
/// through the relay. An error, or an answer of 400 or more, is the caller's to log.
fn proxy(request: Request, hosts: &net::Hosts, url: &str, extra: &[(String, String)], playlists: Option<&str>) -> Result<(), String> {
    let mut headers = extra.to_vec();
    headers.push(("Accept-Encoding".into(), "identity".into()));
    if let Some(range) = request.headers().iter().find(|h| h.field.equiv("Range")) {
        headers.push(("Range".into(), range.value.to_string()));
    }
    let method = if request.method() == &Method::Head { "HEAD" } else { "GET" };
    let (upstream, final_url) = match net::request(&net::sandboxed_agent(hosts), hosts, method, url, &headers, None) {
        Ok(answer) => answer,
        Err(e) => {
            let _ = request.respond(error(502, &e));
            return Err(e);
        }
    };
    let status = upstream.status();
    let kind = upstream.header("Content-Type").unwrap_or_default().to_ascii_lowercase();
    if let Some(base) = playlists.filter(|_| status < 300 && (kind.contains("mpegurl") || final_url.split(['?', '#']).next().unwrap_or("").ends_with(".m3u8"))) {
        let mut text = String::new();
        upstream.into_reader().take(4 << 20).read_to_string(&mut text).map_err(|e| e.to_string())?;
        let playlist = url::Url::parse(&final_url).map_err(|e| e.to_string())?;
        let rewritten = crate::probe::rewrite_playlist(&text, &playlist, base);
        let _ = request.respond(Response::from_string(rewritten).with_header(header("Content-Type", "application/vnd.apple.mpegurl")));
        return Ok(());
    }
    let copied: Vec<Header> = ["Content-Type", "Content-Range", "Accept-Ranges", "Last-Modified", "ETag"]
        .iter()
        .filter_map(|name| upstream.header(name).map(|value| header(name, value)))
        .collect();
    let length = upstream.header("Content-Length").and_then(|v| v.parse().ok());
    // With the origin's length, never chunked (tiny_http chunks 32 KB and more by default): FFmpeg
    // 4.4 reads `Transfer-Encoding: chunked` after `Content-Range` as a file of unknown size, then
    // reads past its end, each read a 416, until its probe is killed.
    let response = Response::new(StatusCode(status), copied, upstream.into_reader(), length, None).with_chunked_threshold(usize::MAX);
    let _ = request.respond(response);
    if status >= 400 {
        return Err(format!("the link answered {status}"));
    }
    Ok(())
}

/// `/relay/{token}/{rest}`: what an addon's `ffprobe()` reads, while it runs. `rest` is the
/// link's own file name, a URI relative to it, or `abs/<an absolute URI, percent-encoded>/<name>`.
fn relay(app: &Arc<App>, request: Request, rest: &str) {
    let (token, rest) = rest.split_once('/').unwrap_or((rest, ""));
    let Some(relay) = app.prober.relay(token) else {
        let _ = request.respond(error(404, "no such probe"));
        return;
    };
    let base = format!("http://{}/relay/{token}", net::reachable(app.listen));
    if let Some(text) = relay.playlists.as_ref().and_then(|p| p.relayed(rest, &base)) {
        let _ = request.respond(Response::from_string(text).with_header(header("Content-Type", "application/vnd.apple.mpegurl")));
        return;
    }
    let target = if let Some(absolute) = rest.strip_prefix("abs/") {
        let encoded = absolute.split('/').next().unwrap_or("");
        Some(percent_encoding::percent_decode_str(encoded).decode_utf8_lossy().into_owned())
    } else {
        url::Url::parse(&relay.url).ok().and_then(|link| {
            let own = link.path_segments().and_then(|mut s| s.next_back()).unwrap_or("");
            if rest == own || rest == "media" { Some(relay.url.clone()) } else { link.join(rest).ok().map(String::from) }
        })
    };
    let Some(target) = target else {
        let _ = request.respond(error(400, "not a link"));
        return;
    };
    let _ = proxy(request, &relay.hosts, &target, &relay.headers, Some(&base));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, SocketAddr, UdpSocket};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// This machine's address on its network, to reach the host as a peer that is not loopback.
    fn lan_ip() -> Option<IpAddr> {
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        let ip = socket.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    }

    fn start(listen: &str, hosts: Json, source: &str) -> (Arc<App>, SocketAddr) {
        start_with(listen, hosts, source, None)
    }

    fn start_with(listen: &str, hosts: Json, source: &str, token: Option<&str>) -> (Arc<App>, SocketAddr) {
        start_manifest(listen, json!({"id": "demo", "name": "Demo", "version": "1.0.0", "hosts": hosts}), source, token)
    }

    fn start_manifest(listen: &str, manifest: Json, source: &str, token: Option<&str>) -> (Arc<App>, SocketAddr) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let server = tiny_http::Server::http(listen).unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let data = std::env::temp_dir().join(format!("rex-addons-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&data);
        let app = App::new(data, "http://127.0.0.1:9".into(), addr, None, None);
        app.install(&manifest, source, None).unwrap();
        app.addons.lock().unwrap().get_mut(manifest["id"].as_str().unwrap()).unwrap().paths.insert("films/a film.mp4".into(), "a".into());
        let served = app.clone();
        let token = token.map(str::to_string);
        std::thread::spawn(move || serve(served, server, token));
        (app, addr)
    }

    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new().redirects(0).build()
    }

    fn status(result: Result<ureq::Response, ureq::Error>) -> ureq::Response {
        match result {
            Ok(r) | Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn management_api_answers_only_this_device() {
        let (_, addr) = start("0.0.0.0:0", json!([]), "export default { items() { return []; }, open() {} };");
        let port = addr.port();
        let answer: Json = serde_json::from_str(&status(agent().get(&format!("http://127.0.0.1:{port}/api/status")).call()).into_string().unwrap()).unwrap();
        assert_eq!(answer["addons"][0]["id"], "demo");
        assert_eq!(answer["rex"]["reachable"], false);
        let Some(lan) = lan_ip() else {
            eprintln!("SKIP: no network address to come from");
            return;
        };
        // A header saying otherwise changes nothing: the socket's peer decides.
        let refused = status(agent().get(&format!("http://{lan}:{port}/api/status")).set("X-Forwarded-For", "127.0.0.1").call());
        assert_eq!(refused.status(), 403);
        // rex on the home network may still read bytes.
        let bytes = status(agent().get(&format!("http://{lan}:{port}/bytes/demo/nothing")).call());
        assert_eq!(bytes.status(), 404);
    }

    #[test]
    fn management_api_needs_the_token_and_a_host_of_its_own() {
        let (_, addr) = start_with("127.0.0.1:0", json!([]), "export default { items() { return []; }, open() {} };", Some("s3cret"));
        let api = format!("http://{addr}/api/status");
        assert_eq!(status(agent().get(&api).call()).status(), 401);
        assert_eq!(status(agent().get(&api).set("Authorization", "Bearer wrong!").call()).status(), 401);
        assert_eq!(status(agent().get(&api).set("Authorization", "s3cret").call()).status(), 401);
        // A text/plain POST from a web page carries no Authorization header.
        assert_eq!(status(agent().post(&format!("http://{addr}/api/addons/demo/sync")).set("Content-Type", "text/plain").send_string("{}")).status(), 401);
        assert_eq!(status(agent().get(&api).set("Authorization", "Bearer s3cret").call()).status(), 200);
        let port = addr.port();
        for host in [format!("localhost:{port}"), format!("LOCALHOST:{port}"), format!("[::1]:{port}")] {
            assert_eq!(status(agent().get(&api).set("Authorization", "Bearer s3cret").set("Host", &host).call()).status(), 200, "{host}");
        }
        // DNS rebinding: a page on evil.example resolved to 127.0.0.1, token or not.
        for host in [format!("evil.example:{port}"), "localhost:1".to_string(), "localhost".to_string()] {
            assert_eq!(status(agent().get(&api).set("Authorization", "Bearer s3cret").set("Host", &host).call()).status(), 403, "{host}");
        }
        // rex and ffprobe read bytes with neither.
        assert_eq!(status(agent().get(&format!("http://{addr}/bytes/demo/nothing")).set("Host", "evil.example").call()).status(), 404);
        // Without a token the API stays open to this device, but never to another Host.
        let (_, open) = start("127.0.0.1:0", json!([]), "export default { items() { return []; }, open() {} };");
        assert_eq!(status(agent().get(&format!("http://{open}/api/status")).call()).status(), 200);
        assert_eq!(status(agent().get(&format!("http://{open}/api/status")).set("Host", "evil.example").call()).status(), 403);
    }

    #[test]
    fn bytes_redirect_when_the_link_needs_nothing() {
        let (_, addr) = start(
            "127.0.0.1:0",
            json!(["cdn.example.com"]),
            "export default { items() { return []; }, open(id) { return { url: 'https://cdn.example.com/' + id + '.mp4' }; } };",
        );
        let response = status(agent().get(&format!("http://{addr}/bytes/demo/films/a%20film.mp4")).call());
        assert_eq!(response.status(), 302);
        assert_eq!(response.header("location"), Some("https://cdn.example.com/a.mp4"));
        assert_eq!(status(agent().get(&format!("http://{addr}/bytes/demo/films/other.mp4")).call()).status(), 404);
    }

    #[test]
    fn bytes_refuse_a_link_off_the_hosts() {
        let (_, addr) = start("127.0.0.1:0", json!(["cdn.example.com"]), "export default { items() { return []; }, open() { return { url: 'https://evil.example/x.mp4' }; } };");
        let response = status(agent().get(&format!("http://{addr}/bytes/demo/films/a%20film.mp4")).call());
        assert_eq!(response.status(), 502);
        assert!(response.into_string().unwrap().contains("not on the addon's hosts"));
    }

    #[test]
    fn bytes_are_relayed_with_range_when_the_link_needs_headers() {
        let Some(lan) = lan_ip() else {
            eprintln!("SKIP: no network address for the upstream (loopback is never fetched)");
            return;
        };
        let upstream = tiny_http::Server::http("0.0.0.0:0").unwrap();
        let upstream_port = upstream.server_addr().to_ip().unwrap().port();
        let file: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let served = file.clone();
        std::thread::spawn(move || {
            for request in upstream.incoming_requests() {
                let referer = request.headers().iter().any(|h| h.field.equiv("Referer") && h.value == "https://site.example/");
                let range = request.headers().iter().find(|h| h.field.equiv("Range")).map(|h| h.value.to_string());
                let response = match (referer, range.as_deref()) {
                    (false, _) => Response::from_string("no referer").with_status_code(403),
                    (true, Some("bytes=100-199")) => Response::from_data(served[100..200].to_vec())
                        .with_status_code(206)
                        .with_header(header("Content-Range", "bytes 100-199/1000"))
                        .with_header(header("Content-Type", "video/mp4")),
                    (true, _) => Response::from_data(served.clone()).with_header(header("Content-Type", "video/mp4")),
                };
                let _ = request.respond(response);
            }
        });
        let source = format!(
            "export default {{ items() {{ return []; }}, open() {{ return {{ url: 'http://{lan}:{upstream_port}/file.mp4', headers: {{ Referer: 'https://site.example/' }} }}; }} }};"
        );
        let (_, addr) = start("127.0.0.1:0", json!([lan.to_string()]), &source);
        let response = status(agent().get(&format!("http://{addr}/bytes/demo/films/a%20film.mp4")).set("Range", "bytes=100-199").call());
        assert_eq!(response.status(), 206);
        assert_eq!(response.header("content-range"), Some("bytes 100-199/1000"));
        assert_eq!(response.header("content-length"), Some("100"));
        let mut body = Vec::new();
        response.into_reader().read_to_end(&mut body).unwrap();
        assert_eq!(body, file[100..200]);
        let whole = status(agent().get(&format!("http://{addr}/bytes/demo/films/a%20film.mp4")).call());
        assert_eq!(whole.status(), 200);
        assert_eq!(whole.header("content-length"), Some("1000"));
    }

    /// The status line and headers of a GET with `range`, read off the wire as a client sees them.
    fn head_of(addr: SocketAddr, path: &str, range: &str) -> (u16, Vec<(String, String)>) {
        use std::io::Write;
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: {addr}\r\nRange: {range}\r\nConnection: close\r\n\r\n").unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            head.push(byte[0]);
        }
        let text = String::from_utf8(head).unwrap();
        let mut lines = text.lines();
        let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
        let headers = lines.filter_map(|l| l.split_once(": ")).map(|(n, v)| (n.to_ascii_lowercase(), v.to_string())).collect();
        (status, headers)
    }

    /// What ffprobe 4.4 asked the relay for and what the origin answered, replayed: the relay, and
    /// /bytes/ for a link with headers, answer with the origin's status, Content-Range and
    /// Content-Length, never chunked. Chunked, FFmpeg 4.4 takes the file's size as unknown (its
    /// `Transfer-Encoding: chunked` line comes after `Content-Range`), reads past the end and gets a
    /// 416 for each read until it is killed (tests/fixtures/relay-ffprobe44.json).
    #[test]
    fn relayed_bytes_keep_the_origins_length() {
        let Some(lan) = lan_ip() else {
            eprintln!("SKIP: no network address for the upstream (loopback is never fetched)");
            return;
        };
        let fixture: Json = serde_json::from_str(include_str!("../tests/fixtures/relay-ffprobe44.json")).unwrap();
        let exchange = fixture["exchange"].as_array().unwrap().clone();
        // The origin, answering each recorded Range as archive.org did, its body zeros.
        let origin = tiny_http::Server::http("0.0.0.0:0").unwrap();
        let link = format!("http://{lan}:{}/his_girl_friday.mp4", origin.server_addr().to_ip().unwrap().port());
        let answers = exchange.clone();
        std::thread::spawn(move || {
            for request in origin.incoming_requests() {
                let range = request.headers().iter().find(|h| h.field.equiv("Range")).map(|h| h.value.to_string());
                let Some(answer) = answers.iter().find(|a| a["range"].as_str() == range.as_deref()) else {
                    let _ = request.respond(error(400, "a range not in the fixture"));
                    continue;
                };
                let recorded = answer["origin"]["headers"].as_object().unwrap();
                let length: usize = recorded["Content-Length"].as_str().unwrap().parse().unwrap();
                let headers = recorded.iter().filter(|(name, _)| *name != "Content-Length").map(|(name, value)| header(name, value.as_str().unwrap())).collect();
                let status = StatusCode(answer["origin"]["status"].as_u64().unwrap() as u16);
                let body = std::io::Read::take(std::io::repeat(0), length as u64);
                let _ = request.respond(Response::new(status, headers, body, Some(length), None).with_chunked_threshold(usize::MAX));
            }
        });
        let source = format!("export default {{ items() {{ return []; }}, open() {{ return {{ url: '{link}', headers: {{ Referer: 'https://archive.org/' }} }}; }} }};");
        let (app, addr) = start("127.0.0.1:0", json!([lan.to_string()]), &source);
        let token = app.prober.hold(crate::probe::Relay { url: link, headers: Vec::new(), hosts: net::Hosts(vec![lan.to_string()]), playlists: None });
        for path in [format!("/relay/{token}/his_girl_friday.mp4"), "/bytes/demo/films/a%20film.mp4".to_string()] {
            for answer in &exchange {
                let range = answer["range"].as_str().unwrap();
                let (status, headers) = head_of(addr, &path, range);
                let got = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
                let origin = &answer["origin"];
                assert_eq!(status as u64, origin["status"].as_u64().unwrap(), "{path} {range}");
                assert_eq!(got("content-length"), origin["headers"]["Content-Length"].as_str(), "{path} {range}: {headers:?}");
                assert_eq!(got("content-range"), origin["headers"]["Content-Range"].as_str(), "{path} {range}: {headers:?}");
                assert_eq!(got("transfer-encoding"), None, "{path} {range}: {headers:?}");
            }
        }
    }

    /// An addon whose `open()` writes a master playlist and two media playlists over `link`.
    fn playlist_addon(host: &str, link: &str) -> (Arc<App>, SocketAddr) {
        let source = format!(
            r#"const media = (url) => `#EXTM3U\n#EXT-X-MAP:URI="${{url}}",BYTERANGE="10@0"\n#EXTINF:5.0,\n#EXT-X-BYTERANGE:100@10\n${{url}}\n#EXT-X-ENDLIST\n`;
               export default {{ items() {{ return []; }}, open() {{ return {{
                 playlist: '#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",NAME="Original",URI="audio.m3u8"\n#EXT-X-STREAM-INF:BANDWIDTH=9,AUDIO="a"\nvideo.m3u8\n',
                 playlists: {{ "video.m3u8": media("{link}"), "audio.m3u8": media("{link}") }} }}; }} }};"#
        );
        let manifest = json!({"id": "demo", "name": "Demo", "version": "1.0.0", "host": host, "hosts": ["cdn.example.com"]});
        start_manifest("127.0.0.1:0", manifest, &source, None)
    }

    #[test]
    fn playlists_an_addon_writes_are_served_at_the_item_and_under_it() {
        let (_, addr) = playlist_addon("0.4", "https://cdn.example.com/v.mp4?n=1");
        let item = format!("http://{addr}/bytes/demo/films/a%20film.mp4");
        let master = status(agent().get(&item).call());
        assert_eq!((master.status(), master.header("content-type")), (200, Some("application/vnd.apple.mpegurl")));
        let text = master.into_string().unwrap();
        assert!(text.contains("URI=\"a%20film.mp4/audio.m3u8\"") && text.ends_with("\na%20film.mp4/video.m3u8"), "{text}");
        // Where FFmpeg resolves the master's names to.
        let video = url::Url::parse(&item).unwrap().join("a%20film.mp4/video.m3u8").unwrap();
        let child = status(agent().get(video.as_str()).call());
        assert_eq!(child.status(), 200);
        assert!(child.into_string().unwrap().contains("#EXT-X-BYTERANGE:100@10\nhttps://cdn.example.com/v.mp4?n=1"));
        assert_eq!(status(agent().head(&item).call()).status(), 200);
        assert_eq!(status(agent().get(&format!("{item}/other.m3u8")).call()).status(), 404);
        assert_eq!(status(agent().get(&format!("http://{addr}/bytes/demo/films/other.mp4/video.m3u8")).call()).status(), 404);
        // A playlist under an item is not the item: it cannot be deleted.
        assert_eq!(status(agent().delete(video.as_str()).call()).status(), 405);
    }

    #[test]
    fn playlists_are_refused_off_the_hosts_and_without_host_0_4() {
        for (host, link, want) in [
            ("0.4", "https://evil.example/v.mp4", "https://evil.example/..., is not on the addon's hosts"),
            ("0.4", "v.mp4", "v.mp4 is no playlist of its own"),
            ("0.3", "https://cdn.example.com/v.mp4", "open() returned playlists, which need \"host\": \"0.4\" in the manifest"),
        ] {
            let (_, addr) = playlist_addon(host, link);
            let response = status(agent().get(&format!("http://{addr}/bytes/demo/films/a%20film.mp4")).call());
            assert_eq!(response.status(), 502);
            let body: Json = serde_json::from_str(&response.into_string().unwrap()).unwrap();
            assert!(body["error"].as_str().unwrap().contains(want), "{body}");
        }
    }

    #[test]
    fn a_delete_from_rex_stops_the_item() {
        let (app, addr) = start("127.0.0.1:0", json!([]), "export default { items() { return []; }, open() {} };");
        let response = status(agent().delete(&format!("http://{addr}/bytes/demo/films/a%20film.mp4")).call());
        assert_eq!(response.status(), 204);
        assert!(app.addons.lock().unwrap()["demo"].deleted.contains("a"));
        assert_eq!(status(agent().delete(&format!("http://{addr}/bytes/demo/films/a%20film.mp4")).call()).status(), 404);
    }

    #[test]
    fn deleted_titles_are_counted_and_restored() {
        let (app, addr) = start("127.0.0.1:0", json!([]), "export default { items() { return []; }, open() {} };");
        assert_eq!(status(agent().delete(&format!("http://{addr}/bytes/demo/films/a%20film.mp4")).call()).status(), 204);
        let read = |r: ureq::Response| -> Json { serde_json::from_str(&r.into_string().unwrap()).unwrap() };
        assert_eq!(read(status(agent().get(&format!("http://{addr}/api/addons/demo")).call()))["deleted"], 1);
        let restored = status(agent().post(&format!("http://{addr}/api/addons/demo/restore")).call());
        assert_eq!(restored.status(), 200);
        assert_eq!(read(restored)["deleted"], 0);
        assert!(app.addons.lock().unwrap()["demo"].deleted.is_empty());
        let saved = crate::addons::read_json(&app.data.join("addons/demo/state.json")).unwrap();
        assert_eq!(saved["deleted"], json!([]));
        assert_eq!(status(agent().post(&format!("http://{addr}/api/addons/nope/restore")).call()).status(), 404);
    }

    /// An addon's recent log: its own console output and the host's lines about it, masked as
    /// they were logged, the last `lines` of them; nothing of another addon's.
    #[test]
    fn an_addons_log_is_its_last_lines_as_logged() {
        let (app, addr) = start_manifest(
            "127.0.0.1:0",
            json!({"id": "logged", "name": "Logged", "version": "1.0.0", "config": [{"key": "token", "label": "Token", "type": "secret"}]}),
            "export default { items() { return []; }, open(id, ctx) { console.log('opening', id, 'with', ctx.config.token); console.warn('x'.repeat(10000)); throw new Error('nope'); } };",
            None,
        );
        // Disabled, so the new settings start no sync whose lines would land among these.
        app.addons.lock().unwrap().get_mut("logged").unwrap().enabled = false;
        app.set_config("logged", &json!({"token": "SECRET-123"})).unwrap();
        app::log("other", "another addon's line");
        let _ = app.open("logged", "a");
        app::warn("logged", "open(a): nope");
        let read = |query: &str| -> (u16, Json) {
            let response = status(agent().get(&format!("http://{addr}/api/addons/logged/log{query}")).call());
            (response.status(), serde_json::from_str(&response.into_string().unwrap()).unwrap())
        };
        let (code, all) = read("");
        assert_eq!(code, 200);
        let lines: Vec<&str> = all["lines"].as_array().unwrap().iter().map(|l| l["line"].as_str().unwrap()).collect();
        assert!(lines.contains(&"[addon logged] opening a with ********"), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("WARN [addon logged] xxx") && l.len() == 4096), "{lines:?}");
        assert_eq!(lines.last(), Some(&"WARN [logged] open(a): nope"));
        assert!(!lines.iter().any(|l| l.contains("SECRET-123") || l.contains("another addon")), "{lines:?}");
        assert!(all["lines"][0]["at"].as_u64().unwrap() > 1_700_000_000);
        let (_, last) = read("?lines=1");
        assert_eq!(last["lines"], json!([all["lines"].as_array().unwrap().last().unwrap()]));
        assert_eq!(read("?lines=many").0, 400);
        assert_eq!(status(agent().get(&format!("http://{addr}/api/addons/nope/log")).call()).status(), 404);
    }

    #[test]
    fn config_is_validated_and_secrets_masked() {
        let (_, addr) = start("127.0.0.1:0", json!([]), "export default { items() { return []; }, open() {} };");
        let url = format!("http://{addr}/api/addons/demo/config");
        // The test manifest has no settings at all.
        let refused = status(agent().put(&url).send_string(r#"{"token": "x"}"#));
        assert_eq!(refused.status(), 400);
        assert!(refused.into_string().unwrap().contains("no setting"));
        assert_eq!(status(agent().get(&format!("http://{addr}/api/addons/nope")).call()).status(), 404);
    }
}
