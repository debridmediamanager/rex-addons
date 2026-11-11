// Your YouTube playlists in rex: Liked videos, Watch later, your own playlists and any playlist
// you link. Each YouTube playlist is a rex playlist in YouTube's order; each video is one episode
// of its channel's show in the YouTube library. Nothing is downloaded: rex reads the video from
// YouTube as it plays.
//
// Everything goes through YouTube's TV app API (InnerTube), signed in with the TV app's own code
// sign-in, as a TV does. The playlists are listed as YouTube's current TV app lists them. A video's
// files are asked for as an older Samsung TV's app asks, which YouTube still hands plain links to
// every file: picture and sound apart, up to 4K, and the 360p MP4 with both together (the current
// TV app gets only the 360p file; measured 2026-11-11). A link works only once its `n` parameter
// (and, for some videos, its signature) is transformed by a function in YouTube's player script,
// as the player does in a browser. The addon downloads that script when YouTube changes it, cuts
// out the function, keeps it in storage, and runs it for each link.
//
// Up to 1080p a video plays from its H.264 picture and its AAC sound, which rex copies into what a
// Plex app plays without converting either. The two are separate files, and rex reads one item at
// one address, so the addon writes an HLS presentation for each video: a media playlist for each
// file, its segments byte ranges of YouTube's own link read off the file's index (`sidx`), and a
// master playlist naming both. rex-addons serves the playlists; FFmpeg reads the media from YouTube.
// A video without that pair, or whose pair fails in any way, plays from its 360p MP4 as it is.

const YT = "https://www.youtube.com";
const OAUTH = {
  // YouTube on TV's own client: its device flow is the one YouTube accepts for the TV app's API.
  client_id: "861556708454-d6dlm3lh05idd8npek18k6be8ba3oc68.apps.googleusercontent.com",
  client_secret: "SboVhoG9s0rNafixCSGGKXAT",
  scope: "http://gdata.youtube.com https://www.googleapis.com/auth/youtube",
};
const TV = {
  ua: "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version",
  // ponytail: the version measured signed in on 2026-11-11; YouTube's TV page names the current one.
  client: { clientName: "TVHTML5", clientVersion: "7.20250923.13.00" },
};
// The client a video's files are asked for as (yt-dlp's tv_samsung): YouTube hands it a plain link
// to every file, signed in. Its listings answer 500, so the playlists come from TV above.
const SAMSUNG_UA = "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1";
const SAMSUNG = {
  ua: SAMSUNG_UA,
  client: { clientName: "TVHTML5", clientVersion: "5.20260707", deviceMake: "Samsung", deviceModel: "UKS9800", osName: "Tizen", osVersion: "2.4.0", userAgent: SAMSUNG_UA },
};
// Any video's id: YouTube lists the account's own playlists in its "save to playlist" answer for one.
const ANY_VIDEO = "jNQXAC9IVRw";

// A video is checked (one player call, its files' indexes read and the first two segments of each
// probed, a few MB) before it reaches rex. A sync checks this many at most, within this time, pausing between them, and the
// rest arrive over the next hourly syncs; what a check learns is kept while the video stays listed.
const CHECKS_PER_SYNC = 30;
const CHECK_BUDGET_MS = 5 * 60 * 1000;
const CHECK_PAUSE_MS = 1500;
// The playlists are listed again at most this often (and whenever the settings change); the
// syncs between only check videos still waiting.
const LISTING_MS = 6 * 60 * 60 * 1000;
// A video that could not be checked (private, removed, no file) is asked about again after this.
const RETRY_MS = 24 * 60 * 60 * 1000;
// Which player script YouTube's TV app runs is looked up again after this.
const PLAYER_MS = 60 * 60 * 1000;

const textOf = (node) => node?.simpleText ?? (node?.runs ?? []).map((run) => run.text).join("") ?? "";

/** Every node under `node` with one of `keys`, in document order, looking inside neither them nor
 * the `skip` keys. */
function find(node, keys, skip = [], out = []) {
  if (Array.isArray(node)) node.forEach((inner) => find(inner, keys, skip, out));
  else if (node && typeof node === "object") {
    for (const [key, value] of Object.entries(node)) {
      if (keys.includes(key)) out.push([key, value]);
      else if (!skip.includes(key)) find(value, keys, skip, out);
    }
  }
  return out;
}
const VIDEOS = ["tileRenderer"];

// ---------------------------------------------------------------- signing in

async function oauth(path, fields) {
  const answer = await fetch(`${YT}/o/oauth2/${path}`, {
    method: "POST", headers: { "Content-Type": "application/json", "User-Agent": TV.ua }, body: JSON.stringify(fields),
  });
  if (answer.status >= 500) throw new Error(`YouTube's sign-in answered ${answer.status}`);
  return { status: answer.status, ...(await answer.json()) };
}

function keep(answer) {
  const before = storage.get("tokens") ?? {};
  storage.set("tokens", {
    access: answer.access_token,
    refresh: answer.refresh_token ?? before.refresh,
    expires_at: Date.now() + (Number(answer.expires_in) || 3600) * 1000,
  });
}

/** The access token, refreshed when it is about to expire (or `force`). */
async function accessToken(force = false) {
  const tokens = storage.get("tokens");
  if (!tokens?.refresh) throw new Error("SIGN_IN_REQUIRED: not signed in to YouTube");
  if (!force && tokens.access && tokens.expires_at > Date.now() + 60 * 1000) return tokens.access;
  const answer = await oauth("token", {
    client_id: OAUTH.client_id, client_secret: OAUTH.client_secret, refresh_token: tokens.refresh, grant_type: "refresh_token",
  });
  if (answer.error || !answer.access_token) {
    throw new Error(`SIGN_IN_REQUIRED: YouTube refused the sign-in (${answer.error_description ?? answer.error ?? answer.status})`);
  }
  keep(answer);
  return answer.access_token;
}

// ---------------------------------------------------------------- InnerTube

/** One InnerTube call as a TV app (`app`), signed in, the token refreshed once when YouTube refuses
 * it. The request's shape (never the token) is logged, so a changed answer can be told from a
 * changed request. */
async function innertube(endpoint, body, app = TV) {
  console.debug(`youtubei/${endpoint}: ${JSON.stringify(body).slice(0, 120)}`);
  const call = async (token) => {
    const answer = await fetch(`${YT}/youtubei/v1/${endpoint}?prettyPrint=false`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json", "User-Agent": app.ua, Authorization: `Bearer ${token}`,
        "X-YouTube-Client-Name": "7", "X-YouTube-Client-Version": app.client.clientVersion,
      },
      body: JSON.stringify({ context: { client: { ...app.client, hl: "en", gl: "US" } }, ...body }),
    });
    if (answer.status === 429) throw new Error("YouTube answered 429 (too many requests from this address); the next sync tries again");
    if (answer.status === 401 || answer.status === 403) return answer.status;
    if (!answer.ok) throw new Error(`YouTube's ${endpoint} answered ${answer.status}`);
    return await answer.json();
  };
  let answer = await call(await accessToken());
  if (typeof answer === "number") answer = await call(await accessToken(true));
  if (typeof answer === "number") throw new Error(`SIGN_IN_REQUIRED: YouTube answered ${answer} to the signed-in account`);
  return answer;
}

