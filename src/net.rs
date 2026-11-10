//! Who may call the host, which hosts an addon may reach, and the HTTP client both use.
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

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

/// The hosts a manifest lists: `example.com` exactly, `*.example.com` any name under it, `*`
/// any public name. An address on this device is never reachable, and one on a private network
/// only when the manifest names that host exactly, never through a wildcard.
#[derive(Clone, Debug, Default)]
pub struct Hosts(pub Vec<String>);

impl Hosts {
    pub fn valid_pattern(pattern: &str) -> bool {
        let name = pattern.strip_prefix("*.").unwrap_or(pattern);
        pattern == "*"
            || (!name.is_empty()
                && name.len() <= 253
                && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | ':')))
    }

    pub fn allows(&self, host: &str) -> bool {
        let host = host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
        let literal = host.parse::<IpAddr>().is_ok() || host == "localhost";
        self.0.iter().any(|pattern| {
            pattern == &host
                || (!literal && pattern == "*")
                || (!literal && pattern.strip_prefix("*.").is_some_and(|parent| host.ends_with(&format!(".{parent}"))))
        })
    }

    fn named(&self, host: &str) -> bool {
        self.0.iter().any(|pattern| pattern == host)
    }
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
            let host = netloc.rsplit_once(':').map_or(netloc, |(host, _)| host);
            let host = host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
            let named = hosts.named(&host);
            let kept: Vec<SocketAddr> = netloc
                .to_socket_addrs()?
                .filter(|addr| {
                    let ip = canonical(addr.ip());
                    !ip.is_loopback() && !ip.is_unspecified() && (named || !is_private(ip))
                })
                .collect();
            if kept.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{host} is on this device or a private network"),
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
    body: Option<&str>,
) -> Result<(ureq::Response, String), String> {
    let mut url = url::Url::parse(url).map_err(|e| format!("not a link: {e}"))?;
    let mut method = method.to_string();
    let mut body = body;
    for _ in 0..10 {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("{}: only http and https", url.scheme()));
        }
        let host = url.host_str().unwrap_or("");
        if !hosts.allows(host) {
            return Err(format!("{host} is not in the addon's hosts"));
        }
        let mut req = agent.request(&method, url.as_str());
        for (name, value) in headers {
            req = req.set(name, value);
        }
        let sent = match body {
            Some(text) => req.send_string(text),
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
        let hosts = Hosts(vec!["archive.org".into(), "*.archive.org".into()]);
        assert!(hosts.allows("archive.org"));
        assert!(hosts.allows("ia800.us.archive.org"));
        assert!(!hosts.allows("evilarchive.org"));
        assert!(!hosts.allows("example.com"));
        let any = Hosts(vec!["*".into()]);
        assert!(any.allows("example.com"));
        assert!(!any.allows("127.0.0.1"));
        assert!(!any.allows("localhost"));
        assert!(!any.allows("[::1]"));
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

    #[test]
    fn resolver_refuses_this_device_even_when_named() {
        let agent = sandboxed_agent(&Hosts(vec!["127.0.0.1".into()]));
        let err = request(&agent, &Hosts(vec!["127.0.0.1".into()]), "GET", "http://127.0.0.1:9/", &[], None).unwrap_err();
        assert!(err.contains("this device or a private network"), "{err}");
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
        let err = request(&agent, &Hosts(vec!["127.0.0.1".into()]), "GET", &format!("http://127.0.0.1:{port}/"), &[], None).unwrap_err();
        assert_eq!(err, "evil.example is not in the addon's hosts");
    }
}
