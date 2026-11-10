//! rex-addons: runs JavaScript addons in QuickJS sandboxes and feeds what they return to rex,
//! each addon one source of rex's feed API.
#[cfg(test)]
mod addon_tests;
mod addons;
mod app;
mod git;
mod net;
mod probe;
mod repos;
mod sandbox;
mod server;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "rex-addons --listen 127.0.0.1:32410 --rex http://127.0.0.1:32400 --data <dir> [--ffprobe <path>] [--bundled <dir>] [--default-repo <url>]...

  --listen   where the management API and the byte paths rex reads answer (default 127.0.0.1:32410)
  --rex      rex's address (default http://127.0.0.1:32400)
  --data     where installed addons, their settings and storage are kept (default ./rex-addons-data)
  --ffprobe  the ffprobe that checks each item once; without it rex holds every item unshown
  --bundled  addons installed on the first start and updated when a newer version is bundled
  --default-repo  an addon repository added once, the first time it is given; repeatable

  REX_ADDONS_TOKEN  when set, every /api/ request must carry Authorization: Bearer <it>";

fn main() {
    let mut listen: SocketAddr = "127.0.0.1:32410".parse().unwrap();
    let mut rex = "http://127.0.0.1:32400".to_string();
    let mut data = PathBuf::from("rex-addons-data");
    let (mut ffprobe, mut bundled, mut defaults) = (None, None, Vec::new());
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if matches!(flag.as_str(), "-h" | "--help") {
            println!("{USAGE}");
            return;
        }
        if flag == "--version" {
            println!("rex-addons {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        let Some(value) = args.next() else {
            eprintln!("{flag} needs a value\n\n{USAGE}");
            std::process::exit(2);
        };
        match flag.as_str() {
            "--listen" => {
                listen = value.parse().unwrap_or_else(|_| {
                    eprintln!("--listen {value}: give an address and port, such as 127.0.0.1:32410");
                    std::process::exit(2)
                })
            }
            "--rex" => rex = value,
            "--data" => data = value.into(),
            "--ffprobe" => ffprobe = Some(value),
            "--bundled" => bundled = Some(PathBuf::from(value)),
            "--default-repo" => defaults.push(value),
            _ => {
                eprintln!("unknown flag {flag}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    if !rex.starts_with("http://") && !rex.starts_with("https://") {
        eprintln!("--rex {rex}: give rex's http(s) address");
        std::process::exit(2);
    }
    let server = tiny_http::Server::http(listen).unwrap_or_else(|e| {
        eprintln!("cannot listen on {listen}: {e}");
        std::process::exit(1)
    });
    let app = app::App::new(data, rex, listen, ffprobe, bundled.as_deref());
    app::log("host", &format!("rex-addons {} on {listen}, rex at {}, {} addons", env!("CARGO_PKG_VERSION"), app.rex, app.addons.lock().unwrap().len()));
    app.start_repos(&defaults);
    if app.prober.bin.is_none() {
        app::warn("host", "no --ffprobe: rex holds every item it is sent until it is probed, so nothing shows in Plex");
    }
    // An environment variable, never a flag: other programs on the device can read flags.
    let token = std::env::var("REX_ADDONS_TOKEN").ok().filter(|t| !t.is_empty());
    if token.is_none() {
        app::warn("host", "no REX_ADDONS_TOKEN: the management API is open to every program on this device");
    }
    let scheduler = app.clone();
    std::thread::spawn(move || loop {
        let due = scheduler.due();
        // rex may start after this host: nothing is tried, and nothing counts as failed, until it answers.
        if !due.is_empty() && scheduler.rex_reachable() {
            for id in due {
                scheduler.start_sync(&id, false);
            }
        }
        std::thread::sleep(Duration::from_secs(10));
    });
    server::serve(app, server, token);
}
