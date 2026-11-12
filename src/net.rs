//! Who may call the host, which hosts an addon may reach, and the HTTP client both use.
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

pub const USER_AGENT: &str = concat!("rex-addons/", env!("CARGO_PKG_VERSION"));

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// This device: 127/8, ::1 (an IPv4-mapped one included).
pub fn is_loopback(ip: IpAddr) -> bool {
    canonical(ip).is_loopback()
}

/// This device or a private network, as rex's feed counts them: loopback, RFC 1918, 100.64/10
/// (Tailscale), link-local, fc00::/7 and fe80::/10, and the unspecified address.
pub fn is_private(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// The hosts an addon may reach: its manifest's (`listed`), `example.com` exactly, `*.example.com`
/// any name under it, `*` any public name, each on any port; and each host and port a person typed
/// into its `host` settings (`granted`), that port only. An address on this device is never
/// reachable, and one on a private network only when listed exactly or granted, never through a
/// wildcard.
#[derive(Clone, Debug, Default)]
pub struct Hosts {
    pub listed: Vec<String>,
    pub granted: Vec<(String, u16)>,
}

impl From<Vec<String>> for Hosts {
    fn from(listed: Vec<String>) -> Hosts {
        Hosts { listed, granted: Vec::new() }
    }
}

fn bare(host: &str) -> String {
    host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase()
}

/// A granted host and port as the API shows it: `192.168.1.20:8096`, `[fd00::5]:8920`.
pub fn shown(host: &str, port: u16) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

impl Hosts {
    pub fn valid_pattern(pattern: &str) -> bool {
        let name = pattern.strip_prefix("*.").unwrap_or(pattern);
        pattern == "*"
            || (!name.is_empty()
                && name.len() <= 253
                && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | ':')))
    }

    pub fn allows(&self, host: &str, port: u16) -> bool {
        let host = bare(host);
        let literal = host.parse::<IpAddr>().is_ok() || host == "localhost";
        self.granted.iter().any(|(granted, on)| *granted == host && *on == port)
            || self.listed.iter().any(|pattern| {
                pattern == &host
                    || (!literal && pattern == "*")
                    || (!literal && pattern.strip_prefix("*.").is_some_and(|parent| host.ends_with(&format!(".{parent}"))))
            })
    }

    /// `allows` for a link, or why not: its host, and for a host a setting grants on other ports
    /// only, which ports those are.
    pub fn check(&self, link: &url::Url) -> Result<(), String> {
        let (host, port) = (link.host_str().unwrap_or(""), link.port_or_known_default().unwrap_or(0));
        match self.allows(host, port) {
            true => Ok(()),
            false => Err(format!("{host} is not in the addon's hosts{}", self.port_note(host, port))),
        }
    }

    /// For a host its settings grant, but not on `port`: `: its settings allow 192.168.1.20 on port
    /// 8096 only, not 32400`. Else nothing.
    pub fn port_note(&self, host: &str, port: u16) -> String {
        let host = bare(host);
        let ports: Vec<String> = self.granted.iter().filter(|(granted, _)| *granted == host).map(|(_, on)| on.to_string()).collect();
        match ports.is_empty() || ports.contains(&port.to_string()) {
            true => String::new(),
            false => format!(": its settings allow {host} on port {} only, not {port}", ports.join(" and ")),
        }
    }

    /// Whether a private address `host` resolves to may be connected to on `port`.
    fn named(&self, host: &str, port: u16) -> bool {
        self.listed.iter().any(|pattern| pattern == host) || self.granted.iter().any(|(granted, on)| granted == host && *on == port)
    }
}

