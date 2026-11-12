// What an addon's main.js can use inside rex-addons 0.8, for your editor: autocomplete and, with
// `// @ts-check` at the top of main.js, a warning where code uses something that isn't there.
// Plain JavaScript (ES2022) plus the globals below, and nothing else: no DOM, no Node, no timers.
// The guide: https://github.com/debridmediamanager/rex-addons/blob/main/docs/writing-addons.md
/// <reference no-default-lib="true" />
/// <reference lib="es2022" />

// ---------------------------------------------------------------------------------------------
// The default export
// ---------------------------------------------------------------------------------------------

type Awaitable<T> = T | Promise<T>;

/**
 * What `main.js` exports by default. Put `/** @type {Addon} *\/` on the line above
 * `export default {` and your editor checks it.
 *
 * `Config` types `ctx.config`, such as `Addon<{count: number}>`.
 */
interface Addon<Config = AddonConfig> {
  /**
   * Every title you have, as a list, every time. Throw rather than return half a list. 30 s of
   * JavaScript, 20 minutes in all. Throw an error whose message starts with `RETRY_AFTER <seconds>`
   * (`RETRY_AFTER 3600: answered 429`) and the next try waits that long, 1 minute to 24 hours,
   * instead of 5 minutes (rex-addons 0.6). With `"host": "0.8"`, a page of them (`{items, next}`)
   * or, when `ctx.full` is false, only what changed (`{partial: true, items, removed}`).
   */
  items(ctx: ItemsContext<Config>): Awaitable<Item[] | ItemsPage | ItemsChanges>;
  /** Where one title plays from. The answer is reused for 10 minutes. 10 s of JavaScript, 90 s in all. */
  open(id: string, ctx: AddonContext<Config>): Awaitable<Opened>;
  /** With `sign_in` (kind `code`): starts a sign-in and returns the code the person types. Needs `"host": "0.3"`. */
  signIn?(ctx: AddonContext<Config>): Awaitable<SignInStart>;
  /** With `sign_in` (kind `code`): called every `interval` seconds with `signIn()`'s `state`. Throw when the website refused. */
  signInPoll?(state: any, ctx: AddonContext<Config>): Awaitable<SignInPoll>;
  /** Optional: forget your tokens, and revoke them if the website lets you. */
  signOut?(ctx: AddonContext<Config>): Awaitable<void>;
  /** The functions the manifest's `exports` names, for `addons.call`: called with the caller's arguments, then `ctx`. */
  [exported: string]: unknown;
}

/** What every call gets as its last argument. */
interface AddonContext<Config = AddonConfig> {
  /** The settings, each one its saved value or its default (`null` with neither). */
  config: Config;
  /** This rex-addons' version, such as `"0.8.0"`. rex-addons 0.6. */
  host: string;
  /** Which addon this is, from its manifest. rex-addons 0.6. */
  addon: { id: string; version: string };
}

/** What `items()` gets: also why this sync runs. */
interface ItemsContext<Config = AddonConfig> extends AddonContext<Config> {
  /**
   * Why this sync runs (rex-addons 0.6): `schedule`, `refresh_minutes` passed; `retry`, the last
   * failed, or an addon this one requires was just signed in or enabled; `manual`, a person asked
   * (a sync from the app, enabling it, restoring deleted titles, `rex-addons check`); `settings`,
   * they changed; `install`, installed or updated and not synced since; `sign_in`, just signed in.
   */
  reason: "schedule" | "retry" | "manual" | "settings" | "install" | "sign_in";
  /**
   * Whether this sync needs every title (rex-addons 0.8): a partial answer then fails it. False
   * only after a sync that worked, and then everything the last `items()` returned reached rex.
   */
  full: boolean;
  /** When the last sync that worked began, in Unix seconds, or `null` (rex-addons 0.8). */
  last_sync: number | null;
  /** The `next` of the page before; absent on the first page (rex-addons 0.8). */
  cursor?: any;
}

/**
 * A page of every title (`"host": "0.8"`). While `next` is set (anything but `null`), `items()` is
 * called again with it as `ctx.cursor`; all pages together are every title.
 */
