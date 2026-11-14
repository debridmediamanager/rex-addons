//! `rex-addons check <addon folder>`: an addon tried once, without rex. Its manifest is parsed and
//! what installing checks is checked, `items()` runs once and, when asked, `open()` once, each step
//! told as it goes. Any step that fails makes the exit status 1.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Map, Value as Json};

use crate::addons::{read_json, Manifest};
use crate::app::{self, App};
use crate::sandbox::{self, Storage};

pub const USAGE: &str = "rex-addons check <addon folder> [--config '<json>'] [--since <unix seconds>] [--open <item id>] [--ffprobe <path>] [--data <dir>]

  --config   settings, as the app would PUT them; the rest take their defaults
  --since    a sync that may answer with changes only: ctx.full false, ctx.last_sync this time
  --open     an item id: open() runs for it too, and what it returned is checked against the hosts
  --ffprobe  the ffprobe behind the addon's ffprobe(); without it ffprobe() fails
  --data     a rex-addons data directory: the addon's storage there is used (its sign-in's tokens,
             say), as are the addons it calls; without it, storage starts empty and is thrown away";

/// Runs the check; the exit status.
pub fn run(args: &[String]) -> i32 {
    let (mut folder, mut config, mut open, mut ffprobe, mut data, mut since) = (None, None, None, None, None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "--config" => &mut config,
            "--open" => &mut open,
            "--ffprobe" => &mut ffprobe,
            "--data" => &mut data,
            "--since" => &mut since,
            _ if folder.is_none() && !arg.starts_with('-') => {
                folder = Some(PathBuf::from(arg));
                continue;
            }
            _ => {
                eprintln!("{arg}: unknown\n\n{USAGE}");
                return 2;
            }
        };
        match args.next() {
            Some(value) => *slot = Some(value.clone()),
            None => {
                eprintln!("{arg} needs a value\n\n{USAGE}");
                return 2;
            }
        }
    }
    let Some(folder) = folder else {
        eprintln!("{USAGE}");
        return 2;
    };
    let since = match since.map(|s| s.parse::<u64>()) {
        None => None,
        Some(Ok(since)) => Some(since),
        Some(Err(_)) => {
            eprintln!("--since takes unix seconds\n\n{USAGE}");
            return 2;
        }
    };
    let temp = std::env::temp_dir().join(format!("rex-addons-check-{}", std::process::id()));
    let failed = check(&folder, config.as_deref(), open.as_deref(), ffprobe, data.as_deref().map(Path::new), since, &temp);
    let _ = std::fs::remove_dir_all(&temp);
    i32::from(failed)
}

fn ok(text: &str) {
    println!("ok    {text}");
}

fn note(text: &str) {
    println!("note  {text}");
}

fn fail(text: &str) -> bool {
    println!("FAIL  {text}");
    true
}