/// The host and ports an address a person typed into a `host` setting grants, as `allows` and the
/// resolver compare them; or why it grants none. `http(s)://name-or-ip[:port][/path]` grants its
/// port, or its scheme's (80, 443); `name-or-ip:port` that port; a bare `name-or-ip` both 80 and
/// 443, as the addon may put either scheme before it. Never a wildcard, never with a user or
/// password, never this device.
pub fn typed_grant(text: &str) -> Result<(String, Vec<u16>), &'static str> {
    const SHAPE: &str = "give an address such as 192.168.1.20:8096 or http://nas.local:8096";
    let text = text.trim();
    let schemed = text.contains("://");
    let link = if schemed { text.to_string() } else { format!("http://{text}") };
    let url = url::Url::parse(&link).map_err(|_| SHAPE)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("give an http:// or https:// address");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("leave the user name and password out of the address");
    }
    // The parsed host, so 127.1, 0x7f.0.0.1 and [::ffff:127.0.0.1] are what they are.
    let this_device = match url.host() {
        Some(url::Host::Domain(name)) => {
            if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-')) {
                return Err(SHAPE);
            }
            name == "localhost" || name.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => is_this_device(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => is_this_device(IpAddr::V6(ip)),
        None => return Err(SHAPE),
    };
    if this_device {
        return Err("that is this device, which addons never reach");
    }
    let ports = match schemed {
        true => vec![url.port_or_known_default().unwrap_or(0)],
        // A port typed is the same under either scheme (http elides :80, https :443); none is not.
        false => {
            let under = |scheme: &str| url::Url::parse(&format!("{scheme}://{text}")).ok().and_then(|u| u.port_or_known_default());
            match (under("http"), under("https")) {
                (Some(http), Some(https)) if http == https => vec![http],
                _ => vec![80, 443],
            }
        }
    };
    if ports.contains(&0) {
        return Err(SHAPE);
    }
    Ok((bare(url.host_str().unwrap_or_default()), ports))
}

/// What the resolver never connects to, named or not.
fn is_this_device(ip: IpAddr) -> bool {
    let ip = canonical(ip);
    ip.is_loopback() || ip.is_unspecified()
}

/// The client addons fetch through: no redirects of its own (each hop is checked against the
/// hosts), and a resolver that refuses this device and, unless named, private networks, so a
/// name that resolves to 127.0.0.1 cannot reach rex or this host's API.
pub fn sandboxed_agent(hosts: &Hosts) -> ureq::Agent {
    let hosts = hosts.clone();
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .user_agent(USER_AGENT)
        .resolver(move |netloc: &str| -> io::Result<Vec<SocketAddr>> {
            // ureq always gives the port.
            let (host, port) = netloc.rsplit_once(':').unwrap_or((netloc, ""));
            let host = bare(host);
            let port = port.parse().unwrap_or(0);
            let named = hosts.named(&host, port);
            let kept: Vec<SocketAddr> = netloc
                .to_socket_addrs()?
                .filter(|addr| !is_this_device(addr.ip()) && (named || !is_private(addr.ip())))
                .collect();
            if kept.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{host} is on this device or a private network{}", hosts.port_note(&host, port)),
                ));
            }
            Ok(kept)
        })
        .build()
}

/// A plain client: installing from a URL the person gave, and talking to rex.
pub fn plain_agent(read: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(5)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(read)
        .user_agent(USER_AGENT)
        .build()
}

/// One request, following redirects itself so each hop's host is checked: the response
/// (any status) and the URL it came from. Errors name the host only: an addon's links can
/// carry an account key in their path.
pub fn request(
    agent: &ureq::Agent,
    hosts: &Hosts,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<(ureq::Response, String), String> {
    request_until(agent, hosts, method, url, headers, body, None)
}

/// `request`, with every hop and the response's body bounded by `deadline` in all, not per read.
pub fn request_until(
    agent: &ureq::Agent,
    hosts: &Hosts,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    deadline: Option<Instant>,
) -> Result<(ureq::Response, String), String> {
    let mut url = url::Url::parse(url).map_err(|e| format!("not a link: {e}"))?;
    let mut method = method.to_string();
    let mut body = body;
    for _ in 0..10 {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("{}: only http and https", url.scheme()));
        }
        hosts.check(&url)?;
        let host = url.host_str().unwrap_or("");
        let mut req = agent.request(&method, url.as_str());
        if let Some(deadline) = deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{host}: out of time"));
            }
            req = req.timeout(left);
        }
        for (name, value) in headers {
            req = req.set(name, value);
        }
        let sent = match body {
            Some(bytes) => req.send_bytes(bytes),
            None => req.call(),
        };
        let response = match sent {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(error)) => {
                // The cause (a refusal of ours, a connection error) says more than ureq's message.
                let why = std::error::Error::source(&error).map(|e| e.to_string()).or(error.message().map(str::to_string));
                return Err(format!("{host}: {}{}", error.kind(), why.map(|why| format!(": {why}")).unwrap_or_default()));
            }
        };
        let status = response.status();
        let location = response.header("location").map(str::to_string);
        match (status, location) {
            (301 | 302 | 303 | 307 | 308, Some(location)) => {
                url = url.join(&location).map_err(|e| format!("{}: a redirect that is no link: {e}", url.host_str().unwrap_or("")))?;
                if status == 303 || (matches!(status, 301 | 302) && method == "POST") {
                    method = "GET".into();
                    body = None;
                }
            }
            _ => return Ok((response, url.to_string())),
        }
    }
    Err(format!("{}: too many redirects", url.host_str().unwrap_or("")))
}