interface ItemsPage {
  items: Item[];
  next?: any;
  partial?: false;
}

/**
 * Only what changed since the last sync, when `ctx.full` is false (`"host": "0.8"`): the new or
 * changed titles, whole, and the ids that are gone. May have `next`, every page partial.
 */
interface ItemsChanges {
  partial: true;
  items: Item[];
  removed?: string[];
  next?: any;
}

/** A setting's value by its `type`: `text`, `secret` and `select` a string (a `select` one of its options' values), `host` the address as the person typed it, whose host and port your addon may reach (rex-addons 0.7), `bool` a boolean, `number` a number, `list` a list of strings. */
type AddonConfig = { [key: string]: any };

/** What `open()` returns. The link must be on your `hosts`. */
type Opened = OpenedLink | Playlists;

interface OpenedLink {
  /** An `http(s)` link on your `hosts`: rex is sent straight there. */
  url: string;
  /** For a link that needs headers (a `Referer`, a token): rex-addons then passes the bytes through itself, `Range` and all. */
  headers?: Record<string, string>;
}

/** HLS playlists you wrote. Needs `"host": "0.4"`. At most 8 MB of playlists in all. */
interface Playlists {
  /** The master playlist's text, starting with `#EXTM3U`. */
  playlist: string;
  /** The media playlists it names, each by a plain name: letters, digits, `-` and `_`, at most 64 of them, ending in `.m3u8` (not `master.m3u8`). Leave it out when the master names only links. Every link in them an absolute `http(s)` link on your `hosts`. */
  playlists?: Record<string, string>;
}

/** What `signIn()` returns. */
interface SignInStart {
  /** The code the person types. Don't print it. */
  code: string;
  /** The https page they type it on. */
  url: string;
  /** How many seconds the code lives, at most 3600. */
  expires_in: number;
  /** How many seconds to wait between checks, 2 to 60. */
  interval: number;
  /** Any JSON you need to finish, handed to `signInPoll()`. The app never sees it and it is never logged. */
  state?: any;
}

/** What `signInPoll()` returns: `{done: false}` while waiting, `{done: true, account}` once approved, your tokens in `storage` by then. */
type SignInPoll = { done: false } | { done: true; account: string };

// ---------------------------------------------------------------------------------------------
// An item: a rex feed item (rex's docs/feed-api.md, "An item")
// ---------------------------------------------------------------------------------------------

