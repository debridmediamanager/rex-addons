# Writing an addon

An addon is a folder with two files: `manifest.json`, which says what it is, and `main.js`, which
says what it has and where each title plays from. rex-addons does the rest: it registers the
addon with rex, sends rex the list, serves the bytes and keeps it all up to date. People install
addons from a git repository, so sharing yours means putting it in one ([below](#a-repository-of-addons)).

You write plain JavaScript (an ES module). There's no npm and no build step.

## manifest.json

```json
{
  "id": "my-films",
  "name": "My films",
  "version": "1.0.0",
  "description": "Two public domain classics from the Internet Archive.",
  "hosts": ["archive.org", "*.archive.org"],
  "config": [
    {"key": "pawnshop", "label": "Include The Pawnshop", "type": "bool", "default": true}
  ],
  "refresh_minutes": 1440
}
```

| Field | Meaning |
|---|---|
| `id` | Lowercase letters, digits and `-`. It's the addon's name in rex too (`addon-my-films`), so never change it |
| `name`, `description` | What people see in the app |
| `version` | Numbers and dots. Raise it with every change: people see an update when the version in your repository is higher than theirs |
| `hosts` | The websites `fetch()` may reach, and that `open()` links may point at: `example.com` exactly, `*.example.com` for anything under it, or `*` for any public website. Addresses on the device or the home network are never reachable through `*` |
| `config` | The settings the app shows. Each has a `key`, a `label`, a `type` (`text`, `secret`, `bool`, `number` or `list`, a list of text), and optionally a `default` and `required: true`. A `secret` is shown masked; a `list` with `secret: true` shows only each link's website, for links that carry a key |
| `refresh_minutes` | How often `items()` runs. Default 360, at least 5 |
| `host` | Optional: the lowest rex-addons version your addon needs, numbers and dots, such as `"0.2"`. From 0.2 on, an older rex-addons refuses to install it and greys it out in a repository's list (0.1 doesn't know the field). Using `sleep`, `atob`, `btoa`, `URL`, `URLSearchParams`, `hash`, `hmac`, `bytes()` or a binary `body` needs `"host": "0.2"`; `sign_in` needs `"host": "0.3"`; playlists from `open()` or for `ffprobe()` need `"host": "0.4"` |
| `directories` | Optional: libraries of the addon's own, in rex's [`directories`](https://github.com/debridmediamanager/rex/blob/main/docs/feed-api.md#libraries-a-source-declares) shape, such as `[{"name": "youtube", "shows": "YouTube"}]`: titles whose `path` starts with `youtube/` go to a TV library called YouTube instead of your Movies or TV Shows. rex-addons sends it when the addon registers |
| `sign_in` | Optional: `{"label": "Sign in to Example"}` for an addon that needs the person's account on a website ([below](#signing-in-with-a-code)). `label` is what the app's button says, `Sign in to <name>` without it |

## main.js

```js
export default {
  async items(ctx) { /* every title you have, as a list */ },
  async open(id, ctx) { /* where one title plays from: { url, headers? } */ },
};
```

`ctx.config` holds the settings, each one its saved value or its default.

### items(ctx)

Returns everything the addon has, every time. rex takes what's gone out of the library, so
**throw an error rather than return half a list** when a website doesn't answer: the sync then
fails, shows the error in the app, and rex keeps what it had. As a second guard, rex refuses a
sync that would take away more than half of the titles.

Each title is a rex feed item. The fields that matter most:

| Field | Meaning |
|---|---|
| `id` | Your key for the title, **for good**. Watch history hangs on it |
| `path` | A relative path ending in a video extension, such as `my-films/his-girl-friday/film.mp4`. It names the file in Plex's Get Info, and its folder groups a release. rex-addons sets the extension to what ffprobe finds, so `.mp4` vs `.mkv` doesn't need to be right |
| `kind`, `ids` | `"movie"` with `{"imdb": "tt…"}` or `{"tmdb": 123}`, and Plex names and pictures it. `"episode"` with `season` and `episode` and the **show's** ids for an episode |
| `display` | `title`, `summary`, `date` (`YYYY-MM-DD` or `YYYY`), `thumb` (a poster URL), `art`. Shown for what Plex doesn't know. For an episode, also `show: {title, summary, poster, art}` |
| `name` | The release name, shown as is for an unplaced episode's show |

| `media_info` | What's in the file: ffprobe's output, as `ffprobe()` returns it. rex shows nothing it hasn't been told this about |

The rest (`release`, `subtitles`, `collections`, `channel` for live TV, music `track`s and so on)
is in rex's [feed API](https://github.com/debridmediamanager/rex/blob/main/docs/feed-api.md#an-item).

Probe each title yourself with `ffprobe()` and keep the answer in `storage`: a file doesn't
change, so once is enough. A title that arrives without `media_info` is probed by rex-addons
through your `open()` instead, and that probe is remembered by the title's `id`.

### open(id, ctx)

Called when rex reads a title, and once before that when it's checked. Return where it plays from:

- `{ url }`: rex is sent straight there.
- `{ url, headers }`: for a link that needs headers (a `Referer`, a token). rex-addons then
  passes the bytes through itself, `Range` and all.
- `{ playlist, playlists }`: an HLS presentation you wrote, for a title made of files a player
  reads together, such as a picture and its sound kept apart ([below](#playlists-you-write)).
  Needs `"host": "0.4"`.

The link must be on one of your `hosts`. An answer is reused for 10 minutes, so `open()` isn't
called for every seek.

### Playlists you write

`playlist` is the text of an HLS master playlist, and `playlists` the media playlists it names,
each by a plain name (`video.m3u8`: letters, digits, `-` and `_`, ending in `.m3u8`):

```js
return {
  playlist: '#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",NAME="Original",DEFAULT=YES,URI="audio.m3u8"\n' +
    '#EXT-X-STREAM-INF:BANDWIDTH=3400000,CODECS="avc1.640028,mp4a.40.2",AUDIO="a"\nvideo.m3u8\n',
  playlists: { "video.m3u8": videoPlaylist, "audio.m3u8": audioPlaylist },
};
```

rex-addons serves the master at the title's own path and each media playlist under it, so rex
(FFmpeg, really) reads the media straight from the links in them; nothing passes through
rex-addons. Every link in them must be an absolute `http(s)` link on your `hosts`; the master may
also name your media playlists. Anything else (a relative link, another scheme, a name you did
not give) fails the play with a clear message, as do more than 8 MB of playlists in all. Give
the title a path ending in `.m3u8` and probe it by passing the same object to `ffprobe()`. rex
plays such a title through its conversion, copying what it can. The
[YouTube addon](../addons/youtube/main.js) writes one for each video, its segments byte ranges
(`EXT-X-BYTERANGE`) of YouTube's files.

## What your code can use

Plain JavaScript, plus these:

| Global | What it does |
|---|---|
| `fetch(url, {method, headers, body})` | Returns `{status, ok, headers, url, text(), json(), bytes()}`, `bytes()` giving a `Uint8Array`. `headers` is a plain object with lowercase names, `url` is where redirects ended. Only `hosts` are reachable, and every redirect is checked again. `body` is text, a `Uint8Array` or an `ArrayBuffer`. The body is read only when you call `text()`, `json()` or `bytes()`, once, so a `Range: bytes=0-0` check costs nothing. Fetches run one at a time: `Promise.all` over several waits for each in turn |
| `ffprobe(url, {headers})` | ffprobe's JSON of a video (`format`, `streams`, `chapters`), for `media_info`. rex-addons reads the link itself, for ffprobe, so the same `hosts` rule applies as for `fetch`; an HLS playlist works too. `ffprobe({playlist, playlists})` probes [playlists you wrote](#playlists-you-write), checked as `open()`'s are (`"host": "0.4"`). It fails with a clear message when rex-addons runs without ffprobe. Up to 2 minutes, which don't count as JavaScript time |
| `console.log`, `console.warn`, `console.error` | Into rex-addons' log, tagged `[addon <id>]`, with your secret settings' values (6 characters or more) shown as `********` |
| `storage.get(key)`, `storage.set(key, value)`, `storage.delete(key)` | Your addon's own saved values, any JSON. They survive restarts and updates |
| `atob(text)`, `btoa(text)` | Base64, as in browsers: text whose characters are bytes (0 to 255) |
| `new URL(link, base)`, `URLSearchParams` | Links, as in browsers but smaller: `new URL(link, base)` resolves a relative link, a URL has `href`, `protocol`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`, `origin` and `searchParams`, and `URLSearchParams` has `get`, `getAll`, `has`, `set`, `append`, `delete`, `toString` and `entries`. Changing `searchParams` changes the URL |
| `hash(algorithm, text)`, `hmac(algorithm, key, text)` | A digest of the text (as UTF-8) in lowercase hex, for signing an API request. `algorithm` is `sha1`, `sha256` or `sha512`; there's no `md5` |
| `await sleep(ms)` | Waits, for backing off when a website answers 429. The wait doesn't count as JavaScript time, and a sleep past the end of the call fails at once |

There's nothing else: no timers (`setTimeout`, use `sleep`), no `require` or `import`, no
`TextEncoder`, no file access. Each call starts fresh, so keep anything you want to remember
across calls in `storage`.

Limits, per call:

| | |
|---|---|
| Memory | 64 MB |
| JavaScript time | 30 s for `items()`, 10 s for `open()`. Time spent waiting on `fetch`, `ffprobe` or `sleep` doesn't count |
| Whole call | 20 minutes for `items()`, 90 s for `open()`. A `fetch` or body read still waiting then fails |

`signIn()`, `signInPoll()` and `signOut()` get `open()`'s limits.
| A fetched body | 16 MB |
| `storage` | 16 MB in all, written to disk when the call ends |

## A complete example

Two public domain films from the Internet Archive. One has an IMDb id, so Plex names it; the
other is named by `display`. Each is probed once and the probe kept. The manifest is the one
above. The [internet-archive addon](../addons/internet-archive/main.js) is the same idea, grown up.

```js
const FILMS = [
  { id: "his-girl-friday", item: "his_girl_friday", file: "his_girl_friday.mp4", imdb: "tt0032599" },
  {
    id: "the-pawnshop", item: "CC_1916_10_02_ThePawnshop", file: "CC_1916_10_02_ThePawnshop_512kb.mp4",
    display: { title: "The Pawnshop", date: "1916", summary: "Charlie Chaplin works in a pawnshop." },
  },
];

export default {
  async items(ctx) {
    // A fetch, to show how: make sure archive.org answers before listing anything.
    const answer = await fetch("https://archive.org/metadata/his_girl_friday/metadata/title");
    if (!answer.ok) throw new Error(`archive.org answered ${answer.status}`);
    console.log(`archive.org has ${(await answer.json()).result}`);

    const items = [];
    for (const film of FILMS.filter((one) => ctx.config.pawnshop || one.id !== "the-pawnshop")) {
      let probe = storage.get(`probe:${film.id}`);
      if (!probe) {
        probe = await ffprobe(link(film));
        storage.set(`probe:${film.id}`, probe);
      }
      items.push({
        id: film.id,
        path: `my-films/${film.id}/${film.file}`,
        ...(film.imdb ? { kind: "movie", ids: { imdb: film.imdb } } : {}),
        display: { thumb: `https://archive.org/services/img/${film.item}`, ...film.display },
        media_info: probe,
      });
    }
    return items;
  },

  async open(id) {
    const film = FILMS.find((one) => one.id === id);
    if (!film) throw new Error(`no film ${id}`);
    return { url: link(film) };
  },
};

function link(film) {
  return `https://archive.org/download/${film.item}/${film.file}`;
}
```

## Signing in with a code

For an addon that needs the person's account on a website. rex-addons runs on TVs with only a
remote, so nobody types a password: the app shows a short code, the person enters it on their
phone at the website's address, and the addon is signed in. That's OAuth's device flow, which
Google, Microsoft, GitHub and many others offer.

The manifest says so, and that it needs rex-addons 0.3:

```json
"host": "0.3",
"sign_in": {"label": "Sign in to Example"}
```

`main.js` then exports two more functions, and optionally a third. Installing refuses an addon
with `sign_in` that lacks `signIn` or `signInPoll`.

| Function | What it does |
|---|---|
| `async signIn(ctx)` | Starts a sign-in. Returns `{code, url, expires_in, interval, state}`: the code the person types, the https page they type it on, how many seconds the code lives (at most 3600) and how many to wait between checks (2 to 60), and `state`, any JSON you need to finish. `state` stays inside rex-addons: the app never sees it and it is never logged |
| `async signInPoll(state, ctx)` | Called every `interval` seconds with that `state`. Return `{done: false}` while the person hasn't entered the code, and `{done: true, account: "<name to show>"}` once they approved, with your tokens kept in `storage` by then. Throw when the website refused: your message is shown |
| `async signOut(ctx)` | Optional. Forget your tokens, and revoke them if the website lets you |

How it goes:

- Until it is signed in, the addon doesn't sync (`items()` isn't called) and its titles don't
  play. Signing in syncs it right away.
- A new sign-in replaces one still waiting. One whose code expires ends with
  `the code expired before it was entered`.
- When the website ends the session (a refresh token revoked or expired), throw an error from
  `items()` or `open()` whose message starts with `SIGN_IN_REQUIRED`, such as
  `SIGN_IN_REQUIRED: Example signed you out`. The addon is signed out, the rest of the message is
  shown (or `signed out by the website`), and its titles stay in rex until it is signed in again.
- Signing out calls `signOut()` and forgets the account. Titles, settings and `storage` stay,
  so `signOut()` is where you delete your tokens. Updating the addon keeps it signed in.
- The code is masked in the log while the sign-in waits, but not yet inside `signIn()`: don't
  print it there. Never print `state` or your tokens.

A generic device flow ([RFC 8628](https://www.rfc-editor.org/rfc/rfc8628)), with the access token
refreshed when it expires. Its `hosts` are `auth.example.com`, `api.example.com` and wherever
`stream_url` points.

```js
const AUTH = "https://auth.example.com";
const API = "https://api.example.com";
const CLIENT = "rex-addons-example";

async function post(url, fields) {
  const body = new URLSearchParams(fields).toString();
  return await (await fetch(url, { method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" }, body })).json();
}

function keep(tokens) {
  storage.set("tokens", { access: tokens.access_token, refresh: tokens.refresh_token ?? storage.get("tokens")?.refresh });
}

// A request with the access token, refreshed once when the API says it expired.
async function api(path) {
  for (let tries = 0; tries < 2; tries++) {
    const tokens = storage.get("tokens");
    if (!tokens) throw new Error("SIGN_IN_REQUIRED");
    const answer = await fetch(`${API}${path}`, { headers: { Authorization: `Bearer ${tokens.access}` } });
    if (answer.status !== 401) {
      if (!answer.ok) throw new Error(`${path} answered ${answer.status}`);
      return await answer.json();
    }
    const refreshed = await post(`${AUTH}/token`, { client_id: CLIENT, grant_type: "refresh_token", refresh_token: tokens.refresh });
    if (refreshed.error) throw new Error(`SIGN_IN_REQUIRED: ${refreshed.error_description ?? refreshed.error}`);
    keep(refreshed);
  }
  throw new Error(`${path} keeps answering 401`);
}

export default {
  async items(ctx) {
    const videos = await api("/me/videos");
    return videos.map((video) => ({ id: video.id, path: `example/${video.id}/video.mp4`, display: { title: video.title } }));
  },

  async open(id) {
    return { url: (await api(`/videos/${id}`)).stream_url };
  },

  async signIn(ctx) {
    const answer = await post(`${AUTH}/device/code`, { client_id: CLIENT, scope: "videos.read" });
    if (answer.error) throw new Error(answer.error_description ?? answer.error);
    return {
      code: answer.user_code, url: answer.verification_uri,
      expires_in: answer.expires_in, interval: answer.interval ?? 5,
      state: { device_code: answer.device_code },
    };
  },

  async signInPoll(state, ctx) {
    const answer = await post(`${AUTH}/token`, {
      client_id: CLIENT, device_code: state.device_code, grant_type: "urn:ietf:params:oauth:grant-type:device_code",
    });
    if (answer.error === "authorization_pending" || answer.error === "slow_down") return { done: false };
    if (answer.error === "access_denied") throw new Error("you said no on the website");
    if (answer.error) throw new Error(answer.error_description ?? answer.error);
    keep(answer);
    return { done: true, account: (await api("/me")).email };
  },

  async signOut(ctx) {
    storage.delete("tokens");
  },
};
```

## A repository of addons

A repository is any public git repository people can reach over HTTPS: GitHub, GitLab, Codeberg
or a server of your own. Every folder in it holding a `manifest.json` and a `main.js` is an
addon, up to three folders down; folders whose names start with `.` are skipped. This one is
laid out like that:

```
repository.json                      optional: {"name": "...", "description": "..."}
addons/internet-archive/manifest.json
addons/internet-archive/main.js
addons/stremio/manifest.json
addons/stremio/main.js
```

Without `repository.json` the repository is named after its address. People add it by that
address (`https://codeberg.org/you/my-addons`, or `...#branch` for another branch), pick your
addons from its list and install them. rex-addons fetches only the newest commit, and only what
it needs: at most 2,000 files and 64 MB, never a symbolic link or submodule. To ship a change,
raise the addon's `version` and push; people see the update when they refresh the repository.

## Trying it

While you work on an addon, skip git: start rex-addons with `--bundled` naming a folder that holds
your addon's folder. It's installed on start, and installed again on a start after you raise its
`version`:

```bash
rex-addons --data ~/rex-addons-dev --ffprobe "$(command -v ffprobe)" --bundled ~/my-addons
curl http://127.0.0.1:32410/api/status
curl -X POST http://127.0.0.1:32410/api/addons/my-films/sync
```

Installing checks that `main.js` loads and exports `items` and `open`, and a sync runs right
away. The log shows what your addon printed and why a sync failed, and
`curl http://127.0.0.1:32400/rex/feed/addon-my-films/items` shows what rex holds. Once it works,
push it to your repository and add that in the app.
