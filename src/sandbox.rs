//! One QuickJS runtime per call: the addon's `main.js` is loaded, one method of its default
//! export is called and its promise driven to the end, and the runtime is dropped. Nothing
//! survives a call but what the addon put in `storage`.
use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::function::Args;
use rquickjs::{Context, Ctx, Exception, Function, Module, Object, Runtime, TypedArray, Value};
use serde_json::{Map, Value as Json};

use crate::net::{self, Hosts};
use crate::probe::Prober;

/// How much a call may take. `cpu` counts the time JavaScript runs, not the time a `fetch`
/// waits on the network; `wall` bounds the whole call.
#[derive(Clone, Copy)]
pub struct Limits {
    pub memory: usize,
    pub cpu: Duration,
    pub wall: Duration,
}

pub const ITEMS: Limits = Limits { memory: 64 << 20, cpu: Duration::from_secs(30), wall: Duration::from_secs(20 * 60) };
pub const OPEN: Limits = Limits { memory: 64 << 20, cpu: Duration::from_secs(10), wall: Duration::from_secs(90) };

/// A fetched body is read up to this; more is an error the addon sees.
const MAX_BODY: u64 = 16 << 20;
/// Fetched responses whose body was not read yet, at most: each holds a connection open.
const UNREAD: usize = 32;
/// An addon's storage, all keys together, as JSON: room for a probe of each of 1,000 films.
const MAX_STORAGE: usize = 16 << 20;

/// An addon's `storage`: one JSON object, written to disk when a call ends.
pub struct Storage {
    path: PathBuf,
    state: Mutex<Stored>,
}

struct Stored {
    map: Map<String, Json>,
    size: usize,
    dirty: bool,
}

fn entry_size(key: &str, value: &Json) -> usize {
    key.len() + value.to_string().len() + 4
}

impl Storage {
    pub fn open(path: PathBuf) -> Storage {
        let map: Map<String, Json> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let size = map.iter().map(|(k, v)| entry_size(k, v)).sum();
        Storage { path, state: Mutex::new(Stored { map, size, dirty: false }) }
    }

    fn get(&self, key: &str) -> Option<Json> {
        self.state.lock().unwrap().map.get(key).cloned()
    }

    /// The keys starting with `prefix`, sorted.
    fn keys(&self, prefix: &str) -> Vec<String> {
        let mut keys: Vec<String> = self.state.lock().unwrap().map.keys().filter(|key| key.starts_with(prefix)).cloned().collect();
        keys.sort();
        keys
    }

    fn change(&self, key: &str, value: Option<Json>) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let before = state.map.get(key).map_or(0, |v| entry_size(key, v));
        let after = value.as_ref().map_or(0, |v| entry_size(key, v));
        if state.size - before + after > MAX_STORAGE {
            return Err(format!("storage is limited to {} MB", MAX_STORAGE >> 20));
        }
        state.size = state.size - before + after;
        match value {
            Some(value) => state.map.insert(key.to_string(), value),
            None => state.map.remove(key),
        };
        state.dirty = true;
        Ok(())
    }

    /// Writes what changed. Every call ends with it, so a call that sets thousands of keys
    /// writes the file once.
    pub fn flush(&self) {
        let mut state = self.state.lock().unwrap();
        if state.dirty {
            let text = serde_json::to_vec(&state.map).unwrap();
            if let Err(error) = crate::addons::write_atomic(&self.path, &text) {
                eprintln!("WARN [host] {}: {error}", self.path.display());
            }
            state.dirty = false;
        }
    }
}

pub struct Env<'a> {
    pub addon: &'a str,
    pub source: &'a str,
    pub hosts: &'a Hosts,
    pub storage: Arc<Storage>,
    pub prober: Arc<Prober>,
    /// The `spotify` global, for an addon that signs in with Spotify: (`GET`, `POST` or `USER`,
    /// path, [body]) to JSON.
    pub spotify: Option<Bridge>,
    /// `addons.call`, for an addon with `requires`: (addon, function, arguments) to its result.
    pub calls: Option<Bridge>,
}

/// A host function behind a global only some addons get. Its time counts as waiting.
pub type Bridge = Arc<dyn Fn(&str, &str, Vec<Json>) -> Result<Json, String> + Send + Sync>;