/** One title `items()` returns: a rex feed item. */
interface Item {
  /** Your key for the title, for good. Watch history hangs on it. */
  id: string;
  /** A relative path ending in a video extension (`.m3u8` for playlists you write), such as `my-films/his-girl-friday/film.mp4`. No leading `/`, no empty, `.` or `..` part. */
  path: string;
  /** What the title is: `movie` or `episode` places it by its `ids`; `track` is music, `photo` a picture; `extra` a clip of its release's title; `unplaced` takes back a placement. Anything else places nothing. */
  kind?: "movie" | "episode" | "extra" | "track" | "photo" | "unplaced" | (string & {});
  /** The movie's or the **show's** ids, never an episode's. For a track, its album's `mbid` or `plex`. */
  ids?: ItemIds;
  /** ffprobe's output, as `ffprobe()` returns it. Without it rex holds the title: it shows nowhere. */
  media_info?: Probe | null;
  /** What the title is called and looks like, for what Plex doesn't know. */
  display?: Display;
  /** The release name, shown as is for an unplaced episode's show. Default: the folder above the file. */
  name?: string;
  /** With `kind: "episode"`: the season on TMDB's numbering (0 is Specials). */
  season?: number;
  /** With `kind: "episode"`: the episode on TMDB's numbering. */
  episode?: number;
  /** The last episode of a file holding several (`E01E02` -> 2). */
  episode_end?: number;
  /** With `kind: "episode"`: TMDB's id of the episode itself. */
  tmdb_episode?: number;
  /** With `kind: "episode"`: which part of its episode the file is, from 1. */
  part?: number;
  /** With `kind: "extra"`: the clip's type. Anything else is `other`. */
  extra_type?: "trailer" | "deleted" | "featurette" | "behindthescenes" | "interview" | "scene" | "short" | "opening" | "ending" | "other" | (string & {});
  /** Groups a release's files (a season's episodes, a track's album). Default: the folder above `path`. */
  release?: string;
  /** A movie's edition label. `""` clears it. */
  edition_title?: string;
  /** Bytes. Default: `media_info.format.size`. */
  size?: number;
  /** The path clients see in Get Info. */
  file?: string;
  /** A library's section key, or `movie`/`show`. */
  library?: string | number;
  /** The folder the file is listed in inside its release, `""` at its top level. */
  folder?: string;
  /** The probe's digest. */
  probe?: string;
  /** The release's content/info hash. */
  hash?: string;
  /** Subtitle files beside it. Absent or `null` keeps the title's, `[]` removes them. */
  subtitles?: Subtitle[] | null;
  /** The collections its title is in. Absent or `null` keeps them, `[]` takes it out of every one. */
  collections?: string[] | null;
  /** The playlists it is in, by title, and its place in each from 1: `{"My Mix": 3}`. `{}` takes it out of every one. */
  playlists?: Record<string, number> | null;
  /** Only on a live channel (an `.m3u8` whose probe has no duration): its place in Live TV. `null` ends the channel. */
  channel?: Channel | null;
  /** With `kind: "track"`: the disc, default 1. */
  disc?: number;
  /** With `kind: "track"`: its number on the album. */
  track?: number;
  /** With `kind: "track"`: `.lrc` or `.txt` lyrics files beside it. */
  lyrics?: { path: string; size?: number }[] | null;
  /** With `kind: "track"`: a loudness analysis, in PMS's names (`gain`, `peak`, `levels` and so on). */
  loudness?: { [field: string]: unknown };
  /** The rest of rex's feed item: https://github.com/debridmediamanager/rex/blob/main/docs/feed-api.md#an-item */
  [field: string]: unknown;
}

interface ItemIds {
  /** IMDb's id, `tt…`. */
  imdb?: string;
  tmdb?: number | string;
  tvdb?: number | string;
  /** For a track: MusicBrainz's release id of its album. */
  mbid?: string;
  /** For a track: Plex's album guid, `plex://album/<id>`. */
  plex?: string;
  /** For a track nothing else names: MusicBrainz's artist id. */
  artist_mbid?: string;
  [id: string]: unknown;
}

/** Every field is optional. Artwork is an `http(s)` link; a field of the wrong type rejects the item. */
interface Display {
  title?: string;
  summary?: string;
  /** `YYYY-MM-DD`, or `YYYY` alone. */
  date?: string;
  /** A film's poster, an episode's still. */
  thumb?: string;
  /** A film's background. */
  art?: string;
  /** An episode's show. */
  show?: { title?: string; summary?: string; poster?: string; art?: string };
  /** An episode's season. */
  season?: { summary?: string; poster?: string };
  /** A track's artist. */
  artist?: { title?: string; summary?: string; poster?: string; art?: string };
  /** A track's album. */
  album?: { title?: string; summary?: string; poster?: string; date?: string };
}

interface Subtitle {
  /** Where its text is, under the addon's own paths. Its extension is its format: `srt`, `ass`, `ssa`, `smi`, `txt`, `psb` or `idx`. */
  path: string;
  /** ISO 639-1 (`en`) or 639-2 (`eng`). */
  language?: string;
  forced?: boolean;
  hearing_impaired?: boolean;
  /** Bytes. */
  size?: number;
  /** With an `.idx`: the VobSub's `.sub`. */
  sub?: string;
  sub_size?: number;
}

interface Channel {
  /** Its number in the guide and its key (`2.1`, `704`): letters, digits, `.`, `-` and `_`. */
  number: string;
  title?: string;
  call_sign?: string;
  guide?: Programme[];
}