/// Whether any step failed. `temp` is thrown away afterwards.
fn check(folder: &Path, config: Option<&str>, open: Option<&str>, ffprobe: Option<String>, data: Option<&Path>, since: Option<u64>, temp: &Path) -> bool {
    let Some(raw) = read_json(&folder.join("manifest.json")) else {
        return fail(&format!("{}: missing, or not JSON", folder.join("manifest.json").display()));
    };
    let manifest = match Manifest::parse(&raw).and_then(|m| m.check_host().map(|_| m)) {
        Ok(manifest) => manifest,
        Err(error) => return fail(&error),
    };
    let needs = manifest.host.as_ref().map_or(String::new(), |host| format!(", needs rex-addons {host}"));
    ok(&format!("manifest: {} {}{needs}", manifest.id, manifest.version));
    let source = match std::fs::read_to_string(folder.join("main.js")) {
        Ok(source) => source,
        Err(error) => return fail(&format!("main.js: {error}")),
    };
    let config = match config.map(serde_json::from_str::<Json>) {
        None => Map::new(),
        Some(Ok(patch)) => match manifest.apply(&Map::new(), &patch) {
            Ok(config) => config,
            Err(error) => return fail(&format!("--config: {error}")),
        },
        Some(Err(error)) => return fail(&format!("--config is not JSON: {error}")),
    };
    // A listener of its own for the relay an addon's ffprobe() reads; its API takes a token nobody has.
    let server = tiny_http::Server::http("127.0.0.1:0").expect("cannot listen on 127.0.0.1");
    let listen = server.server_addr().to_ip().unwrap();
    let app = App::new(data.unwrap_or(temp).into(), "http://127.0.0.1:9".into(), listen, ffprobe, None);
    let served = app.clone();
    std::thread::spawn(move || crate::server::serve(served, server, Some(crate::probe::token())));

    match app.loads(&manifest, &source) {
        Ok(names) => ok(&format!("main.js loads and exports {}", names.iter().map(|n| format!("{n}()")).collect::<Vec<_>>().join(", "))),
        Err(error) => return fail(&error),
    }
    let id = manifest.id.clone();
    if manifest.sign_in.is_some() {
        note("it signs in (sign_in), which check can't do: items() and open() run with what its storage holds, so give --data a rex-addons data directory where it is signed in");
    }
    if !manifest.requires.is_empty() {
        note(&format!("addons.call reaches {} only as installed in --data's rex-addons data directory", manifest.requires.join(", ")));
    }
    let missing = manifest.missing(&config);
    if !missing.is_empty() {
        for label in missing {
            fail(&format!("set {label} first: a required setting, given with --config"));
        }
        return true;
    }
    app::mask_secrets(&id, manifest.secrets(&config));
    // The installed addon's storage when --data has one, else a throwaway one.
    let installed = data.map(|data| data.join("addons").join(&id)).filter(|dir| dir.join("manifest.json").exists());
    let storage = Arc::new(Storage::open(match installed {
        Some(dir) => dir.join("storage.json"),
        None => {
            let _ = std::fs::create_dir_all(temp);
            temp.join("storage.json")
        }
    }));
    let env = app.env(&id, &manifest, &config, &source, storage, true);
    let shown = |value: &Json| app::masked(&id, &serde_json::to_string_pretty(value).unwrap());

    let mut failed = false;
    let mut ctx = app::call_ctx(&manifest, &config);
    ctx["reason"] = json!("manual");
    ctx["full"] = json!(since.is_none());
    ctx["last_sync"] = json!(since);
    let started = Instant::now();
    let (mut titles, mut unprobed, mut removed, mut first) = (0, 0, Vec::new(), None);
    let mut seen = app::Seen::default();
    let paged = app::pages(&env, ctx, &manifest, since.is_none(), &BTreeSet::new(), &mut seen, |page, _| {
        for why in &page.left_out {
            failed = fail(&app::masked(&id, why));
        }
        titles += page.items.len();
        unprobed += page.items.iter().filter(|item| !item.get("media_info").is_some_and(Json::is_object)).count();
        removed.extend(page.removed);
        first = first.take().or_else(|| page.items.into_iter().next());
        Ok(())
    });
    match paged {
        Err(error) => failed = fail(&format!("items(): {}", app::masked(&id, &error))),
        Ok(()) => {
            let count = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
            let what = match seen.partial {
                Some(true) => format!("changes only: {}, {} removed", count(titles, "title", "titles"), removed.len()),
                _ => count(titles, "title", "titles"),
            };
            let pages = if seen.pages > 1 { format!(" over {} pages", seen.pages) } else { String::new() };
            ok(&format!("items(): {what}{pages} in {:.1} s", started.elapsed().as_secs_f64()));
            if unprobed > 0 {
                note(&format!("{} without media_info: rex-addons probes each through open() when it syncs (with --ffprobe), and rex holds it until then", count(unprobed, "title", "titles")));
            }
            if !removed.is_empty() {
                note(&format!("removed: {}", removed.iter().take(5).cloned().collect::<Vec<_>>().join(", ")));
            }
            if let Some(first) = first {
                println!("first title:\n{}", shown(&Json::Object(first)));
            }
        }
    }
    if let Some(item) = open {
        match sandbox::call(&env, "open", &[json!(item), app::call_ctx(&manifest, &config)], sandbox::OPEN) {
            Err(error) => failed = fail(&format!("open({item:?}): {}", app::masked(&id, &error))),
            Ok(answer) => {
                println!("open({item:?}) returned:\n{}", shown(&answer));
                match app::opened_from(&answer, &manifest, &env.hosts) {
                    Ok(opened) if opened.playlists.is_some() => ok("open(): playlists, every link in them on the addon's hosts"),
                    Ok(_) => ok("open(): a link on the addon's hosts"),
                    Err(error) => failed = fail(&app::masked(&id, &error)),
                }
            }
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An addon folder of its own, under the test's temp directory.
    fn folder(name: &str, manifest: Json, main: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rex-addons-check-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(dir.join("main.js"), main).unwrap();
        dir
    }

    fn checked(dir: &Path, extra: &[&str]) -> i32 {
        let mut args = vec![dir.display().to_string()];
        args.extend(extra.iter().map(|a| a.to_string()));
        run(&args)
    }

    #[test]
    fn check_fails_on_each_kind_of_problem() {
        let manifest = json!({"id": "films", "name": "Films", "version": "1.0.0", "hosts": ["cdn.example.com"],
            "config": [{"key": "key", "label": "API key", "type": "secret", "required": true}]});
        let good = "export default { items(ctx) { return [{id: 'a', path: 'films/a.mp4', display: {title: ctx.config.key}}]; },
            open(id) { return { url: id === 'off' ? 'https://elsewhere.example/a.mp4' : 'https://cdn.example.com/a.mp4' }; } };";
        let dir = folder("good", manifest.clone(), good);
        assert_eq!(checked(&dir, &["--config", r#"{"key": "secret-value"}"#, "--open", "a"]), 0);
        // A required setting not given; settings that don't fit; an open() off the hosts.
        assert_eq!(checked(&dir, &[]), 1);
        assert_eq!(checked(&dir, &["--config", r#"{"key": 5}"#]), 1);
        assert_eq!(checked(&dir, &["--config", r#"{"key": "secret-value"}"#, "--open", "off"]), 1);
        // An entry left out, a throw, an export missing, a manifest refused, a host too new.
        let left_out = folder("left-out", manifest.clone(), "export default { items() { return [{id: 'a', path: '/abs.mp4'}]; }, open() {} };");
        assert_eq!(checked(&left_out, &["--config", r#"{"key": "secret-value"}"#]), 1);
        let throws = folder("throws", manifest.clone(), "export default { items() { throw new Error('down'); }, open() {} };");
        assert_eq!(checked(&throws, &["--config", r#"{"key": "secret-value"}"#]), 1);
        assert_eq!(checked(&folder("no-open", manifest.clone(), "export default { items() { return []; } };"), &[]), 1);
        assert_eq!(checked(&folder("bad-manifest", json!({"id": "Films"}), good), &[]), 1);
        assert_eq!(checked(&folder("too-new", json!({"id": "a", "name": "A", "version": "1", "host": "99"}), good), &[]), 1);
        // Pages, and changes only with --since; partial without it fails, as in a sync that needs every title.
        let paged = json!({"id": "paged", "name": "Paged", "version": "1.0.0", "host": "0.8"});
        let pages = folder("paged", paged.clone(), "export default { items(ctx) {
            const at = ctx.cursor ?? 0;
            if (!ctx.full) return {partial: true, items: [{id: 'n', path: 'f/n.mp4'}], removed: ['a0'], ...(at < 1 ? {next: at + 1} : {})};
            return {items: [{id: 'a' + at, path: 'f/' + at + '.mp4'}], ...(at < 2 ? {next: at + 1} : {})}; }, open() {} };");
        assert_eq!(checked(&pages, &[]), 0);
        assert_eq!(checked(&pages, &["--since", "1794489288"]), 1, "the same id on two pages is left out");
        let changes = folder("changes", paged, "export default { items(ctx) { return ctx.full ? [] : {partial: true, items: [], removed: ['a']}; }, open() {} };");
        assert_eq!(checked(&changes, &["--since", "1794489288"]), 0);
        assert_eq!(checked(&changes, &["--since", "yesterday"]), 2);
        // Asked wrongly: no folder, a flag without its value, one it doesn't know.
        assert_eq!(run(&[]), 2);
        assert_eq!(checked(&dir, &["--open"]), 2);
        assert_eq!(checked(&dir, &["--force"]), 2);
        for name in ["good", "left-out", "throws", "no-open", "bad-manifest", "too-new", "paged", "changes"] {
            let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("rex-addons-check-test-{}-{name}", std::process::id())));
        }
    }
}