const PRELUDE: &str = r#"
(() => {
  const host = globalThis.__rex;
  delete globalThis.__rex;
  // QuickJS-NG's web extras: an addon gets plain ECMAScript, atob and btoa, and the globals below only.
  for (const name of ["queueMicrotask", "performance", "DOMException"]) delete globalThis[name];
  const show = (args) => args.map((arg) => {
    if (typeof arg === "string") return arg;
    if (arg instanceof Error) return arg.stack ? `${arg}\n${arg.stack}` : String(arg);
    try { return JSON.stringify(arg) ?? String(arg); } catch { return String(arg); }
  }).join(" ");
  const console = Object.freeze({
    log: (...args) => host.log("info", show(args)),
    info: (...args) => host.log("info", show(args)),
    debug: (...args) => host.log("info", show(args)),
    warn: (...args) => host.log("warn", show(args)),
    error: (...args) => host.log("error", show(args)),
  });
  async function fetch(url, init = {}) {
    let body = init.body;
    if (body instanceof ArrayBuffer) body = new Uint8Array(body);
    else if (ArrayBuffer.isView(body)) body = new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
    const binary = body instanceof Uint8Array;
    const answer = JSON.parse(host.fetch(String(url), JSON.stringify({
      method: init.method, headers: init.headers, body: binary ? undefined : body,
    }), binary ? body : null));
    let used = false;
    const read = (bytes) => {
      if (used) throw new TypeError("the body was already read");
      used = true;
      return host.body(answer.handle, bytes);
    };
    return Object.freeze({
      status: answer.status,
      ok: answer.status >= 200 && answer.status < 300,
      headers: Object.freeze(answer.headers),
      url: answer.url,
      text: async () => read(false),
      json: async () => JSON.parse(read(false)),
      bytes: async () => read(true),
    });
  }
  async function sleep(ms) {
    host.sleep(Number(ms));
  }
  async function ffprobe(target, init = {}) {
    // A link, or playlists the addon wrote ({playlist, playlists}, as open() returns them).
    const own = typeof target === "object" && target !== null && !(target instanceof URL);
    return JSON.parse(host.ffprobe(own ? "" : String(target), JSON.stringify({ headers: init.headers }), own ? JSON.stringify(target) : null));
  }
  // URL and URLSearchParams, small: the url crate parses and resolves, each URL keeps its parts
  // and each URLSearchParams its list of pairs, and a URL's params write back to its search.
  const parts = new WeakMap(), lists = new WeakMap(), paramsOf = new WeakMap(), owners = new WeakMap();
  const parse = (link, base = null, field = null, value = null) => JSON.parse(host.url(link, base, field, value));
  const decode = (text) => { text = text.replace(/\+/g, " "); try { return decodeURIComponent(text); } catch { return text; } };
  const encode = (text) => encodeURIComponent(text).replace(/[!'()~]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase()).replace(/%20/g, "+");
  const pairs = (query) => query.replace(/^\?/, "").split("&").filter(Boolean).map((pair) => {
    const at = pair.indexOf("=");
    return at < 0 ? [decode(pair), ""] : [decode(pair.slice(0, at)), decode(pair.slice(at + 1))];
  });
  const changed = (params) => {
    const url = owners.get(params);
    if (url) parts.set(url, parse(url.href, null, "search", params.toString()));
  };
  class URLSearchParams {
    constructor(init = "") {
      lists.set(this, typeof init === "object" && init !== null
        ? (Symbol.iterator in init ? [...init] : Object.entries(init)).map(([name, value]) => [String(name), String(value)])
        : pairs(String(init)));
    }
    get(name) { return lists.get(this).find(([n]) => n === String(name))?.[1] ?? null; }
    getAll(name) { return lists.get(this).filter(([n]) => n === String(name)).map(([, v]) => v); }
    has(name) { return lists.get(this).some(([n]) => n === String(name)); }
    append(name, value) { lists.get(this).push([String(name), String(value)]); changed(this); }
    delete(name) { lists.set(this, lists.get(this).filter(([n]) => n !== String(name))); changed(this); }
    set(name, value) {
      name = String(name);
      const list = lists.get(this), at = list.findIndex(([n]) => n === name);
      if (at < 0) list.push([name, String(value)]);
      else { list[at] = [name, String(value)]; lists.set(this, list.filter(([n], i) => n !== name || i === at)); }
      changed(this);
    }
    toString() { return lists.get(this).map(([n, v]) => `${encode(n)}=${encode(v)}`).join("&"); }
    entries() { return lists.get(this).map(([n, v]) => [n, v])[Symbol.iterator](); }
    keys() { return lists.get(this).map(([n]) => n)[Symbol.iterator](); }
    values() { return lists.get(this).map(([, v]) => v)[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
  }
  class URL {
    constructor(link, base) { parts.set(this, parse(String(link), base === undefined ? null : String(base))); }
    get searchParams() {
      if (!paramsOf.has(this)) {
        const params = new URLSearchParams(this.search);
        paramsOf.set(this, params);
        owners.set(params, this);
      }
      return paramsOf.get(this);
    }
    toString() { return this.href; }
    toJSON() { return this.href; }
  }
  const hash = (algorithm, text) => host.hash(String(algorithm), "", String(text), false);
  const hmac = (algorithm, key, text) => host.hash(String(algorithm), String(key), String(text), true);
  for (const name of ["href", "origin", "protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"]) {
    Object.defineProperty(URL.prototype, name, {
      get() { return parts.get(this)[name]; },
      set: name === "origin" ? undefined : function (value) {
        parts.set(this, parse(this.href, null, name, String(value)));
        if (paramsOf.has(this)) lists.set(paramsOf.get(this), pairs(this.search));
      },
      configurable: true,
    });
  }
  const storage = Object.freeze({
    get: (key) => { const value = host.storageGet(String(key)); return value === undefined ? undefined : JSON.parse(value); },
    set: (key, value) => { value === undefined ? host.storageDelete(String(key)) : host.storageSet(String(key), JSON.stringify(value)); },
    delete: (key) => { host.storageDelete(String(key)); },
    keys: (prefix = "") => JSON.parse(host.storageKeys(String(prefix))),
  });
  const globals = { console, fetch, ffprobe, storage, sleep, URL, URLSearchParams, hash, hmac };
  if (host.spotify) {
    const ask = (method, path, body) => JSON.parse(host.spotify(method, String(path), body === undefined ? null : JSON.stringify(body)));
    globals.spotify = Object.freeze({
      user: () => ask("USER", "/"),
      get: async (path) => ask("GET", path),
      post: async (path, body = null) => ask("POST", path, body),
    });
  }
  if (host.call) {
    globals.addons = Object.freeze({
      call: async (id, name, ...args) => JSON.parse(host.call(String(id), String(name), JSON.stringify(args))),
    });
  }
  for (const [name, value] of Object.entries(globals)) {
    Object.defineProperty(globalThis, name, { value, writable: false, configurable: false, enumerable: false });
  }
})();
"#;

/// `default[method](...args)` in a fresh runtime, its promise driven to the end, as JSON. An
/// empty `method` only loads the module and checks it exports each function `args` names.
pub fn call(env: &Env, method: &str, args: &[Json], limits: Limits) -> Result<Json, String> {
    let runtime = Runtime::new().map_err(|e| e.to_string())?;
    runtime.set_memory_limit(limits.memory);
    // Calls run on the host's request threads, whose stacks are small.
    runtime.set_max_stack_size(512 << 10);
    let start = Instant::now();
    let waited = Rc::new(Cell::new(Duration::ZERO));
    let stopped = Rc::new(Cell::new(None::<&'static str>));
    {
        let (waited, stopped) = (waited.clone(), stopped.clone());
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let elapsed = start.elapsed();
            let reason = if elapsed > limits.wall {
                Some("wall")
            } else if elapsed.saturating_sub(waited.get()) > limits.cpu {
                Some("cpu")
            } else {
                None
            };
            stopped.set(stopped.get().or(reason));
            reason.is_some()
        })));
    }
    let context = Context::full(&runtime).map_err(|e| e.to_string())?;
    let result = context.with(|ctx| -> Result<Json, String> {
        let caught = |error: rquickjs::Error| describe(&ctx, error);
        install(&ctx, env, waited.clone(), start + limits.wall, limits.wall).map_err(caught)?;
        let module = Module::declare(ctx.clone(), "main.js", env.source).map_err(caught)?;
        let (module, loaded) = module.eval().map_err(caught)?;
        loaded.finish::<()>().map_err(caught)?;
        let exported: Value = module.get("default").map_err(caught)?;
        let Some(exported) = exported.into_object() else {
            return Err("main.js has no default export object".into());
        };
        if method.is_empty() {
            // Installing: the module loads and exports what the addon needs.
            for name in args.iter().filter_map(Json::as_str) {
                if !exported.get::<_, Value>(name).map_err(caught)?.is_function() {
                    return Err(format!("main.js exports no {name}()"));
                }
            }
            return Ok(Json::Null);
        }
        let function: Value = exported.get(method).map_err(caught)?;
        let Some(function) = function.into_function() else {
            return Err(format!("main.js exports no {method}()"));
        };
        let mut call_args = Args::new(ctx.clone(), args.len());
        call_args.this(exported).map_err(caught)?;
        for arg in args {
            call_args.push_arg(ctx.json_parse(arg.to_string()).map_err(caught)?).map_err(caught)?;
        }
        let mut returned: Value = function.call_arg(call_args).map_err(caught)?;
        if let Some(promise) = returned.as_promise() {
            returned = promise.finish::<Value>().map_err(|error| match error {
                rquickjs::Error::WouldBlock => format!("{method}() never finished: it waits on something that never comes (there are no timers)"),
                error => caught(error),
            })?;
        }
        match ctx.json_stringify(returned).map_err(caught)? {
            Some(text) => serde_json::from_str(&text.to_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string()),
            None => Ok(Json::Null),
        }
    });
    env.storage.flush();
    match stopped.get() {
        Some("wall") => Err(format!("{method}() took longer than {} s", limits.wall.as_secs())),
        Some(_) => Err(format!("{method}() ran JavaScript for longer than {} s", limits.cpu.as_secs())),
        None => result,
    }
}

/// An error out of QuickJS as text: the thrown value's message and stack.
fn describe(ctx: &Ctx, error: rquickjs::Error) -> String {
    if !matches!(error, rquickjs::Error::Exception) {
        return error.to_string();
    }
    let thrown = ctx.catch();
    if let Some(exception) = thrown.as_exception() {
        let message = exception.message().unwrap_or_default();
        return match exception.stack() {
            Some(stack) if !stack.trim().is_empty() => format!("{message}\n{}", stack.trim_end()),
            _ => message,
        };
    }
    match ctx.json_stringify(thrown.clone()) {
        Ok(Some(text)) => text.to_string().unwrap_or_default(),
        _ => format!("{thrown:?}"),
    }
}

fn throw(ctx: &Ctx, message: &str) -> rquickjs::Error {
    Exception::throw_type(ctx, message)
}

/// What a fetch or body read that failed at the call's deadline says, rather than ureq's timeout.
fn out_of_time(deadline: Instant, wall: Duration, what: &str, error: String) -> String {
    if Instant::now() >= deadline {
        format!("{what}: no time left, a call may take {} s in all", wall.as_secs())
    } else {
        format!("{what}: {error}")
    }
}

fn install<'js>(ctx: &Ctx<'js>, env: &Env, waited: Rc<Cell<Duration>>, deadline: Instant, wall: Duration) -> rquickjs::Result<()> {
    let host = Object::new(ctx.clone())?;
    let addon = env.addon.to_string();
    host.set(
        "log",
        Function::new(ctx.clone(), move |level: String, text: String| {
            let line = format!("[addon {addon}] {}", crate::app::masked(&addon, &text));
            match level.as_str() {
                "info" => crate::app::emit(&line, false),
                _ => crate::app::emit(&format!("{} {line}", level.to_uppercase()), true),
            }
        })?,
    )?;

    let agent = net::sandboxed_agent(env.hosts);
    let hosts = env.hosts.clone();
    let bodies: Rc<RefCell<Vec<Option<ureq::Response>>>> = Rc::default();
    let (fetch_bodies, fetch_waited) = (bodies.clone(), waited.clone());
    host.set(
        "fetch",
        Function::new(ctx.clone(), move |ctx: Ctx<'js>, url: String, init: String, bytes: Option<TypedArray<'js, u8>>| -> rquickjs::Result<String> {
            let init: Json = serde_json::from_str(&init).unwrap_or_default();
            // A copy: no JavaScript runs while it's taken.
            let bytes = bytes.map(|bytes| unsafe { bytes.as_bytes() }.map(<[u8]>::to_vec).unwrap_or_default());
            let body = bytes.as_deref().or(init["body"].as_str().map(str::as_bytes));
            let method = init["method"].as_str().unwrap_or("GET").to_ascii_uppercase();
            if !matches!(method.as_str(), "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE") {
                return Err(throw(&ctx, &format!("fetch: method {method} is not allowed")));
            }
            let headers: Vec<(String, String)> = init["headers"]
                .as_object()
                .map(|headers| {
                    headers
                        .iter()
                        .map(|(name, value)| (name.clone(), value.as_str().map_or_else(|| value.to_string(), str::to_string)))
                        .collect()
                })
                .unwrap_or_default();
            let started = Instant::now();
            // The response's body is bounded by the same deadline.
            let sent = net::request_until(&agent, &hosts, &method, &url, &headers, body, Some(deadline));
            fetch_waited.set(fetch_waited.get() + started.elapsed());
            let (response, final_url) = sent.map_err(|error| throw(&ctx, &out_of_time(deadline, wall, "fetch", error)))?;
            let mut names = response.headers_names();
            names.dedup();
            let headers: Map<String, Json> = names
                .into_iter()
                .map(|name| {
                    let value = response.all(&name).join(", ");
                    (name.to_ascii_lowercase(), Json::String(value))
                })
                .collect();
            let status = response.status();
            let mut bodies = fetch_bodies.borrow_mut();
            // Each unread body holds a connection: past UNREAD, the oldest is closed.
            if bodies.iter().filter(|body| body.is_some()).count() >= UNREAD {
                if let Some(oldest) = bodies.iter_mut().find(|body| body.is_some()) {
                    *oldest = None;
                }
            }
            bodies.push(Some(response));
            Ok(serde_json::json!({"status": status, "headers": headers, "url": final_url, "handle": bodies.len() - 1}).to_string())
        })?,
    )?;
    host.set(
        "body",
        Function::new(ctx.clone(), {
            let waited = waited.clone();
            move |ctx: Ctx<'js>, handle: usize, binary: bool| -> rquickjs::Result<Value<'js>> {
            let response = bodies.borrow_mut().get_mut(handle).and_then(Option::take);
            let Some(response) = response else {
                return Err(throw(&ctx, &format!("the body was already read, or closed after {UNREAD} newer unread ones")));
            };
            let started = Instant::now();
            let mut bytes = Vec::new();
            let read = response.into_reader().take(MAX_BODY + 1).read_to_end(&mut bytes);
            waited.set(waited.get() + started.elapsed());
            read.map_err(|error| throw(&ctx, &out_of_time(deadline, wall, "reading the body", error.to_string())))?;
            if bytes.len() as u64 > MAX_BODY {
                return Err(throw(&ctx, &format!("the body is over {} MB", MAX_BODY >> 20)));
            }
            // Copied into QuickJS's own memory, so the call's memory limit counts it.
            match binary {
                true => Ok(TypedArray::<u8>::new_copy(ctx, bytes)?.into_value()),
                false => Ok(rquickjs::String::from_str(ctx, &String::from_utf8_lossy(&bytes))?.into_value()),
            }
        }})?,
    )?;

    host.set(
        "url",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, link: String, base: Option<String>, field: Option<String>, value: Option<String>| -> rquickjs::Result<String> {
                use url::quirks;
                let invalid = |link: &str| throw(&ctx, &format!("Invalid URL: {link}"));
                let mut url = match &base {
                    Some(base) => url::Url::parse(base).and_then(|base| base.join(&link)),
                    None => url::Url::parse(&link),
                }
                .map_err(|_| invalid(&link))?;
                // A setter: as in browsers, a value that doesn't fit leaves the URL as it was.
                if let (Some(field), Some(value)) = (field, value) {
                    match field.as_str() {
                        "href" => quirks::set_href(&mut url, &value).map_err(|_| invalid(&value))?,
                        "protocol" => quirks::set_protocol(&mut url, &value).unwrap_or(()),
                        "username" => quirks::set_username(&mut url, &value).unwrap_or(()),
                        "password" => quirks::set_password(&mut url, &value).unwrap_or(()),
                        "host" => quirks::set_host(&mut url, &value).unwrap_or(()),
                        "hostname" => quirks::set_hostname(&mut url, &value).unwrap_or(()),
                        "port" => quirks::set_port(&mut url, &value).unwrap_or(()),
                        "pathname" => quirks::set_pathname(&mut url, &value),
                        "search" => quirks::set_search(&mut url, &value),
                        "hash" => quirks::set_hash(&mut url, &value),
                        _ => (),
                    };
                }
                Ok(serde_json::json!({
                    "href": quirks::href(&url), "origin": quirks::origin(&url), "protocol": quirks::protocol(&url),
                    "username": quirks::username(&url), "password": quirks::password(&url), "host": quirks::host(&url),
                    "hostname": quirks::hostname(&url), "port": quirks::port(&url), "pathname": quirks::pathname(&url),
                    "search": quirks::search(&url), "hash": quirks::hash(&url),
                })
                .to_string())
            },
        )?,
    )?;
    host.set(
        "hash",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, algorithm: String, key: String, text: String, keyed: bool| -> rquickjs::Result<String> {
            use ring::{digest, hmac};
            // ring has no md5, so neither do addons.
            let (plain, keyed_with) = match algorithm.to_ascii_lowercase().replace('-', "").as_str() {
                "sha1" => (&digest::SHA1_FOR_LEGACY_USE_ONLY, hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY),
                "sha256" => (&digest::SHA256, hmac::HMAC_SHA256),
                "sha512" => (&digest::SHA512, hmac::HMAC_SHA512),
                _ => return Err(throw(&ctx, &format!("{}: {algorithm} is not sha1, sha256 or sha512", if keyed { "hmac" } else { "hash" }))),
            };
            Ok(match keyed {
                true => crate::git::hex(hmac::sign(&hmac::Key::new(keyed_with, key.as_bytes()), text.as_bytes()).as_ref()),
                false => crate::git::hex(digest::digest(plain, text.as_bytes()).as_ref()),
            })
        })?,
    )?;
    let sleep_waited = waited.clone();
    host.set(
        "sleep",
        Function::new(ctx.clone(), move |ctx: Ctx<'js>, ms: f64| -> rquickjs::Result<()> {
            let wanted = Duration::try_from_secs_f64(ms.max(0.0) / 1000.0).unwrap_or(Duration::MAX);
            if wanted > deadline.saturating_duration_since(Instant::now()) {
                return Err(throw(&ctx, &format!("sleep({ms}) would outlast the call: a call may take {} s in all", wall.as_secs())));
            }
            std::thread::sleep(wanted);
            sleep_waited.set(sleep_waited.get() + wanted);
            Ok(())
        })?,
    )?;
    let storage = env.storage.clone();
    host.set(
        "storageGet",
        Function::new(ctx.clone(), move |key: String| storage.get(&key).map(|value| value.to_string()))?,
    )?;
    let storage = env.storage.clone();
    host.set(
        "storageKeys",
        Function::new(ctx.clone(), move |prefix: String| Json::from(storage.keys(&prefix)).to_string())?,
    )?;
    let storage = env.storage.clone();
    host.set(
        "storageSet",
        Function::new(ctx.clone(), move |ctx: Ctx<'js>, key: String, value: String| -> rquickjs::Result<()> {
            let value = serde_json::from_str(&value).map_err(|e| throw(&ctx, &e.to_string()))?;
            storage.change(&key, Some(value)).map_err(|e| throw(&ctx, &e))
        })?,
    )?;
    let storage = env.storage.clone();
    host.set(
        "storageDelete",
        Function::new(ctx.clone(), move |ctx: Ctx<'js>, key: String| -> rquickjs::Result<()> {
            storage.change(&key, None).map_err(|e| throw(&ctx, &e))
        })?,
    )?;
    let (prober, hosts, probe_waited) = (env.prober.clone(), env.hosts.clone(), waited.clone());
    host.set(
        "ffprobe",
        Function::new(ctx.clone(), move |ctx: Ctx<'js>, url: String, init: String, playlists: Option<String>| -> rquickjs::Result<String> {
            let init: Json = serde_json::from_str(&init).unwrap_or_default();
            let headers = init["headers"]
                .as_object()
                .map(|h| h.iter().map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string))).collect())
                .unwrap_or_default();
            let started = Instant::now();
            let probed = match playlists {
                Some(text) => prober.probe_playlists(&serde_json::from_str(&text).unwrap_or_default(), &hosts),
                None => prober.probe(&url, headers, &hosts),
            };
            probe_waited.set(probe_waited.get() + started.elapsed());
            probed.map(|probe| probe.to_string()).map_err(|error| throw(&ctx, &format!("ffprobe: {error}")))
        })?,
    )?;
    for (name, bridge) in [("spotify", env.spotify.clone()), ("call", env.calls.clone())] {
        let Some(bridge) = bridge else { continue };
        let bridge_waited = waited.clone();
        host.set(
            name,
            Function::new(ctx.clone(), move |ctx: Ctx<'js>, what: String, path: String, rest: Option<String>| -> rquickjs::Result<String> {
                // A body (spotify) or the arguments (call), as JSON.
                let rest: Json = match rest {
                    Some(text) => serde_json::from_str(&text).map_err(|e| throw(&ctx, &e.to_string()))?,
                    None => Json::Null,
                };
                let args = match rest {
                    Json::Array(args) if name == "call" => args,
                    Json::Null => Vec::new(),
                    body => vec![body],
                };
                let started = Instant::now();
                let answer = bridge(&what, &path, args);
                bridge_waited.set(bridge_waited.get() + started.elapsed());
                answer.map(|value| value.to_string()).map_err(|error| throw(&ctx, &error))
            })?,
        )?;
    }
    ctx.globals().set("__rex", host)?;
    ctx.eval::<(), _>(PRELUDE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(source: &str, hosts: &[&str], limits: Limits) -> Result<Json, String> {
        let path = std::env::temp_dir().join(format!("rex-addons-storage-{}-{:?}.json", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_file(&path);
        let env = Env {
            addon: "test",
            source,
            hosts: &Hosts(hosts.iter().map(|h| h.to_string()).collect()),
            storage: Arc::new(Storage::open(path)),
            prober: Arc::new(Prober::new(None, "127.0.0.1:9".parse().unwrap())),
            spotify: None,
            calls: None,
        };
        call(&env, "items", &[serde_json::json!({"config": {"n": 2}})], limits)
    }

    const QUICK: Limits = Limits { memory: 16 << 20, cpu: Duration::from_secs(1), wall: Duration::from_secs(10) };

    #[test]
    fn returns_json_and_reads_config() {
        let got = run("export default { async items(ctx) { return [{id: 'a', n: ctx.config.n}]; }, open() {} };", &[], QUICK);
        assert_eq!(got.unwrap(), serde_json::json!([{"id": "a", "n": 2}]));
    }

    #[test]
    fn only_the_host_globals_beyond_ecmascript() {
        let got = run(
            "export default { items() { return ['setTimeout', 'setInterval', 'require', 'process', 'XMLHttpRequest', 'WebSocket', '__rex', 'os', 'std', 'print',
               'queueMicrotask', 'performance', 'DOMException', 'TextEncoder', 'TextDecoder', 'spotify', 'addons']
               .filter((name) => typeof globalThis[name] !== 'undefined')
               .concat([typeof fetch, typeof console.log, typeof storage.get, typeof ffprobe, typeof sleep, typeof atob, typeof btoa, typeof URL, typeof URLSearchParams, typeof hash, typeof hmac]); }, open() {} };",
            &[],
            QUICK,
        );
        assert_eq!(got.unwrap(), serde_json::json!(["function", "function", "function", "function", "function", "function", "function", "function", "function", "function", "function"]));
    }

    /// The bridges an addon gets only when its manifest asks: their answers come back as
    /// JSON, their errors as thrown errors, and their time is waiting, not JavaScript time.
    #[test]
    fn bridges_answer_json_throw_errors_and_count_as_waiting() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let (spotify_seen, calls_seen) = (seen.clone(), seen.clone());
        let env = Env {
            addon: "test",
            source: "export default { async items() {
               const thrown = async (f) => { try { await f(); return 'no error'; } catch (e) { return e.message; } };
               return [spotify.user(), await spotify.get('/a?b=1'), await spotify.post('/p', {set: 'collection'}), await spotify.post('/empty'),
                 await addons.call('other', 'find', 'a song', 2, {x: [1]}), await addons.call('other', 'nothing'),
                 await thrown(() => spotify.get('/fail')), await thrown(() => addons.call('other', 'fail'))];
             }, open() {} };",
            hosts: &Hosts::default(),
            storage: Arc::new(Storage::open(PathBuf::from("/dev/null"))),
            prober: Arc::new(Prober::new(None, "127.0.0.1:9".parse().unwrap())),
            spotify: Some(Arc::new(move |method: &str, path: &str, args: Vec<Json>| {
                spotify_seen.lock().unwrap().push(format!("{method} {path} {}", Json::Array(args.clone())));
                match path {
                    "/fail" => Err("SIGN_IN_REQUIRED: gone".into()),
                    _ if method == "USER" => Ok(Json::from("someone")),
                    // Slow: the 1 s of JavaScript time allowed is not spent on it.
                    _ => { std::thread::sleep(Duration::from_millis(700)); Ok(serde_json::json!({"path": path, "body": args})) }
                }
            })),
            calls: Some(Arc::new(move |id: &str, name: &str, args: Vec<Json>| {
                calls_seen.lock().unwrap().push(format!("{id}.{name} {}", Json::Array(args.clone())));
                match name {
                    "fail" => Err("other: no such song".into()),
                    "nothing" => Ok(Json::Null),
                    _ => { std::thread::sleep(Duration::from_millis(700)); Ok(serde_json::json!({"url": "https://cdn.example/a.m4a"})) }
                }
            })),
        };
        let got = call(&env, "items", &[], QUICK).unwrap();
        assert_eq!(got, serde_json::json!([
            "someone", {"path": "/a?b=1", "body": []}, {"path": "/p", "body": [{"set": "collection"}]}, {"path": "/empty", "body": []},
            {"url": "https://cdn.example/a.m4a"}, null, "SIGN_IN_REQUIRED: gone", "other: no such song"
        ]));
        assert_eq!(*seen.lock().unwrap(), [
            "USER / []", "GET /a?b=1 []", "POST /p [{\"set\":\"collection\"}]", "POST /empty []",
            "other.find [\"a song\",2,{\"x\":[1]}]", "other.nothing []", "GET /fail []", "other.fail []",
        ]);
    }

    #[test]
    fn base64_as_browsers_do_it() {
        let got = run(
            "export default { items() {
               const thrown = (f) => { try { f(); return 'no error'; } catch (e) { return e.name; } };
               return [btoa(''), btoa('hello'), btoa('\\xff\\xfe\\x00'), atob('aGVsbG8='), atob(' aGVs bG8\\n'), atob('aGVsbG8'), atob('//4A') === '\\xff\\xfe\\x00',
                 thrown(() => btoa('\\u20ac')), thrown(() => atob('a')), thrown(() => atob('a*bc')), thrown(() => atob('aGVsbG8=='))];
             }, open() {} };",
            &[],
            QUICK,
        );
        assert_eq!(
            got.unwrap(),
            serde_json::json!(["", "aGVsbG8=", "//4A", "hello", "hello", "hello", true, "InvalidCharacterError", "InvalidCharacterError", "InvalidCharacterError", "InvalidCharacterError"])
        );
    }

    #[test]
    fn url_resolves_links_and_edits_queries() {
        let got = run(
            r#"export default { items() {
               const thrown = (f) => { try { f(); return 'no error'; } catch (e) { return e.name; } };
               const u = new URL('../b/c.mp4?x=1#t', 'https://user:pw@Example.com:8443/a/d/index.html');
               const got = [u.href, u.protocol, u.host, u.hostname, u.port, u.pathname, u.search, u.hash, u.origin, u.username,
                 new URL('//cdn.example/x', 'https://a.example/y').href, new URL('/root', new URL('https://a.example/y/z')).href,
                 new URL('?page=2', 'https://a.example/list?page=1').href, new URL('https://a.example:443/').port,
                 thrown(() => new URL('not a link')), thrown(() => new URL('/relative'))];
               const v = new URL('https://api.example/search?q=a+b&q=c%26d&empty');
               const p = v.searchParams;
               got.push(p.get('q'), p.getAll('q'), p.has('empty'), p.get('empty'), p.get('none'));
               p.set('q', 'x y'); p.append('key', "it's~"); p.delete('empty');
               got.push(v.href, [...p], [...p.keys()], p.toString());
               v.search = '?z=1';
               v.pathname = '/other';
               got.push(p.get('z'), String(v), JSON.stringify({ v }), new URLSearchParams({ a: 1, b: 'two words' }).toString(),
                 new URLSearchParams([['x', 'é']]).toString());
               return got;
             }, open() {} };"#,
            &[],
            QUICK,
        );
        assert_eq!(
            got.unwrap(),
            serde_json::json!([
                "https://user:pw@example.com:8443/a/b/c.mp4?x=1#t", "https:", "example.com:8443", "example.com", "8443", "/a/b/c.mp4",
                "?x=1", "#t", "https://example.com:8443", "user",
                "https://cdn.example/x", "https://a.example/root", "https://a.example/list?page=2", "",
                "TypeError", "TypeError",
                "a b", ["a b", "c&d"], true, "", null,
                "https://api.example/search?q=x+y&key=it%27s%7E", [["q", "x y"], ["key", "it's~"]], ["q", "key"], "q=x+y&key=it%27s%7E",
                "1", "https://api.example/other?z=1", "{\"v\":\"https://api.example/other?z=1\"}", "a=1&b=two+words", "x=%C3%A9"
            ])
        );
    }

    #[test]
    fn hash_and_hmac_in_hex() {
        let got = run(
            "export default { items() {
               const fox = 'The quick brown fox jumps over the lazy dog';
               const thrown = (f) => { try { f(); return 'no error'; } catch (e) { return e.message; } };
               return [hash('sha1', 'abc'), hash('sha256', 'abc'), hash('SHA-512', 'abc'), hash('sha256', '\\u00e9'),
                 hmac('sha1', 'key', fox), hmac('sha256', 'key', fox), hmac('sha512', 'key', fox), thrown(() => hash('md5', 'abc'))];
             }, open() {} };",
            &[],
            QUICK,
        );
        assert_eq!(
            got.unwrap(),
            serde_json::json!([
                "a9993e364706816aba3e25717850c26c9cd0d89d",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
                "4a99557e4033c3539de2eb65472017cad5f9557f7a0625a09f1c3f6e2ba69c4c",
                "de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9",
                "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8",
                "b42af09057bac1e2d41708e48a902e09b5ff7f12ab428a4fe86653c73dd248fb82f948a549f7b791a5b41915ee4d1ec3935357e4e2317250d0372afa2ebeeb3a",
                "hash: md5 is not sha1, sha256 or sha512"
            ])
        );
    }

    #[test]
    fn memory_is_limited() {
        let error = run("export default { items() { const a = []; for (;;) a.push('x'.repeat(1 << 20) + a.length); }, open() {} };", &[], QUICK).unwrap_err();
        assert!(error.contains("out of memory"), "{error}");
    }

    #[test]
    fn cpu_is_limited() {
        let started = Instant::now();
        let error = run("export default { items() { for (;;) {} }, open() {} };", &[], QUICK).unwrap_err();
        assert!(error.contains("longer than 1 s"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn sleep_is_waited_time_within_the_call() {
        // 1.2 s asleep, with 1 s of JavaScript time allowed.
        let started = Instant::now();
        let got = run("export default { async items() { await sleep(600); await sleep('600'); return 'woke'; }, open() {} };", &[], QUICK);
        assert_eq!(got.unwrap(), serde_json::json!("woke"));
        assert!(started.elapsed() >= Duration::from_millis(1200));
        let started = Instant::now();
        let error = run("export default { async items() { await sleep(60000); }, open() {} };", &[], QUICK).unwrap_err();
        assert!(error.starts_with("sleep(60000) would outlast the call: a call may take 10 s in all"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn fetch_refuses_hosts_not_listed() {
        let error = run("export default { async items() { await fetch('https://elsewhere.example.org/x'); }, open() {} };", &["example.com"], QUICK).unwrap_err();
        assert!(error.contains("elsewhere.example.org is not in the addon's hosts"), "{error}");
        // `*` is any public host, never this device.
        let error = run("export default { async items() { await fetch('http://127.0.0.1:32400/rex/sources'); }, open() {} };", &["*"], QUICK).unwrap_err();
        assert!(error.contains("127.0.0.1 is not in the addon's hosts"), "{error}");
        let error = run("export default { async items() { await fetch('http://localtest.me:32400/'); }, open() {} };", &["*"], QUICK);
        if let Err(error) = error {
            // localtest.me resolves to 127.0.0.1 where DNS answers; offline it does not resolve at all.
            assert!(error.contains("this device") || error.contains("resolve"), "{error}");
        }
    }

    #[test]
    fn no_imports_and_no_timers_to_wait_on() {
        let error = run("import x from './other.js'; export default { items() { return x; }, open() {} };", &[], QUICK).unwrap_err();
        assert!(error.contains("other.js"), "{error}");
        let error = run("export default { items() { return new Promise(() => {}); }, open() {} };", &[], QUICK).unwrap_err();
        assert!(error.contains("never finished"), "{error}");
    }

    #[test]
    fn storage_persists_between_calls() {
        let path = std::env::temp_dir().join(format!("rex-addons-storage-keep-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let source = "export default { items() { const n = (storage.get('n') ?? 0) + 1; storage.set('n', n); storage.set('gone', 1); storage.delete('gone');
            storage.set('probe:b', 1); storage.set('probe:a', 2); storage.set('probed', 3);
            return [n, storage.get('gone'), storage.keys(), storage.keys('probe:'), storage.keys('none')]; }, open() {} };";
        let call_once = || {
            let env = Env {
                addon: "test",
                source,
                hosts: &Hosts::default(),
                storage: Arc::new(Storage::open(path.clone())),
                prober: Arc::new(Prober::new(None, "127.0.0.1:9".parse().unwrap())),
                spotify: None,
                calls: None,
            };
            call(&env, "items", &[], QUICK).unwrap()
        };
        let keys = serde_json::json!(["n", "probe:a", "probe:b", "probed"]);
        assert_eq!(call_once(), serde_json::json!([1, null, keys, ["probe:a", "probe:b"], []]));
        assert_eq!(call_once(), serde_json::json!([2, null, keys, ["probe:a", "probe:b"], []]));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_thrown_error_names_its_message() {
        let error = run("export default { items() { throw new Error('catalog down'); }, open() {} };", &[], QUICK).unwrap_err();
        assert!(error.starts_with("catalog down"), "{error}");
    }

    #[test]
    fn ffprobe_needs_the_flag_and_the_hosts() {
        let error = run("export default { async items() { return await ffprobe('https://example.com/a.mp4'); }, open() {} };", &["example.com"], QUICK).unwrap_err();
        assert!(error.starts_with("ffprobe: ffprobe() is not available: rex-addons runs without --ffprobe"), "{error}");
        let env = Env {
            addon: "test",
            source: "export default { async items() { return await ffprobe('http://127.0.0.1:32400/library/parts/1/file.mp4'); }, open() {} };",
            hosts: &Hosts(vec!["*".into()]),
            storage: Arc::new(Storage::open(PathBuf::from("/dev/null"))),
            prober: Arc::new(Prober::new(Some("ffprobe".into()), "127.0.0.1:9".parse().unwrap())),
            spotify: None,
            calls: None,
        };
        let error = call(&env, "items", &[], QUICK).unwrap_err();
        assert!(error.starts_with("ffprobe: 127.0.0.1 is not in the addon's hosts"), "{error}");
    }

    /// This machine's network address: the sandbox never fetches loopback.
    fn lan_ip() -> Option<std::net::IpAddr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        let ip = socket.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    }

    /// A server that sends `/stall` nothing and `/drip` a byte every 100 ms, each for 15 s.
    fn slow_server() -> Option<(String, String)> {
        use std::io::Write;
        let lan = lan_ip()?;
        let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let base = format!("http://{lan}:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut request = [0u8; 1024];
                    let read = stream.read(&mut request).unwrap_or(0);
                    if request[..read].starts_with(b"GET /drip") {
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n");
                        for _ in 0..150 {
                            if stream.write_all(b"x").is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    } else {
                        std::thread::sleep(Duration::from_secs(15));
                    }
                });
            }
        });
        Some((base, lan.to_string()))
    }

    #[test]
    fn binary_bodies_both_ways() {
        let Some(lan) = lan_ip() else {
            eprintln!("skipped: no network address");
            return;
        };
        let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
        let base = format!("http://{lan}:{}", server.server_addr().to_ip().unwrap().port());
        std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = Vec::new();
                request.as_reader().read_to_end(&mut body).unwrap();
                let answer = if request.url() == "/bin" { vec![0, 255, 128, 10] } else { body };
                let _ = request.respond(tiny_http::Response::from_data(answer));
            }
        });
        let got = run(
            &format!(
                "export default {{ async items() {{
                   const echo = async (body) => (await fetch('{base}/echo', {{ method: 'POST', body }}));
                   const bin = await (await fetch('{base}/bin')).bytes();
                   const once = await fetch('{base}/bin');
                   await once.bytes();
                   let again;
                   try {{ await once.text(); }} catch (e) {{ again = e.message; }}
                   const sent = new Uint8Array([1, 2, 250, 0]);
                   return [bin instanceof Uint8Array, Array.from(bin), again, Array.from(await (await echo(sent.subarray(1))).bytes()),
                     Array.from(await (await echo(sent.buffer)).bytes()), await (await echo('text \\u00e9')).text()];
                 }}, open() {{}} }};"
            ),
            &[&lan.to_string()],
            QUICK,
        );
        assert_eq!(got.unwrap(), serde_json::json!([true, [0, 255, 128, 10], "the body was already read", [2, 250, 0], [1, 2, 250, 0], "text é"]));
    }

    #[test]
    fn a_slow_server_cannot_outlast_the_call() {
        let Some((base, lan)) = slow_server() else {
            eprintln!("skipped: no network address");
            return;
        };
        let limits = Limits { wall: Duration::from_secs(2), ..QUICK };
        for (what, source) in [
            ("fetch", format!("export default {{ async items() {{ await fetch('{base}/stall'); }}, open() {{}} }};")),
            ("body", format!("export default {{ async items() {{ try {{ return await (await fetch('{base}/drip')).text(); }} catch (e) {{ return e.message; }} }}, open() {{}} }};")),
        ] {
            let started = Instant::now();
            let got = run(&source, &[&lan], limits);
            assert!(started.elapsed() < Duration::from_secs(5), "{what}: took {:?}", started.elapsed());
            let (Ok(Json::String(message)) | Err(message)) = got else { panic!("{what}: {got:?}") };
            assert!(message.contains("2 s"), "{what}: {message}");
        }
    }
}