interface Programme {
  /** Epoch seconds, or ISO 8601. */
  start: number | string;
  /** Epoch seconds, or ISO 8601; after `start`. */
  end: number | string;
  title: string;
  summary?: string;
  /** `episode`, with `show`, `season` and `episode`. */
  type?: string;
  show?: string;
  season?: number;
  episode?: number;
  categories?: string[];
  /** An `http(s)` link to its picture. */
  thumb?: string;
  previously_shown?: boolean;
}

/** ffprobe's JSON of a file (`format`, `streams`, `chapters`), cut down here to the common fields. */
interface Probe {
  format?: {
    format_name?: string;
    /** Seconds, as text. None for a live stream. */
    duration?: string;
    size?: string;
    bit_rate?: string;
    tags?: Record<string, string>;
    [field: string]: unknown;
  };
  streams?: ProbeStream[];
  chapters?: { id?: number; start_time?: string; end_time?: string; tags?: Record<string, string>; [field: string]: unknown }[];
  [field: string]: unknown;
}

interface ProbeStream {
  index: number;
  codec_type?: string;
  codec_name?: string;
  width?: number;
  height?: number;
  channels?: number;
  tags?: Record<string, string>;
  [field: string]: unknown;
}

// ---------------------------------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------------------------------

type FetchMethod = "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE";

interface FetchInit {
  /** `GET` by default. */
  method?: FetchMethod | Lowercase<FetchMethod>;
  /** Sent as given. `User-Agent: rex-addons/<version>` unless you set your own. */
  headers?: Record<string, string>;
  body?: string | ArrayBuffer | ArrayBufferView;
}

interface FetchResponse {
  readonly status: number;
  /** `status` is 200 to 299. */
  readonly ok: boolean;
  /** A plain object with lowercase names. */
  readonly headers: Readonly<Record<string, string>>;
  /** Where redirects ended. */
  readonly url: string;
  /** The body as text. Read once: `text()`, `json()` or `bytes()`. Up to 16 MB. */
  text(): Promise<string>;
  /** The body as JSON. Read once. Up to 16 MB. */
  json(): Promise<any>;
  /** The body's bytes. Read once. Up to 16 MB. */
  bytes(): Promise<Uint8Array>;
}

/**
 * Fetches a link on your `hosts`; every redirect (up to 9) is checked again. The body is read
 * only when you call `text()`, `json()` or `bytes()`. Fetches run one at a time: `Promise.all`
 * over several waits for each in turn. At most 32 responses can wait with their body unread.
 */
declare function fetch(url: string | URL, init?: FetchInit): Promise<FetchResponse>;

/**
 * ffprobe's JSON of a video, for `media_info`: a link on your `hosts` (an HLS playlist works too),
 * or `{playlist, playlists}` you wrote (`"host": "0.4"`). Up to 2 minutes, which don't count as
 * JavaScript time. Fails when rex-addons runs without `--ffprobe`.
 */
declare function ffprobe(target: string | URL | Playlists, init?: { headers?: Record<string, string> }): Promise<Probe>;

/** Into rex-addons' log, tagged `[addon <id>]`, secret settings masked. */
declare const console: {
  readonly log: (...data: any[]) => void;
  readonly info: (...data: any[]) => void;
  readonly debug: (...data: any[]) => void;
  /** A line starting with `WARN`. */
  readonly warn: (...data: any[]) => void;
  /** A line starting with `ERROR`. */
  readonly error: (...data: any[]) => void;
};

/** Your addon's own saved values, any JSON. They survive restarts and updates. 16 MB in all. */
declare const storage: {
  /** `undefined` for a key never set. */
  readonly get: (key: string) => any;
  /** Setting `undefined` deletes the key. */
  readonly set: (key: string, value: any) => void;
  readonly delete: (key: string) => void;
  /** Every key, sorted; with `prefix`, only those starting with it. rex-addons 0.6. */
  readonly keys: (prefix?: string) => string[];
};

/** Waits, for backing off on a 429. Doesn't count as JavaScript time; a sleep past the end of the call fails at once. */
declare function sleep(ms: number): Promise<void>;