/// The address a listen address is reached at from this device: 0.0.0.0 and :: become loopback.
pub fn reachable(listen: SocketAddr) -> SocketAddr {
    match listen.ip() {
        ip if ip.is_unspecified() => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listen.port()),
        _ => listen,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_match_exact_wildcard_and_any() {
        // A manifest's hosts, on any port.
        let hosts = Hosts::from(vec!["archive.org".into(), "*.archive.org".into()]);
        assert!(hosts.allows("archive.org", 443) && hosts.allows("archive.org", 8443));
        assert!(hosts.allows("ia800.us.archive.org", 80));
        assert!(!hosts.allows("evilarchive.org", 443));
        assert!(!hosts.allows("example.com", 443));
        let any = Hosts::from(vec!["*".into()]);
        assert!(any.allows("example.com", 443));
        assert!(!any.allows("127.0.0.1", 80));
        assert!(!any.allows("localhost", 80));
        assert!(!any.allows("[::1]", 80));
    }

    /// A granted host on its port only, the scheme's default port counting as typed, an IPv6
    /// literal too; and a refusal that names the port.
    #[test]
    fn granted_hosts_match_their_port_only() {
        let hosts = Hosts { listed: vec!["example.com".into()], granted: vec![("nas".into(), 80), ("fd00::5".into(), 8920), ("192.168.1.20".into(), 8096)] };
        let check = |link: &str| hosts.check(&url::Url::parse(link).unwrap());
        assert_eq!(check("http://nas/library"), Ok(()));
        assert_eq!(check("http://NAS:80/"), Ok(()));
        assert_eq!(check("https://nas/").unwrap_err(), "nas is not in the addon's hosts: its settings allow nas on port 80 only, not 443");
        assert_eq!(check("http://nas:8080/").unwrap_err(), "nas is not in the addon's hosts: its settings allow nas on port 80 only, not 8080");
        assert_eq!(check("https://[fd00::5]:8920/"), Ok(()));
        assert_eq!(check("http://[fd00::5]:8921/").unwrap_err(), "[fd00::5] is not in the addon's hosts: its settings allow fd00::5 on port 8920 only, not 8921");
        assert_eq!(check("http://192.168.1.20:8096/Items"), Ok(()));
        assert_eq!(check("http://192.168.1.20:32400/rex/sources").unwrap_err(), "192.168.1.20 is not in the addon's hosts: its settings allow 192.168.1.20 on port 8096 only, not 32400");
        assert_eq!(check("http://192.168.1.21:8096/").unwrap_err(), "192.168.1.21 is not in the addon's hosts");
        assert_eq!(shown("fd00::5", 8920), "[fd00::5]:8920");
        assert_eq!(shown("nas", 80), "nas:80");
    }

    #[test]
    fn private_networks() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.1.20", "172.16.0.1", "100.64.0.1", "169.254.1.1", "::1", "fd00::1", "fe80::1", "::ffff:192.168.1.1", "0.0.0.0"] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "207.241.224.2", "2606:4700::1111", "100.128.0.1"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }

    /// Named exactly, as a manifest or a host setting would: every way of writing this device is
    /// still refused where the connection is made, a name that resolves to it (localhost) too.
    #[test]
    fn resolver_refuses_this_device_even_when_named() {
        for (named, link) in [
            ("127.0.0.1", "http://127.0.0.1:9/"),
            ("127.0.0.1", "http://127.1:9/"),
            ("::1", "http://[::1]:9/"),
            ("::ffff:7f00:1", "http://[::ffff:127.0.0.1]:9/"),
            ("0.0.0.0", "http://0.0.0.0:9/"),
            ("::", "http://[::]:9/"),
            ("localhost", "http://localhost:9/"),
        ] {
            let hosts = Hosts::from(vec![named.into()]);
            let err = request(&sandboxed_agent(&hosts), &hosts, "GET", link, &[], None).unwrap_err();
            assert!(err.contains("this device or a private network"), "{link}: {err}");
        }
        // Granted with its port, as a host setting would: the same.
        let hosts = Hosts { listed: Vec::new(), granted: vec![("localhost".into(), 9)] };
        let err = request(&sandboxed_agent(&hosts), &hosts, "GET", "http://localhost:9/", &[], None).unwrap_err();
        assert!(err.ends_with("localhost is on this device or a private network"), "{err}");
    }
}

