//! One addon calling another's exports (`requires`, `exports`, `addons.call`), driven through the
//! API and real sandboxed addons.
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::addons::read_json;
use crate::app::App;

struct Rig {
    app: Arc<App>,
}

fn rex() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let _ = request.respond(tiny_http::Response::from_string("{}"));
        }
    });
    url
}

fn rig(name: &str) -> Rig {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let api: SocketAddr = server.server_addr().to_ip().unwrap();
    let data = std::env::temp_dir().join(format!("rex-addons-calls-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let app = App::new(data, rex(), api, None, None);
    let served = app.clone();
    std::thread::spawn(move || crate::server::serve(served, server, None));
    Rig { app }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.app.data);
    }
}

impl Rig {
    fn status(&self, id: &str) -> Json {
        self.app.status(id).unwrap()
    }

    fn until(&self, id: &str, what: &str, done: impl Fn(&Json) -> bool) -> Json {
        for _ in 0..400 {
            let status = self.status(id);
            if done(&status) {
                return status;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("{what} never happened: {}", self.status(id));
    }
}

/// An addon others may call: `find` remembers what it was asked in its own storage and answers
/// a link on its own host; `fail` and `revoked` throw.
const CALLEE: &str = r#"
export default {
  items() { return []; },
  open() { return { url: "https://tube.example/x" }; },
  signIn() {}, signInPoll() {},
  async find(query, ctx) {
    storage.set("asked", query);
    return { url: `https://tube.example/${query}.m4a`, quality: ctx.config.quality };
  },
  fail(query) { throw new Error(`no match for ${query}`); },
  revoked() { throw new Error("SIGN_IN_REQUIRED: the token was revoked"); },
  async onward() { return await addons.call("caller", "find", "x"); },
  secret() { return "not exported"; },
};
"#;

/// Plays what the callee finds, and returns what any call answers or throws.
const CALLER: &str = r#"
export default {
  items() { return []; },
  async open(id) {
    const [name, query] = id.split(":");
    try {
      const found = await addons.call("tube", name, query);
      return { url: found.url, headers: { "X-Quality": String(found.quality) } };
    } catch (error) {
      return { url: "https://tube.example/error?" + encodeURIComponent(error.message) };
    }
  },
  find() { return {}; },
};
"#;

fn callee_manifest(extra: Json) -> Json {
    let mut manifest = json!({"id": "tube", "name": "Tube", "version": "1.0.0", "host": "0.5", "hosts": ["tube.example"],
        "sign_in": {}, "exports": ["find", "fail", "revoked", "onward"],
        "config": [{"key": "quality", "label": "Quality", "type": "number", "default": 1080}]});
    for (key, value) in extra.as_object().unwrap() {
        manifest[key] = value.clone();
    }
    manifest
}

fn calls_rig(name: &str) -> Rig {
    let rig = rig(name);
    let caller = json!({"id": "caller", "name": "Caller", "version": "1.0.0", "host": "0.5", "hosts": ["tube.example"],
        "requires": ["tube"], "exports": ["find"]});
    rig.app.install(&caller, CALLER, None).unwrap();
    rig
}

/// What the caller's open() of `name:query` says: the link it got, or the error it caught.
fn ask(rig: &Rig, name: &str, query: &str) -> String {
    let item = format!("{name}:{query}");
    rig.app.forget_open("caller", &item);
    let url = rig.app.open("caller", &item).unwrap().url;
    match url.strip_prefix("https://tube.example/error?") {
        Some(error) => format!("error: {}", percent_encoding::percent_decode_str(error).decode_utf8_lossy()),
        None => url,
    }
}

fn signed_in(rig: &Rig, id: &str, account: Option<&str>) {
    rig.app.addons.lock().unwrap().get_mut(id).unwrap().account = account.map(str::to_string);
}

#[test]
fn an_addon_calls_anothers_export_in_the_others_sandbox() {
    let rig = calls_rig("calls");
    rig.app.install(&callee_manifest(json!({})), CALLEE, None).unwrap();
    signed_in(&rig, "tube", Some("someone"));
    let opened = rig.app.open("caller", "find:song").unwrap();
    // The callee's own settings, and its answer passed through the caller's open().
    assert_eq!(opened.url, "https://tube.example/song.m4a");
    assert_eq!(opened.headers, [("X-Quality".to_string(), "1080".to_string())]);
    // It wrote its own storage, not the caller's.
    assert_eq!(read_json(&rig.app.data.join("addons/tube/storage.json")).unwrap()["asked"], "song");
    assert!(read_json(&rig.app.data.join("addons/caller/storage.json")).is_none_or(|s| s.get("asked").is_none()));
    assert_eq!(rig.status("caller")["requires"], json!([{"id": "tube", "ok": true, "reason": null}]));
    assert_eq!(rig.status("tube")["requires"], json!([]));
}

#[test]
fn unmet_requires_are_shown_and_explained() {
    let rig = calls_rig("unmet");
    let requires = |rig: &Rig| rig.status("caller")["requires"].clone();
    assert_eq!(requires(&rig), json!([{"id": "tube", "ok": false, "reason": "not installed"}]));
    assert_eq!(ask(&rig, "find", "a"), "error: the addon tube is not installed: install it and sign in to it first");

    rig.app.install(&callee_manifest(json!({})), CALLEE, None).unwrap();
    assert_eq!(requires(&rig), json!([{"id": "tube", "ok": false, "reason": "sign in needed"}]));
    assert_eq!(ask(&rig, "find", "a"), "error: sign in to Tube first");

    signed_in(&rig, "tube", Some("someone"));
    rig.app.set_enabled("tube", false).unwrap();
    assert_eq!(requires(&rig), json!([{"id": "tube", "ok": false, "reason": "disabled"}]));
    assert_eq!(ask(&rig, "find", "a"), "error: Tube is disabled: enable it first");

    rig.app.set_enabled("tube", true).unwrap();
    assert_eq!(requires(&rig), json!([{"id": "tube", "ok": true, "reason": null}]));
    // Only what it exports, and only for addons that require it.
    assert_eq!(ask(&rig, "secret", "a"), "error: Tube exports no secret()");
    assert_eq!(rig.app.call_export("tube", "caller", "find", vec![]).unwrap_err(), "caller is not in this addon's requires");
}

#[test]
fn an_addon_syncs_as_soon_as_the_one_it_requires_can_be_called() {
    let rig = rig("wake");
    let caller = json!({"id": "caller", "name": "Caller", "version": "1.0.0", "host": "0.5", "hosts": ["tube.example"], "requires": ["tube"]});
    let source = r#"export default { async items() { await addons.call("tube", "find", "a"); return []; }, open() {} };"#;
    rig.app.install(&caller, source, None).unwrap();
    rig.app.install(&callee_manifest(json!({})), CALLEE, None).unwrap();
    signed_in(&rig, "tube", Some("someone"));
    rig.app.set_enabled("tube", false).unwrap();
    rig.app.start_sync("caller", false, "manual");
    let failed = rig.until("caller", "the failed sync", |s| s["state"] == "error");
    // The reason alone: the stack of the throw stays in the log.
    assert_eq!(failed["error"], "Tube is disabled: enable it first");

    // Nobody asks the caller to sync again: enabling what it requires does.
    rig.app.set_enabled("tube", true).unwrap();
    let synced = rig.until("caller", "the sync after tube came back", |s| s["state"] == "idle" && s["last_sync"].is_number());
    assert_eq!(synced["error"], Json::Null);
}

#[test]
fn errors_propagate_and_a_callee_signed_out_leaves_the_caller_signed_in() {
    let rig = calls_rig("errors");
    rig.app.install(&callee_manifest(json!({})), CALLEE, None).unwrap();
    signed_in(&rig, "tube", Some("someone"));
    assert_eq!(ask(&rig, "fail", "x"), "error: Tube: no match for x");

    assert_eq!(ask(&rig, "revoked", "x"), "error: sign in to Tube again: the token was revoked");
    let tube = rig.status("tube");
    assert_eq!((&tube["sign_in"]["account"], tube["sign_in"]["error"].as_str()), (&Json::Null, Some("the token was revoked")));
    assert_eq!(rig.status("caller")["requires"][0]["reason"], "sign in needed");

    // A caller that signs in itself stays signed in when the error comes through uncaught.
    let rethrows = "export default { items() { return []; }, signIn() {}, signInPoll() {},
        async open() { return await addons.call('tube', 'revoked'); } };";
    let manifest = json!({"id": "rethrows", "name": "Rethrows", "version": "1.0.0", "host": "0.5", "sign_in": {}, "requires": ["tube"]});
    rig.app.install(&manifest, rethrows, None).unwrap();
    signed_in(&rig, "rethrows", Some("someone"));
    signed_in(&rig, "tube", Some("someone"));
    let error = rig.app.open("rethrows", "x").err().unwrap();
    assert!(error.starts_with("sign in to Tube again: the token was revoked"), "{error}");
    assert_eq!(rig.status("rethrows")["sign_in"]["account"], "someone");
}

#[test]
fn a_called_addon_cannot_call_a_third() {
    let rig = calls_rig("depth");
    rig.app.install(&callee_manifest(json!({"requires": ["caller"]})), CALLEE, None).unwrap();
    signed_in(&rig, "tube", Some("someone"));
    assert_eq!(ask(&rig, "onward", "x"), "error: Tube: addons.call: an addon called by another cannot call a third");
}

#[test]
fn installing_checks_exports_exist() {
    let rig = rig("exports");
    let manifest = callee_manifest(json!({"exports": ["find", "missing"]}));
    assert_eq!(rig.app.install(&manifest, CALLEE, None).unwrap_err(), "main.js: main.js exports no missing()");
}