/** The account's own playlists, as its "save to playlist" answer lists them: [{id, title}]. */
async function ownPlaylists() {
  const answer = await innertube("playlist/get_add_to_playlist", { videoIds: [ANY_VIDEO] });
  if (!find(answer, ["addToPlaylistRenderer"]).length) throw new Error("YouTube did not list the account's playlists");
  const found = find(answer, ["playlistAddToOptionRenderer"]).map(([, option]) => ({ id: option.playlistId, title: textOf(option.title) }));
  return found.filter((one) => one.id && !["WL", "LL"].includes(one.id));
}

/** The signed-in account's name: of the accounts YouTube lists (a family's, a brand account's),
 * the one selected. */
async function accountName() {
  try {
    const answer = await innertube("account/accounts_list", {});
    const accounts = find(answer, ["accountItem"]).map(([, item]) => item);
    return textOf((accounts.find((item) => item.isSelected) ?? accounts[0])?.accountName) || "YouTube";
  } catch (error) {
    console.warn(`the account's name: ${error.message}`);
    return "YouTube";
  }
}

/** One video's tile of a playlist page as {id, title, channel, thumb}, or null for what is not a
 * playable video: private or deleted (no length, no title), live or upcoming. */
function entry([, node]) {
  if (node.contentType && node.contentType !== "TILE_CONTENT_TYPE_VIDEO") return null;
  const id = node.onSelectCommand?.watchEndpoint?.videoId ?? node.contentId;
  const header = node.header?.tileHeaderRenderer ?? {};
  const overlays = (header.thumbnailOverlays ?? []).map((o) => o.thumbnailOverlayTimeStatusRenderer).filter(Boolean);
  if (!id || !overlays.some((o) => textOf(o.text)) || overlays.some((o) => ["LIVE", "UPCOMING"].includes(o.style))) return null;
  const meta = node.metadata?.tileMetadataRenderer ?? {};
  const thumbs = (header.thumbnail?.thumbnails ?? []).filter((t) => /^https:\/\//.test(t.url ?? ""));
  return {
    id, title: textOf(meta.title), channel: textOf(meta.lines?.[0]?.lineRenderer?.items?.[0]?.lineItemRenderer?.text),
    thumb: thumbs.reduce((best, t) => (!best || (t.width ?? 0) > (best.width ?? 0) ? t : best), null)?.url?.replace(/\?.*/, ""),
  };
}

/** The first `count` videos of a playlist, in its order, following its continuations, and its
 * title. Throws rather than answer with part of it. */
async function listPlaylist(id, count) {
  let answer = await innertube("browse", { browseId: `VL${id}` });
  const title = find(answer.contents ?? {}, ["entityMetadataRenderer", "playlistHeaderRenderer"], VIDEOS)
    .map(([, node]) => textOf(node.title)).find(Boolean);
  const videos = [];
  for (let page = 0; ; page++) {
    const entries = find(answer.contents ?? answer.continuationContents ?? answer.onResponseReceivedActions ?? {}, VIDEOS);
    const next = find(answer, ["nextContinuationData", "reloadContinuationData", "continuationCommand"], VIDEOS)
      .map(([, node]) => node.continuation ?? node.token).find(Boolean);
    if (!page && !title && !entries.length && !next) {
      const alert = find(answer.alerts ?? [], ["text"]).map(([, node]) => textOf(node)).find(Boolean);
      throw new Error(`playlist ${id}: ${alert || "YouTube answered no playlist"}`);
    }
    for (const video of entries.map(entry)) {
      if (video && !videos.some((one) => one.id === video.id) && videos.length < count) videos.push(video);
    }
    if (videos.length >= count || !next) break;
    if (page > count / 5 + 10) throw new Error(`playlist ${id}: more pages than ${count} videos fill`);
    answer = await innertube("browse", { continuation: next });
    if (!answer.continuationContents && !answer.onResponseReceivedActions) {
      throw new Error(`playlist ${id}: YouTube answered page ${page + 2} with nothing`);
    }
  }
  return { title, videos };
}

// ---------------------------------------------------------------- the player script's n function

// Enough of a JavaScript tokenizer to tell code from strings, comments and regular expressions, and
// to know how deep in brackets it is.
const WORD = /[A-Za-z_$][\w$]*/y;
const NUMBER = /(?:0[xXoObB][\da-fA-F_]+|(?:\d[\d_]*\.?[\d_]*|\.\d[\d_]*)(?:[eE][+-]?\d+)?)n?/y;
const SPACE = /(?:\s+|\/\/[^\n]*|\/\*[\s\S]*?\*\/)+/y;
const REGEX = /\/(?:[^/\\[\n]|\\.|\[(?:[^\]\\\n]|\\.)*\])+\/[a-z]*/y;
const PUNCT = /=>|\.\.\.|\?\.(?!\d)|[=!]==?|[<>]=|&&=?|\|\|=?|\?\?=?|\*\*=?|<<=?|>>>?=?|[-+*/%&|^]=|\+\+|--|[^\s]/y;
const REGEX_AFTER = new Set(["return", "typeof", "case", "do", "else", "in", "of", "new", "delete", "void", "throw", "instanceof", "yield", "await"]);

function tokens(src, at) {
  const t = { at, type: "", start: 0, text: "", stack: [] };
  const sticky = (re) => {
    re.lastIndex = t.at;
    return re.exec(src) ? re.lastIndex : -1;
  };
  // Where a string (or a template's text) that starts at `t.at` ends: a character at a time, as
  // QuickJS's regular expressions recurse deeper on a long string than a small stack allows.
  const skip = (ends) => {
    let at = t.at;
    for (let c = src[at]; !ends.includes(c); c = src[at]) {
      if (c === undefined) throw new Error(`the player script has an unended string at ${t.start}`);
      at += c === "\\" ? 2 : 1;
    }
    return at;
  };
  const template = () => {
    for (t.at = skip("`$"); src[t.at] === "$" && src[t.at + 1] !== "{"; t.at = skip("`$")) t.at += 1;
    if (src[t.at] === "`") t.at += 1;
    else if (src.startsWith("${", t.at)) {
      t.at += 2;
      t.stack.push("`");
    } else throw new Error(`the player script has an unended template at ${t.start}`);
  };
  t.next = () => {
    const regexAllowed = t.type === "word" ? REGEX_AFTER.has(t.text) : t.type === "punct" && !")]".includes(t.text);
    const space = sticky(SPACE);
    if (space > 0) t.at = space;
    t.start = t.at;
    if (t.at >= src.length) return (t.type = "");
    const c = src[t.at];
    let end;
    if ((end = sticky(WORD)) > 0) {
      [t.type, t.text, t.at] = ["word", src.slice(t.start, end), end];
    } else if ((c >= "0" && c <= "9") || (c === "." && src[t.at + 1] >= "0" && src[t.at + 1] <= "9")) {
      [t.type, t.at] = ["number", sticky(NUMBER)];
    } else if (c === '"' || c === "'") {
      t.at += 1;
      [t.type, t.at] = ["string", skip(c) + 1];
    } else if (c === "`") {
      [t.type, t.at] = ["string", t.at + 1];
      template();
    } else if (c === "/" && (regexAllowed || !t.type)) {
      if ((t.at = sticky(REGEX)) < 0) throw new Error(`the player script has an unended regular expression at ${t.start}`);
      t.type = "string";
    } else {
      [t.type, t.at] = ["punct", sticky(PUNCT)];
      t.text = src.slice(t.start, t.at);
      if (t.text === "(" || t.text === "[" || t.text === "{") t.stack.push(t.text);
      else if ((t.text === ")" || t.text === "]" || t.text === "}") && t.stack.pop() === "`") {
        t.type = "string";
        template();
      }
    }
    return t.type;
  };
  return t;
}

/** The top-level statements of the player script's one function, [{start, end, names}]: `names` are
 * what each declares or assigns to, Closure Compiler's `A=Foo.prototype;A.bar=...` counting as Foo. */
function statements(src) {
  // ponytail: the TV player's shape, (function(){...}).call(this); yt-dlp also reads (function(g){...})(_yt_player).
  const open = src.search(/^\s*\(function\s*\(\)\s*\{/);
  if (open < 0) throw new Error("the player script is not one function");
  const body = src.indexOf("{", open) + 1;
  const t = tokens(src, body);
  const list = [], aliases = new Map();
  let current = null, part = null;
  const add = (name) => current.names.includes(name) || current.names.push(name);
  const close = () => {
    if (!part?.target) return;
    const { target, member, value } = part;
    if (!member && value?.length > 1 && value.at(-1) === "prototype") {
      aliases.set(target, value[0]);
      add(value[0]);
    } else if (member && aliases.has(target)) add(aliases.get(target));
    else {
      if (!member) aliases.delete(target);
      add(target);
    }
  };
  const finish = (end) => {
    close();
    if (current) list.push({ start: current.start, end, names: current.names });
    current = part = null;
  };
  for (;;) {
    const before = t.stack.length;
    if (!t.next()) throw new Error("the player script ends inside its function");
    const depth = t.stack.length;
    const punct = t.type === "punct" ? t.text : "";
    if (punct === "}" && before === 0) {
      finish(t.start);
      return { list, strict: /^\s*["']use strict["']/.test(src.slice(body, body + 20)) };
    }
    if (!current) {
      if (punct === ";") continue;
      const kind = t.type === "word" ? t.text : "";
      current = { start: t.start, names: [], kind };
      if (kind === "function" || kind === "class") part = { state: "name" };
      else if (["var", "let", "const"].includes(kind)) part = { state: "name" };
      else part = kind ? { state: "lead", name: kind } : { state: "done" };
      continue;
    }
    if (before === 0 && depth === 0 && (punct === ";" || punct === ",")) {
      if (punct === ";") finish(t.at);
      else {
        close();
        part = { state: ["var", "let", "const"].includes(current.kind) ? "name" : "start" };
      }
      continue;
    }
    if (before === 1 && depth === 0 && punct === "}") {
      // A block ends: so does a declaration, and any statement a new one follows.
      const peek = tokens(src, t.at);
      peek.next();
      if (current.kind === "function" || current.kind === "class" || (peek.type === "word" && !["else", "catch", "finally", "while", "instanceof", "in", "of"].includes(peek.text))) finish(t.at);
      else if (part) part.state = "done";
      continue;
    }
    if (before > 0 && depth > 0) continue;
    const word = t.type === "word" ? t.text : null;
    switch (part.state) {
      case "name":
        if (word) add(word);
        part.state = "done";
        break;
      case "start":
        part = word ? { state: "lead", name: word } : { state: "done" };
        break;
      case "lead":
        if (punct === ".") part.state = "dot";
        else if (punct === "[" || punct === "]") part.member = true;
        else if (punct === "=") Object.assign(part, { state: "value", target: part.name, value: [] });
        else part.state = "done";
        break;
      case "dot":
        part.member = true;
        part.state = word ? "lead" : "done";
        break;
      case "value":
        // A plain chain of names (Foo.prototype), or anything else.
        if (word && (!part.value.length || part.dot)) {
          part.value.push(word);
          part.dot = false;
        } else if (punct === "." && !part.dot && part.value.length) part.dot = true;
        else [part.value, part.state] = [null, "assigned"];
        break;
    }
  }
}

const PAGE = {
  hash: "", host: "www.youtube.com", hostname: "www.youtube.com", href: "https://www.youtube.com/watch?v=rex",
  origin: "https://www.youtube.com", password: "", pathname: "/watch", port: "", protocol: "https:", search: "?v=rex", username: "",
};

/** The player's URL function, out of `code`, made to transform `{n, sig}` as the player does: the
 * link it builds is given them, and its first method that is not set, get or clone applies the
 * transforms (yt-dlp's solver does the same). */
function transformer(code, scope) {
  const make = scope ? Function("scope", `with (scope) return ${code}`)(scope) : (0, eval)(code);
  const build = make(globalThis, globalThis, Object.create(null), Object.create(null), PAGE, { prototype: {} });
  return ({ n, sig }) => {
    const url = build("https://youtube.com/watch?v=rex", "s", sig ? encodeURIComponent(sig) : undefined);
    url.set("n", n);
    const proto = Object.getPrototypeOf(url);
    url[Object.keys(proto).concat(Object.getOwnPropertyNames(proto)).find((key) => !["constructor", "set", "get", "clone"].includes(key))]();
    const s = url.get("s");
    return { n: url.get("n"), sig: s ? decodeURIComponent(s) : null };
  };
}

// An `n` and a signature to try a cut-out function on.
const SAMPLE = { n: "rexAddonsCheck01", sig: "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz" };
const solve = (transform) => ({ n: transform({ n: SAMPLE.n }).n, sig: transform({ sig: SAMPLE.sig }).sig });

/** The small program out of a player script that transforms `n` and signatures: the function that
 * builds a link (it sets `alr=yes`, as yt-dlp's solver finds it), and only the top-level statements
 * running it really reaches. Those are found by running it with every name it looks up outside
 * itself watched, adding the statements that define the names it missed, until nothing is missing:
 * the player's code swallows its own errors, so a missing name would otherwise just leave `n` as
 * it was. */
function cut(src) {
  const { list, strict } = statements(src);
  const defines = new Map();
  list.forEach((s, i) => s.names.forEach((name) => defines.set(name, [...(defines.get(name) ?? []), i])));
  const candidates = new Set();
  for (let at = src.indexOf('.set("alr","yes")'); at >= 0; at = src.indexOf('.set("alr","yes")', at + 1)) {
    list.find((s) => s.start <= at && at < s.end)?.names.forEach((name) => candidates.add(name));
  }
  const found = [];
  for (const entry of candidates) {
    const keep = new Set(), names = new Set();
    const want = (first) => {
      for (const todo = [first]; todo.length;) {
        const name = todo.pop();
        names.add(name);
        for (const i of defines.get(name) ?? []) {
          if (!keep.has(i)) {
            keep.add(i);
            todo.push(...list[i].names);
          }
        }
      }
    };
    want(entry);
    for (let round = 0; round < 500; round++) {
      const code = `(function(window,self,document,navigator,location,XMLHttpRequest){${strict ? "'use strict';" : ""}\n${[...keep].sort((a, b) => a - b)
        .map((i) => src.slice(list[i].start, list[i].end).trim().replace(/[^;}]$/, "$&;")).join("\n")}\nreturn ${entry};})`;
      const missing = new Set();
      const scope = new Proxy({}, { has: (_, name) => (typeof name === "string" && defines.has(name) && !names.has(name) && missing.add(name), false) });
      let result;
      try {
        result = solve(transformer(code, scope));
      } catch (error) {
        result = error;
      }
      if (missing.size) {
        missing.forEach(want);
        continue;
      }
      if (typeof result?.n === "string" && result.n !== SAMPLE.n && /^[\w-]+$/.test(result.n) && typeof result.sig === "string") found.push({ code, ...result });
      break;
    }
  }
  if (!found.length) throw new Error(`no function in the player script transforms n (tried ${[...candidates].join(", ") || "none"})`);
  if (new Set(found.map((one) => one.n + one.sig)).size > 1) throw new Error("functions in the player script disagree on n");
  // Run once more as it will be: strict, with nothing watched.
  const { code, n, sig } = found[0];
  const alone = solve(transformer(code));
  if (alone.n !== n || alone.sig !== sig) throw new Error("the cut-out n function answers differently on its own");
  return code;
}

/** The TV app's player script as cut down for its links: {id, sts, code, checked}. Looked up again
 * after PLAYER_MS (or when `fresh`), downloaded and cut again only when YouTube changed it. */
async function playerScript(fresh = false) {
  const kept = storage.get("player");
  if (kept && !fresh && Date.now() - kept.checked < PLAYER_MS) return kept;
  const page = await fetch(`${YT}/tv`, { headers: { "User-Agent": TV.ua } });
  if (!page.ok) throw new Error(`YouTube's TV page answered ${page.status}`);
  const path = /"jsUrl":"(\/s\/player\/([\w-]+)\/[^"]+\.js)"/.exec(await page.text());
  if (!path) throw new Error("YouTube's TV page names no player script");
  if (kept?.id === path[2] && kept.code) {
    storage.set("player", { ...kept, checked: Date.now() });
    return storage.get("player");
  }
  const script = await fetch(`${YT}${path[1]}`, { headers: { "User-Agent": TV.ua } });
  if (!script.ok) throw new Error(`YouTube's player script answered ${script.status}`);
  const src = await script.text();
  const sts = Number(/(?:signatureTimestamp|sts)\s*:\s*(\d{5})/.exec(src)?.[1]);
  if (!sts) throw new Error(`player script ${path[2]}: no signature timestamp in it`);
  const started = Date.now();
  let code;
  try {
    code = cut(src);
  } catch (error) {
    throw new Error(`player script ${path[2]}: ${error.message}`);
  }
  console.log(`player ${path[2]}: its n function cut out (${code.length} of ${src.length} bytes) in ${Date.now() - started} ms`);
  storage.set("player", { id: path[2], sts, code, checked: Date.now() });
  return storage.get("player");
}

// ---------------------------------------------------------------- playing

/** The player answer for a video, as the Samsung TV app asks, with the player script's signature
 * timestamp. */
async function player(id, script) {
  return await innertube("player", {
    videoId: id, contentCheckOk: true, racyCheckOk: true,
    playbackContext: { contentPlaybackContext: { signatureTimestamp: script.sts, html5Preference: "HTML5_PREF_WANTS" } },
  }, SAMSUNG);
}

/** Why a player answer cannot play at all, or null. `bot` when YouTube wants proof this is no bot. */
function unplayable(answer) {
  const status = answer.playabilityStatus ?? {};
  if (status.status !== "OK") {
    const why = status.reason || textOf(status.errorScreen?.playerErrorMessageRenderer?.reason) || status.status || "no answer";
    return { why, bot: /not a bot/i.test(why) };
  }
  const details = answer.videoDetails ?? {};
  if (details.isLive || details.isUpcoming || details.isPostLiveDvr || !Number(details.lengthSeconds)) return { why: "a live stream" };
  return null;
}

/** Why a player answer cannot play as a video, or null. */
function refusal(answer) {
  const refused = unplayable(answer);
  if (refused) return refused;
  if (!muxed(answer) && !pair(answer, Infinity)) return { why: "YouTube offered neither H.264 with AAC nor a 360p file" };
  return null;
}

/** The plain file with picture and sound together, H.264 + AAC: 360p (itag 18). */
const muxed = (answer) => (answer.streamingData?.formats ?? []).find((f) => /avc1/.test(f.mimeType) && /mp4a/.test(f.mimeType) && (f.url || f.signatureCipher));

/** A file's link, its `n` (and signature) transformed as the player script does. */
function link(format, transform) {
  let url;
  if (format.url) url = new URL(format.url);
  else {
    const cipher = new URLSearchParams(format.signatureCipher);
    url = new URL(cipher.get("url"));
    url.searchParams.set(cipher.get("sp") || "signature", transform({ sig: cipher.get("s") }).sig);
  }
  const n = url.searchParams.get("n");
  if (n) url.searchParams.set("n", transform({ n }).n);
  return url.href;
}

/** A file's last change (its `lastModified`, in microseconds), the day it was put up as near as YouTube says. */
function dated(format) {
  const changed = Number(format.lastModified) / 1000;
  return changed > 0 ? new Date(changed).toISOString().slice(0, 10) : undefined;
}

/** The H.264 picture of most lines up to `most` (YouTube's 1080p of a 1920x804 film counts as
 * 1080), and the AAC sound (itag 140), both with a link and an index: {video, audio}, or null.
 * `itags` picks exactly those, as a video was checked. YouTube lists itag 140 up to three times:
 * plain, with its loudness evened out (`isDrc`) and with voices raised (`xtags`); and once per dub
 * (`audioTrack`). The plain one in the video's own language is taken. */
function pair(answer, most, itags) {
  const files = (answer.streamingData?.adaptiveFormats ?? []).filter((f) => (f.url || f.signatureCipher) && f.initRange && f.indexRange);
  const lines = (f) => parseInt(f.qualityLabel) || Math.min(f.width, f.height);
  const video = files
    .filter((f) => (itags ? f.itag === itags[0] : /^video\/mp4; codecs="avc1/.test(f.mimeType) && lines(f) <= most))
    .sort((a, b) => lines(b) - lines(a) || (b.fps ?? 0) - (a.fps ?? 0) || b.bitrate - a.bitrate)[0];
  const audio = files
    .filter((f) => f.itag === (itags ? itags[1] : 140) && !f.isDrc && (!f.audioTrack || f.audioTrack.audioIsDefault))
    .sort((a, b) => Number(!!a.xtags) - Number(!!b.xtags))[0];
  return video && audio ? { video, audio } : null;
}

/** A file's segments, out of the `sidx` box at `at` in `bytes` (ISO/IEC 14496-12, 8.16.3):
 * [{offset, size, duration}], offsets in the file. */
function segments(bytes, at) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (at + 32 > bytes.length || String.fromCharCode(...bytes.subarray(at + 4, at + 8)) !== "sidx") throw new Error("no index where YouTube says the file's index is");
  const version = bytes[at + 8];
  const timescale = view.getUint32(at + 16);
  const big = (p) => view.getUint32(p) * 2 ** 32 + view.getUint32(p + 4);
  let p = at + 20;
  const first = version === 0 ? view.getUint32(p + 4) : big(p + 8);
  p += version === 0 ? 8 : 16;
  const count = view.getUint16(p + 2);
  p += 4;
  if (!timescale || !count || p + count * 12 > bytes.length) throw new Error("the file's index is cut short");
  // The first segment starts this far after the end of the box.
  let offset = at + view.getUint32(at) + first;
  const list = [];
  for (let i = 0; i < count; i++, p += 12) {
    const reference = view.getUint32(p);
    if (reference >>> 31) throw new Error("the file's index points at another index");
    const size = reference & 0x7fffffff;
    list.push({ offset, size, duration: view.getUint32(p + 4) / timescale });
    offset += size;
  }
  return list;
}

// The picture's playlist says each segment starts this much before it does. FFmpeg's HLS reader
// seeks to where the playlist says a segment starts, then drops the picture up to a keyframe whose
// decode time is at or past that; H.264 with B-frames decodes a keyframe a frame or two before it
// shows it, so the segment's own keyframe was dropped and, after a seek, the picture came up to
// 5 s after the sound (FFmpeg 8.1, measured 2026-11-11). Starting the timeline a little early
// (shortening the first segment) keeps it.
const EARLY = 0.25;

/** A media playlist of `segments` of the file at `url`, whose first `init` bytes are its header. */
function mediaPlaylist(url, init, list) {
  const lines = ["#EXTM3U", "#EXT-X-VERSION:7", `#EXT-X-TARGETDURATION:${Math.ceil(Math.max(...list.map((s) => s.duration)))}`,
    "#EXT-X-PLAYLIST-TYPE:VOD", "#EXT-X-MEDIA-SEQUENCE:0", `#EXT-X-MAP:URI="${url}",BYTERANGE="${init}@0"`];
  for (const s of list) lines.push(`#EXTINF:${s.duration.toFixed(6)},`, `#EXT-X-BYTERANGE:${s.size}@${s.offset}`, url);
  return lines.concat("#EXT-X-ENDLIST", "").join("\n");
}

/** The HLS presentation of a picture and sound pair, as open() returns it: {playlist, playlists}.
 * Each file's index is read (a couple of KB) to cut it into segments. */
async function presentation(chosen, transform) {
  const playlists = {};
  for (const [name, format] of Object.entries(chosen)) {
    const url = link(format, transform);
    const answer = await fetch(url, { headers: { Range: `bytes=0-${format.indexRange.end}` } });
    if (answer.status !== 206 && answer.status !== 200) throw new Error(`YouTube answered ${answer.status} for the ${name}'s index`);
    const list = segments(await answer.bytes(), Number(format.indexRange.start));
    if (name === "video") list[0].duration -= Math.min(EARLY, list[0].duration / 2);
    playlists[`${name}.m3u8`] = mediaPlaylist(url, Number(format.initRange.end) + 1, list);
  }
  const { video, audio } = chosen;
  const codecs = (format) => /codecs="([^"]+)"/.exec(format.mimeType)?.[1] ?? "";
  const playlist = ["#EXTM3U", "#EXT-X-VERSION:7", "#EXT-X-INDEPENDENT-SEGMENTS",
    '#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="Original",DEFAULT=YES,AUTOSELECT=YES,URI="audio.m3u8"',
    `#EXT-X-STREAM-INF:BANDWIDTH=${video.bitrate + audio.bitrate},CODECS="${codecs(video)},${codecs(audio)}",RESOLUTION=${video.width}x${video.height}${video.fps ? `,FRAME-RATE=${video.fps}` : ""},AUDIO="audio"`,
    "video.m3u8", ""].join("\n");
  return { playlist, playlists };
}

/** Whether YouTube serves a link: a one-byte read. */
async function serves(url) {
  const answer = await fetch(url, { headers: { Range: "bytes=0-0" } });
  return answer.status === 206 || answer.status === 200;
}

/** What a player answer plays from, probed: {kind: "pair", itags, ...} up to `most` lines, else
 * (or when anything on that way fails) {kind: "mp4", ...}, the 360p file; null when YouTube refuses
 * the file's link, as for an outdated player script. */
async function playable(id, answer, script, most) {
  const transform = transformer(script.code);
  if (most > 360) {
    try {
      const chosen = pair(answer, most);
      if (!chosen) throw new Error(`no H.264 picture up to ${most} lines with AAC sound`);
      const probe = await ffprobe(await presentation(chosen, transform));
      // FFmpeg's HLS reader guesses the rates from the first segments; YouTube says them.
      const rate = (format) => String(format.averageBitrate ?? format.bitrate);
      probe.format.bit_rate = String(Number(rate(chosen.video)) + Number(rate(chosen.audio)));
      for (const stream of probe.streams) stream.bit_rate = rate(stream.codec_type === "video" ? chosen.video : chosen.audio);
      return { kind: "pair", itags: [chosen.video.itag, chosen.audio.itag], height: chosen.video.height, date: dated(chosen.video), probe };
    } catch (error) {
      if (/ffprobe\(\) is not available/.test(error.message)) throw error;
      console.warn(`${id}: ${error.message}; its 360p file plays instead`);
    }
  }
  const file = muxed(answer);
  if (!file) throw new Error("YouTube offered no 360p file");
  const url = link(file, transform);
  if (!(await serves(url))) return null;
  return { kind: "mp4", height: file.height, date: dated(file), probe: await ffprobe(url) };
}

// ---------------------------------------------------------------- songs, for other addons

// How a song is found with what this addon has, the TV app's sign-in (measured 2026-11-11): YouTube
// Music's own search (WEB_REMIX) and the web's both answer 400 to the TV token, so songs are looked
// for as the TV app searches, `youtubei/v1/search` with TVHTML5. Its answer mixes music videos, live
// takes, covers and Shorts; YouTube Music's own recording of a song (its artist's "Topic" track)
// comes as a square tile whose second line starts "Song" and has no length on it. The tiles are
// ranked against the track, and the best is asked for as a play would (the Samsung TV client's player
// call): that gives its true length and its AAC file, and proves it plays. Searching a recording's
// ISRC sometimes brings that tile among unrelated videos, so it is tried when the title finds none.

// A track's length and a tile's may differ by this much and still be one recording.
const LENGTH_SLACK = (ms) => Math.max(4000, ms * 0.025);
// Words naming a take other than the studio one. A tile with one the track's own title lacks is
// another recording (or no recording at all), whatever else agrees.
const OTHER_TAKE = ["live", "remix", "acoustic", "instrumental", "karaoke", "cover", "demo", "unplugged", "sped up", "slowed",
  "reverb", "nightcore", "8d", "piano version", "orchestral", "reaction", "tutorial", "lesson", "mashup", "extended", "loop"];
const MINOR = ["lyrics", "lyric video", "visualizer", "official video", "music video", "official music video", "video"];

/** Text as compared: letters and digits of any script, accents and case off, everything else one space. */
function fold(text) {
  return (text ?? "").normalize("NFKD").replace(/\p{M}/gu, "").toLowerCase().replace(/&/g, " and ")
    .replace(/[^\p{L}\p{N}]+/gu, " ").trim();
}

/** A title without what is in brackets and without who it features: "Song (feat. X)" and "Song [Live]" are "Song". */
const unbracketed = (title) => (title ?? "").replace(/\s*[([{【].*?[)\]}】]/g, " ").replace(/\s+(?:feat|ft|featuring)\.?\s.*$/i, "");
/** A title's name without its version: "Song - Remastered 2011" is "Song" too. */
const bare = (title) => fold(unbracketed(title).replace(/\s+[-–—]\s+.*$/, ""));

/** How alike two folded texts are, 0 to 1: Dice's coefficient of their letter pairs, which works
 * for scripts written without spaces as for those with. */
function alike(a, b) {
  const pairs = (s) => {
    const list = [];
    for (const word of s.split(" ")) for (let i = 0; i < Math.max(1, word.length - 1); i++) list.push(word.slice(i, i + 2));
    return list;
  };
  if (!a || !b) return 0;
  if (a === b) return 1;
  const left = pairs(a), right = pairs(b);
  let same = 0;
  for (const pair of left) {
    const at = right.indexOf(pair);
    if (at >= 0) {
      same += 1;
      right.splice(at, 1);
    }
  }
  return (2 * same) / (left.length + pairs(b).length);
}

const takes = (text, words) => words.filter((word) => ` ${fold(text)} `.includes(` ${word} `));

/** One search tile as {video_id, title, channel, duration_ms (null for a song's tile), kind}, or
 * null for what is not a video: a playlist, a mix, a film, a Short. */
function tile([, lockup]) {
  if (!["LOCKUP_CONTENT_TYPE_MUSIC", "LOCKUP_CONTENT_TYPE_VIDEO"].includes(lockup.contentType) || !lockup.contentId) return null;
  const meta = lockup.metadata?.lockupMetadataViewModel ?? {};
  const rows = (meta.metadata?.contentMetadataViewModel?.metadataRows ?? []).map((row) => (row.metadataParts ?? []).map((part) => part.text?.content ?? ""));
  const badge = find(lockup.contentImage ?? {}, ["thumbnailBadgeViewModel"]).map(([, b]) => b.text).find((t) => /^\d+(:\d\d)+$/.test(t ?? ""));
  const song = rows[1]?.[0] === "Song" || lockup.styling?.contentImageAspectRatio === "LOCKUP_CONTENT_IMAGE_ASPECT_RATIO_SQUARE";
  return {
    video_id: lockup.contentId, title: meta.title?.content ?? "", channel: rows[0]?.[0] ?? "",
    duration_ms: badge ? badge.split(":").reduce((sum, part) => sum * 60 + Number(part), 0) * 1000 : null,
    kind: song ? "song" : "video",
  };
}

/** The videos of a TV search answer, each once, in its order. */
function tiles(answer) {
  const seen = new Set();
  return find(answer.contents ?? {}, ["lockupViewModel"]).map(tile).filter((one) => one && !seen.has(one.video_id) && seen.add(one.video_id));
}

/** How well a tile matches a track `want` ({title, artists, duration_ms}), 0 to 1, or 0 for a tile
 * that is no recording of it: another take, another artist's, a title too unlike, a length too far.
 * A song tile (YouTube Music's recording) is preferred over a music video, which often has more
 * than the song in it. */
function scoreSong(want, one) {
  const title = bare(want.title);
  const artists = (want.artists ?? []).map(fold).filter(Boolean);
  // A video's title often holds its artist ("Artist - Song (Official Video)"): of its parts, the
  // one most like the track's title is the title.
  const parts = (one.title ?? "").replace(/\s*[([{【].*?[)\]}】]/g, " ").split(/\s+[-–—|:]\s+|\s*\/\/\s*/).map(unbracketed).map(fold).filter(Boolean);
  // Or it is somewhere inside it, word for word ("ARTIST「Song」Official Video").
  const inside = title && ` ${fold(one.title)} `.includes(` ${title} `) ? 0.9 : 0;
  const named = Math.max(inside, ...parts.map((part) => alike(title, part)));
  if (named < 0.75) return 0;
  const channel = fold((one.channel ?? "").replace(/\s*-\s*topic$/i, "").replace(/vevo$/i, "").replace(/\bofficial\b/i, ""));
  const where = ` ${channel} ${fold(one.title)} `;
  const byArtist = artists.length === 0 || artists.some((artist) => where.includes(` ${artist} `) || alike(artist, channel) >= 0.8);
  if (!byArtist) return 0;
  // Another take: a word the tile has and the track has not. A take the track is (its own
  // "Live") that the tile does not say is a weaker mismatch: song tiles often leave it off.
  if (takes(one.title, OTHER_TAKE).some((word) => !takes(want.title, OTHER_TAKE).includes(word))) return 0;
  const missing = takes(want.title, OTHER_TAKE).filter((word) => !takes(one.title, OTHER_TAKE).includes(word)).length;
  // A song tile shows no length; YouTube Music's recording is nearly always the track's, and the
  // player call checks it before it counts.
  let length = one.kind === "song" ? 0.8 : 0.5;
  if (one.duration_ms != null && want.duration_ms) {
    const off = Math.abs(one.duration_ms - want.duration_ms);
    if (off > 4 * LENGTH_SLACK(want.duration_ms)) return 0;
    length = off <= LENGTH_SLACK(want.duration_ms) ? 1 - off / (2 * LENGTH_SLACK(want.duration_ms)) : 0.1;
  }
  const kind = one.kind === "song" ? 1 : /official audio|audio oficial|\baudio\b/i.test(one.title) ? 0.8 : takes(one.title, MINOR).length ? 0.4 : 0.6;
  const score = 0.4 * named + 0.3 * length + 0.3 * kind - 0.2 * missing;
  return Math.max(0, Math.min(1, Math.round(score * 1000) / 1000));
}

// A tile scoring under this is no match.
const SONG_FLOOR = 0.6;

/** The AAC file (itag 140) of a player answer: plain, not evened out, in the video's own language. */
const aac = (answer) => (answer.streamingData?.adaptiveFormats ?? [])
  .filter((f) => f.itag === 140 && (f.url || f.signatureCipher) && !f.isDrc && (!f.audioTrack || f.audioTrack.audioIsDefault))
  .sort((a, b) => Number(!!a.xtags) - Number(!!b.xtags))[0];

/** What a play of an AAC file reads: its length, size and rates, from the player answer. */
function audioOf(format) {
  return {
    mime: format.mimeType.split(";")[0], duration_ms: Number(format.approxDurationMs), size: Number(format.contentLength),
    bitrate: Number(format.averageBitrate ?? format.bitrate), sample_rate: Number(format.audioSampleRate) || 44100,
    channels: Number(format.audioChannels) || 2,
  };
}

/** The best of `found` (already scored, best first) that plays and whose true length agrees, with
 * its file's details; at most `tries` player calls. */
async function verified(want, found, tries) {
  for (const one of found.slice(0, tries)) {
    const answer = await innertube("player", { videoId: one.video_id, contentCheckOk: true, racyCheckOk: true }, SAMSUNG);
    const refused = unplayable(answer);
    if (refused?.bot) throw new Error(`YouTube asks this account to prove it is not a bot (${refused.why})`);
    const format = aac(answer);
    if (refused || !format) {
      console.warn(`${one.video_id}: ${refused?.why ?? "no AAC file"}; trying the next`);
      continue;
    }
    const audio = audioOf(format);
    if (want.duration_ms && Math.abs(audio.duration_ms - want.duration_ms) > LENGTH_SLACK(want.duration_ms)) continue;
    const scored = scoreSong(want, { ...one, duration_ms: audio.duration_ms });
    if (scored >= SONG_FLOOR) return { ...one, ...audio, score: scored };
  }
  return null;
}

/** A recording on YouTube of the track `want` ({title, artists, album, duration_ms, isrc}): its
 * artist and title searched, the tiles ranked, and the best ones asked for until one plays at the
 * track's length; failing that, its ISRC searched. {video_id, title, channel, duration_ms, kind,
 * score, via, size, bitrate, mime, sample_rate, channels}, or null when YouTube has none. */
async function findSong(want) {
  if (!want?.title) throw new Error("findSong() needs at least a title");
  const ranked = (answer) => tiles(answer).map((one) => ({ ...one, score: scoreSong(want, one) }))
    .filter((one) => one.score >= SONG_FLOOR).sort((a, b) => b.score - a.score);
  // "Song - Remastered 2011" is searched as "Artist Song": YouTube's titles rarely say the remaster.
  // "Song - Live at Wembley" keeps its take.
  const version = /\s+[-–—]\s+(.*)$/.exec(want.title);
  const title = version && !takes(version[1], OTHER_TAKE).length ? want.title.slice(0, version.index) : want.title;
  const query = [(want.artists ?? [])[0], title].filter(Boolean).join(" ");
  const hit = await verified(want, ranked(await innertube("search", { query })), 2);
  if (hit) return { ...hit, via: "title" };
  // An ISRC search brought a track the title search missed for 2 of 35 tracks of a real library
  // (2026-11-11), so it is the fallback, not the first try.
  if (!/^[A-Z]{2}[A-Z0-9]{3}\d{7}$/.test(want.isrc ?? "")) return null;
  await sleep(1000);
  const byIsrc = await verified(want, ranked(await innertube("search", { query: want.isrc })), 1);
  return byIsrc ? { ...byIsrc, via: "isrc" } : null;
}

/** The AAC file of a video to play now: {url, mime, bitrate, duration_ms, size}. A link lives six
 * hours, so ask again for each play. */
async function songAudio(id) {
  if (!/^[\w-]{11}$/.test(id ?? "")) throw new Error(`songAudio() needs a video id, not ${JSON.stringify(id)}`);
  const script = await playerScript();
  const answer = await player(id, script);
  const refused = unplayable(answer);
  if (refused) throw new Error(`${id} does not play: ${refused.why}`);
  const format = aac(answer);
  if (!format) throw new Error(`${id} does not play: YouTube offered no AAC file`);
  const { mime, bitrate, duration_ms, size } = audioOf(format);
  return { url: link(format, transformer(script.code)), mime, bitrate, duration_ms, size };
}

function settings(config) {
  const number = (value, fallback, low, high) => Math.max(low, Math.min(high, Math.floor(Number(value) || fallback)));
  const links = (config.playlists ?? []).map((link) => link.trim()).filter(Boolean).map((link) => {
    let id = link;
    if (/^https?:\/\//.test(link)) id = new URL(link).searchParams.get("list") ?? "";
    if (!/^[A-Za-z0-9_-]{2,64}$/.test(id)) throw new Error(`${link}: not a YouTube playlist link`);
    return id;
  });
  return {
    count: number(config.count, 200, 1, 5000), most: number(config.max_height, 1080, 360, 1080),
    liked: config.liked !== false, later: config.watch_later !== false, own: config.own !== false, links,
  };
}

/** Every playlist the settings name, each with its first videos: [{title, videos: [{id, title, channel, thumb}]}]. */
async function listAll(wanted) {
  const sources = [];
  if (wanted.liked) sources.push({ id: "LL", title: "Liked videos" });
  if (wanted.later) sources.push({ id: "WL", title: "Watch later" });
  if (wanted.own) sources.push(...(await ownPlaylists()));
  sources.push(...wanted.links.filter((id) => !sources.some((one) => one.id === id)).map((id) => ({ id })));
  const lists = [];
  const titles = new Set();
  for (const source of sources) {
    const listed = await listPlaylist(source.id, wanted.count);
    const named = source.title || listed.title;
    if (!named) throw new Error(`playlist ${source.id}: YouTube answered no title`);
    let title = named;
    for (let n = 2; titles.has(title); n++) title = `${named} (${n})`;
    titles.add(title);
    lists.push({ title, videos: listed.videos });
    console.log(`${title}: ${listed.videos.length} videos`);
  }
  return lists;
}

/** What a check of one video learns: the listing's details, and what plays and its probe.
 * `sync.looked` once the player script was looked up again this sync. */
async function check(listed, most, sync) {
  let script = await playerScript();
  let answer = await player(listed.id, script);
  const refused = refusal(answer);
  if (refused) return { refused };
  let found = await playable(listed.id, answer, script, most);
  if (!found) {
    // YouTube may have moved to another player script since it was looked up.
    if (sync.looked) throw new Error("YouTube refuses the file's link");
    sync.looked = true;
    script = await playerScript(true);
    answer = await player(listed.id, script);
    found = await playable(listed.id, answer, script, most);
    if (!found) throw new Error("YouTube refuses the file's link, also with the newest player script");
  }
  return {
    video: {
      title: listed.title, channel: listed.channel || answer.videoDetails.author || "YouTube", channel_id: answer.videoDetails.channelId,
      thumb: listed.thumb ?? `https://i.ytimg.com/vi/${listed.id}/hqdefault.jpg`, quality: most, ...found,
    },
  };
}

export default {
  async items(ctx) {
    const wanted = settings(ctx.config);
    const key = JSON.stringify([wanted.count, wanted.liked, wanted.later, wanted.own, wanted.links]);
    let listing = storage.get("listing");
    if (!listing || listing.key !== key || Date.now() - listing.at > LISTING_MS) {
      listing = { key, at: Date.now(), lists: await listAll(wanted) };
      storage.set("listing", listing);
    }

    // Each video once, with its place in every playlist it is in.
    const places = new Map();
    for (const list of listing.lists) {
      list.videos.forEach((video, at) => {
        if (!places.has(video.id)) places.set(video.id, { video, at: {} });
        places.get(video.id).at[list.title] = at + 1;
      });
    }

    // Check what is new, or was checked for another quality (or by 1.0, which kept no quality),
    // a bounded number a sync: a video's probe is of the file it plays at that quality.
    const numbers = storage.get("numbers") ?? {};
    const until = Date.now() + CHECK_BUDGET_MS;
    let checks = 0, waiting = 0, halted = false;
    const sync = { looked: false };
    for (const [id, { video: listed }] of places) {
      const known = storage.get(`v:${id}`);
      const skipped = storage.get(`skip:${id}`);
      if (known?.quality === wanted.most || (skipped && skipped.until > Date.now())) continue;
      if (checks >= CHECKS_PER_SYNC || Date.now() > until || halted) {
        waiting += 1;
        continue;
      }
      if (checks++) await sleep(CHECK_PAUSE_MS);
      let checked;
      try {
        checked = await check(listed, wanted.most, sync);
      } catch (error) {
        if (/^(SIGN_IN_REQUIRED|YouTube answered 429|player script|YouTube's TV page|YouTube's player script)|ffprobe\(\) is not available/.test(error.message)) throw error;
        console.warn(`${id}: ${error.message}`);
        storage.set(`skip:${id}`, { until: Date.now() + RETRY_MS, why: error.message });
        continue;
      }
      if (checked.refused) {
        const { why, bot } = checked.refused;
        if (bot) {
          // Asking on would only make it worse: what is ready goes to rex, the rest waits.
          console.warn(`YouTube asks this account to prove it is not a bot (${why}); checks stop until the next sync`);
          halted = true;
          continue;
        }
        console.warn(`${id}: ${why}`);
        storage.set(`skip:${id}`, { until: Date.now() + RETRY_MS, why });
        continue;
      }
      const video = checked.video;
      // Episodes are numbered per channel in the order they arrive, so no number ever moves.
      video.episode = known?.episode ?? (numbers[video.channel_id] = (numbers[video.channel_id] ?? 0) + 1);
      storage.set(`v:${id}`, video);
      storage.delete(`skip:${id}`);
    }
    if (waiting) console.log(`${waiting} videos wait to be checked; they arrive over the next syncs`);

    // Until it is checked again, a video 1.0 checked plays as it did: its 360p file (1.0's YouTube
    // HLS streams were never offered signed in, and are left out).
    const ready = [...places.keys()].map((id) => [id, storage.get(`v:${id}`)]).filter(([, video]) => video && video.kind !== "hls");
    // A show's name and poster: its channel's first video's here, the same on every push (a
    // video made with another channel names both).
    const shows = {};
    for (const [, video] of [...ready].sort(([, a], [, b]) => a.episode - b.episode)) shows[video.channel_id] ??= { title: video.channel, poster: video.thumb };
    const items = ready.map(([id, video]) => ({
      id: `yt-${id}`,
      path: `youtube/${video.channel_id}/${id}.${video.kind === "pair" ? "m3u8" : "mp4"}`,
      kind: "episode", season: 1, episode: video.episode,
      name: shows[video.channel_id].title, release: `youtube:${video.channel_id}`,
      display: { title: video.title, date: video.date, thumb: video.thumb, show: shows[video.channel_id] },
      playlists: places.get(id).at,
      media_info: video.probe,
    }));

    // Forget videos that left every playlist, and channels left with none, so storage stays bounded.
    for (const id of storage.get("known") ?? []) {
      if (!places.has(id)) ["v", "skip"].forEach((kind) => storage.delete(`${kind}:${id}`));
    }
    storage.set("known", [...places.keys()]);
    const channels = new Set(ready.map(([, video]) => video.channel_id));
    storage.set("numbers", Object.fromEntries(Object.entries(numbers).filter(([channel]) => channels.has(channel))));
    console.log(`${items.length} of ${places.size} videos are in rex`);
    return items;
  },

  async open(id) {
    const vid = id.replace(/^yt-/, "");
    const known = storage.get(`v:${vid}`);
    const script = await playerScript();
    const answer = await player(vid, script);
    const refused = refusal(answer);
    if (refused) throw new Error(`${vid} does not play: ${refused.why}`);
    // The files it was checked with, so what plays is what rex was told about. A link lives six
    // hours, so fresh ones are asked for at each play.
    const transform = transformer(script.code);
    if (known?.kind === "pair") {
      const chosen = pair(answer, Infinity, known.itags);
      if (!chosen) throw new Error(`${vid} does not play: YouTube no longer offers its files ${known.itags.join(" and ")}`);
      return await presentation(chosen, transform);
    }
    const file = muxed(answer);
    if (!file) throw new Error(`${vid} does not play: YouTube offered no 360p file`);
    return { url: link(file, transform) };
  },

  // For other addons (manifest `exports`), such as the Spotify addon.
  findSong,
  songAudio,

  async signIn() {
    const device = Array.from({ length: 32 }, () => Math.floor(Math.random() * 16).toString(16)).join("");
    const answer = await oauth("device/code", { client_id: OAUTH.client_id, scope: OAUTH.scope, device_id: device, device_model: "ytlr::" });
    if (answer.error || !answer.device_code) throw new Error(`YouTube refused to start a sign-in: ${answer.error_description ?? answer.error ?? answer.status}`);
    return {
      code: answer.user_code, url: answer.verification_url ?? answer.verification_uri,
      expires_in: answer.expires_in, interval: answer.interval ?? 5, state: { device_code: answer.device_code },
    };
  },

  async signInPoll(state) {
    const answer = await oauth("token", {
      client_id: OAUTH.client_id, client_secret: OAUTH.client_secret, code: state.device_code, grant_type: "http://oauth.net/grant_type/device/1.0",
    });
    if (answer.error === "authorization_pending" || answer.error === "slow_down") return { done: false };
    if (answer.error === "access_denied") throw new Error("the sign-in was refused on the website");
    if (answer.error === "expired_token") throw new Error("the code expired before it was entered");
    if (answer.error || !answer.access_token || !answer.refresh_token) throw new Error(`YouTube refused the sign-in: ${answer.error_description ?? answer.error ?? answer.status}`);
    keep(answer);
    storage.delete("listing");
    return { done: true, account: await accountName() };
  },

  async signOut() {
    const tokens = storage.get("tokens");
    storage.delete("tokens");
    storage.delete("listing");
    if (tokens?.refresh) {
      try {
        await fetch(`${YT}/o/oauth2/revoke?token=${encodeURIComponent(tokens.refresh)}`, { method: "POST" });
      } catch (error) {
        console.warn(`revoking the sign-in: ${error.message}`);
      }
    }
  },
};