#[cfg(test)]
mod redirect_tests {
    use super::*;

    #[test]
    fn each_redirect_hop_is_checked() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let to = tiny_http::Header::from_bytes("Location", "http://evil.example/steal").unwrap();
                let _ = request.respond(tiny_http::Response::empty(302).with_header(to));
            }
        });
        // An agent that leaves redirects to request(), as the sandboxed one does, minus its resolver.
        let agent = ureq::AgentBuilder::new().redirects(0).build();
        let err = request(&agent, &Hosts::from(vec!["127.0.0.1".into()]), "GET", &format!("http://127.0.0.1:{port}/"), &[], None).unwrap_err();
        assert_eq!(err, "evil.example is not in the addon's hosts");
    }

    /// A host and port a setting granted, on a private network: another port of the same address
    /// (rex's feed, on zen, 2026-11-12) is refused, fetched or redirected to, and by the resolver
    /// itself; so are other private addresses and this device.
    #[test]
    fn a_grant_reaches_its_port_and_nothing_else_private() {
        let Some(lan) = lan_ip() else {
            eprintln!("SKIP: no network address for the granted host (loopback is never fetched)");
            return;
        };
        let other = tiny_http::Server::http("0.0.0.0:0").unwrap();
        let other_port = other.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in other.incoming_requests() {
                let _ = request.respond(tiny_http::Response::from_string("rex's sources"));
            }
        });
        let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let to = match request.url() {
                    "/other-port" => format!("http://{lan}:{other_port}/rex/sources"),
                    "/lan" => "http://192.168.255.254/x".to_string(),
                    "/tailscale" => "http://100.64.0.9/x".to_string(),
                    "/device" => format!("http://127.0.0.1:{port}/ok"),
                    "/mapped" => format!("http://[::ffff:127.0.0.1]:{port}/ok"),
                    "/self" => "/ok".to_string(),
                    _ => {
                        let _ = request.respond(tiny_http::Response::from_string("ok"));
                        continue;
                    }
                };
                let _ = request.respond(tiny_http::Response::empty(302).with_header(tiny_http::Header::from_bytes("Location", to).unwrap()));
            }
        });
        let hosts = Hosts { listed: vec!["*".into()], granted: vec![(lan.to_string(), port)] };
        let agent = sandboxed_agent(&hosts);
        let get = |link: String| request(&agent, &hosts, "GET", &link, &[], None).map(|(r, _)| r.into_string().unwrap());
        let other_port_refused = format!("{lan} is not in the addon's hosts: its settings allow {lan} on port {port} only, not {other_port}");
        assert_eq!(get(format!("http://{lan}:{port}/self")).unwrap(), "ok");
        assert_eq!(get(format!("http://{lan}:{other_port}/rex/sources")).unwrap_err(), other_port_refused);
        assert_eq!(get(format!("http://{lan}:{port}/other-port")).unwrap_err(), other_port_refused);
        assert_eq!(get(format!("http://{lan}:{port}/lan")).unwrap_err(), "192.168.255.254 is not in the addon's hosts");
        assert_eq!(get(format!("http://{lan}:{port}/tailscale")).unwrap_err(), "100.64.0.9 is not in the addon's hosts");
        assert_eq!(get(format!("http://{lan}:{port}/device")).unwrap_err(), "127.0.0.1 is not in the addon's hosts");
        assert_eq!(get(format!("http://{lan}:{port}/mapped")).unwrap_err(), "[::ffff:7f00:1] is not in the addon's hosts");
        // Past that check, the resolver still connects to the granted port only.
        let err = agent.get(&format!("http://{lan}:{other_port}/rex/sources")).call().unwrap_err().to_string();
        assert!(err.contains("is on this device or a private network"), "{err}");
        assert!(agent.get(&format!("http://{lan}:{port}/ok")).call().is_ok());
        // The same address, not granted: `*` never reaches a private network.
        let public = Hosts::from(vec!["*".into()]);
        let err = request(&sandboxed_agent(&public), &public, "GET", &format!("http://{lan}:{port}/ok"), &[], None).unwrap_err();
        assert_eq!(err, format!("{lan} is not in the addon's hosts"));
    }

    /// This machine's network address: the sandbox never fetches loopback.
    fn lan_ip() -> Option<IpAddr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        let ip = socket.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    }
}