/** Base64 to text whose characters are bytes (0 to 255). rex-addons 0.2. */
declare function atob(data: string): string;
/** Text whose characters are bytes (0 to 255) to base64. rex-addons 0.2. */
declare function btoa(data: string): string;

/** `md5` is rex-addons 0.6's. */
type HashAlgorithm = "md5" | "sha1" | "sha256" | "sha512";
/** Text, taken as UTF-8, or bytes (rex-addons 0.6). */
type HashInput = string | ArrayBuffer | ArrayBufferView;
/** A digest of `data` in lowercase hex. rex-addons 0.2. */
declare function hash(algorithm: HashAlgorithm, data: HashInput): string;
/** An HMAC of `data` with `key`, in lowercase hex. rex-addons 0.2. */
declare function hmac(algorithm: HashAlgorithm, key: HashInput, data: HashInput): string;

/** Text to UTF-8 bytes, as in browsers. rex-addons 0.6. */
declare class TextEncoder {
  readonly encoding: "utf-8";
  encode(text?: string): Uint8Array;
}

/** UTF-8 bytes to text, as in browsers; bytes that aren't UTF-8 become U+FFFD. UTF-8 only: another label throws. rex-addons 0.6. */
declare class TextDecoder {
  constructor(label?: "utf-8" | "utf8" | "unicode-1-1-utf-8");
  readonly encoding: "utf-8";
  decode(bytes?: ArrayBuffer | ArrayBufferView): string;
}

/** Random numbers, as in browsers. No `subtle`. rex-addons 0.6. */
declare const crypto: {
  /** Fills an integer typed array, at most 65,536 bytes, and returns it. */
  readonly getRandomValues: <T extends Int8Array | Uint8Array | Uint8ClampedArray | Int16Array | Uint16Array | Int32Array | Uint32Array | BigInt64Array | BigUint64Array>(array: T) => T;
  /** A random version 4 UUID, lowercase. */
  readonly randomUUID: () => string;
};

/** A link, as in browsers but smaller. rex-addons 0.2. */
declare class URL {
  /** Resolves `url` against `base` when given. Throws `Invalid URL: ...`. */
  constructor(url: string | URL, base?: string | URL);
  href: string;
  readonly origin: string;
  protocol: string;
  username: string;
  password: string;
  host: string;
  hostname: string;
  port: string;
  pathname: string;
  search: string;
  hash: string;
  /** Changing it changes the URL. */
  readonly searchParams: URLSearchParams;
  toString(): string;
  toJSON(): string;
}

/** A query string, as in browsers but smaller. rex-addons 0.2. */
declare class URLSearchParams {
  constructor(init?: string | Record<string, string | number | boolean> | Iterable<readonly [string, string]>);
  get(name: string): string | null;
  getAll(name: string): string[];
  has(name: string): boolean;
  append(name: string, value: string | number | boolean): void;
  set(name: string, value: string | number | boolean): void;
  delete(name: string): void;
  toString(): string;
  entries(): IterableIterator<[string, string]>;
  keys(): IterableIterator<string>;
  values(): IterableIterator<string>;
  [Symbol.iterator](): IterableIterator<[string, string]>;
}

/**
 * Only with `"sign_in": {"kind": "spotify"}` (rex-addons 0.5): the person's Spotify, through
 * Spotify's own undocumented API. A failure throws; one starting with `SIGN_IN_REQUIRED` signs
 * the addon out if you let it through.
 */
declare const spotify: {
  /** The account's Spotify username. */
  readonly user: () => string;
  /** `GET` a path starting with `/`, answered as JSON. */
  readonly get: (path: string) => Promise<any>;
  /** `POST` `body` as JSON to a path starting with `/`, answered as JSON. */
  readonly post: (path: string, body?: any) => Promise<any>;
};

/** Only with `requires` (rex-addons 0.5): other addons' exported functions. */
declare const addons: {
  /**
   * Calls the function `name` that addon `id` exports, in that addon's own sandbox. Arguments
   * and the result are JSON. What it throws, you get thrown. One level: it cannot call a third.
   */
  readonly call: (id: string, name: string, ...args: any[]) => Promise<any>;
};
