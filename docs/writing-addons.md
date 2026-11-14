# Writing an addon

An addon is a folder with two files: `manifest.json`, which says what it is, and `main.js`, which
says what it has and where each title plays from. rex-addons does the rest: it registers the
addon with rex, sends rex the list, serves the bytes and keeps it all up to date. People install
addons from a git repository, so sharing yours means putting it in one ([below](#a-repository-of-addons)).

You write plain JavaScript (an ES module). There's no npm and no build step, and your editor can
still check it ([editor help](#editor-help)).

This page is about addons that run inside rex-addons. A program of your own, in any language,
that talks to rex's feed API directly is a rex addon too: see
[Building rex addons](https://notes.debridmediamanager.com/rex/addons/).

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
| `id` | Lowercase letters, digits and `-`, starting with a letter or digit, at most 64 characters. It's the addon's name in rex too (`addon-my-films`), so never change it |
| `name`, `description` | What people see in the app |
| `version` | Numbers and dots. Raise it with every change: people see an update when the version in your repository is higher than theirs |
| `hosts` | The websites `fetch()` may reach, and that `open()` links may point at: `example.com` exactly, `*.example.com` for anything under it, or `*` for any public website. This device (`127.0.0.1`, `localhost`) is never reachable. An address on a private network (a NAS at `192.168.1.20`, a Tailscale address) is reachable only when you list it exactly, by its address or by a name that leads to it, or when the person types it into a `host` setting ([below](#a-server-on-your-home-network)); never through `*` or `*.example.com` |
| `config` | The settings the app shows. Each has a `key` (letters, digits and `_`, each key once), a `label`, a `type` (`text`, `secret`, `bool`, `number`, `list`, a list of text, `select`, one of its `options`, or `host`, an address the person types, which your addon may then reach: [below](#a-server-on-your-home-network)), and optionally a `default` (never for a `host`), `required: true` and `help`, a line of text the app shows with it. A `secret` is shown masked; a `list` with `secret: true` shows only each link's website, for links that carry a key. A `select` has `options`, such as `[{"value": "1080", "label": "1080p"}, {"value": "720", "label": "720p"}]`: each `value` is text, given once, and its `label` is what the app shows. Your code gets the `value`, always one of them: one a newer version of your addon drops gives the `default` |
| `refresh_minutes` | How often `items()` runs. Default 360, at least 5 |
| `host` | Optional: the lowest rex-addons version your addon needs, such as `"0.2"` ([below](#which-rex-addons-it-needs)) |
| `directories` | Optional: libraries of the addon's own, in rex's [`directories`](https://github.com/debridmediamanager/rex/blob/main/docs/feed-api.md#libraries-a-source-declares) shape, such as `[{"name": "youtube", "shows": "YouTube"}]`: titles whose `path` starts with `youtube/` go to a TV library called YouTube instead of your Movies or TV Shows. rex-addons sends it when the addon registers |
| `sign_in` | Optional: `{"label": "Sign in to Example"}` for an addon that needs the person's account on a website ([below](#signing-in-with-a-code)). `label` is what the app's button says, `Sign in to <name>` without it. `kind` is `"code"` (the default: your `signIn()`) or `"spotify"` ([Spotify pairing](#signing-in-with-spotify)) |
| `exports` | Optional: functions of your default export that other addons may call, such as `["findSong"]` ([below](#calling-another-addon)). Each a name of letters, digits and `_`, at most 64 characters |
| `requires` | Optional: ids of addons whose `exports` yours calls, such as `["youtube"]`, never your own. The app shows whether each is installed, enabled and signed in |

Installing refuses a manifest that breaks one of these rules, and says which.

### Which rex-addons it needs

Set `host` to the lowest version that has everything you use. A rex-addons older than `host`
refuses to install the addon (`this addon needs rex-addons 0.5 or newer`) and greys it out in a
repository's list. rex-addons 0.1 doesn't know the field.

| `host` | What it adds | With a lower `host` |
|---|---|---|
| `0.2` | `sleep`, `atob`, `btoa`, `URL`, `URLSearchParams`, `hash`, `hmac`, `bytes()`, a binary `body` | Not checked: it installs, and fails where it uses them on an older rex-addons |
| `0.3` | `sign_in` | Installing refuses it |
| `0.4` | Playlists from `open()`, and `ffprobe()` of them | Playing fails: `open() returned playlists, which need "host": "0.4" in the manifest`. `ffprobe()` of them isn't checked |
| `0.5` | `"kind": "spotify"`, `exports`, `requires` | Installing refuses it |
| `0.6` | `select` settings | Installing refuses it |
| `0.6` | `storage.keys`, `TextEncoder`, `TextDecoder`, `crypto`, `md5` and bytes in `hash` and `hmac`, `ctx.host`, `ctx.addon`, `ctx.reason`, `RETRY_AFTER` | Not checked: it installs, and fails where it uses them on an older rex-addons (`RETRY_AFTER` is just an error there, tried again in 5 minutes) |
| `0.7` | `host` settings | Installing refuses it |
| `0.8` | `items()` in pages and with changes only: `{items, next}`, `{partial, items, removed}`, `ctx.cursor`, `ctx.full`, `ctx.last_sync` ([a large library](#a-large-library)) | Not checked at install, as a return value can't be: the sync fails, here with `items() returned {items: [...]}, which needs "host": "0.8" in the manifest`, on an older rex-addons with `items() must return a list` |

A setting's `help` needs no `host`: an older rex-addons doesn't show it.

## main.js

```js
export default {
  async items(ctx) { /* every title you have, as a list */ },
  async open(id, ctx) { /* where one title plays from: { url, headers? } */ },
};
```

Each may be `async` or a plain function that returns its answer. Each gets `ctx` as its last
argument:

| | |
|---|---|
| `ctx.config` | The settings, each one its saved value or its default |
| `ctx.host` | This rex-addons' version, such as `"0.8.0"` |
| `ctx.addon` | `{id, version}`, from your manifest |
| `ctx.reason` | Only for `items()`: why this sync runs ([below](#why-a-sync-runs)) |
| `ctx.full`, `ctx.last_sync`, `ctx.cursor` | Only for `items()`: whether this sync needs every title, when the last sync that worked began, and which page to return ([a large library](#a-large-library)) |

### items(ctx)

Returns everything the addon has, as a list, every time; a large library can return it in pages,
or only what changed ([a large library](#a-large-library)). rex takes what's gone out of the
library, so **throw an error rather than return half a list** when a website doesn't answer: the
sync then fails, shows the error in the app, and rex keeps what it had. As a second guard, rex
refuses a sync that would take away more than half of the titles. An empty list changes nothing
in rex.

A sync that failed is tried again 5 minutes later, and every 5 minutes after that. So when a
website answers `429`, either `await sleep(...)` and try again within the call, or throw an error
whose message starts with `RETRY_AFTER <seconds>`, such as
`RETRY_AFTER 3600: YouTube answered 429`: the next try then waits that long (at least a minute, at
most a day), and the app shows the rest of the message, or `the website asked to wait 3600 s`. A
sync someone starts from the app still runs at once. Needs `"host": "0.6"`.

A `required` setting that is empty (not set, blank text or an empty list) fails the sync with
`set <label> first`, and `items()` isn't called.

Each title is a rex feed item. The fields that matter most:

| Field | Meaning |
|---|---|
| `id` | Your key for the title, **for good**. Watch history hangs on it |
| `path` | A relative path ending in a video extension, such as `my-films/his-girl-friday/film.mp4`. It names the file in Plex's Get Info, and its folder groups a release. rex-addons sets the extension to what ffprobe finds, so `.mp4` vs `.mkv` doesn't need to be right |
| `kind`, `ids` | `"movie"` with `{"imdb": "tt…"}` or `{"tmdb": 123}`, and Plex names and pictures it. `"episode"` with `season` and `episode` and the **show's** ids for an episode ([below](#an-episode)) |
| `display` | `title`, `summary`, `date` (`YYYY-MM-DD` or `YYYY`), `thumb` (a poster URL), `art`. Shown for what Plex doesn't know. For an episode, also `show: {title, summary, poster, art}` |
| `name` | The release name, shown as is for an unplaced episode's show |
| `media_info` | What's in the file: ffprobe's output, as `ffprobe()` returns it. rex shows nothing it hasn't been told this about ([below](#probing-and-held-titles)) |

The rest (`release`, `subtitles`, `collections`, `channel` for live TV, music `track`s and so on)
is in rex's [feed API](https://github.com/debridmediamanager/rex/blob/main/docs/feed-api.md#an-item).

An entry that can't be a title is left out, with a `WARN` in the log and in the status's
`warnings`, and the rest still sync: one that isn't an object, has no `id` or `path` or an empty
`id`, repeats another's `id` or `path`, or has a `path` that starts with `/` or holds an empty,
`.` or `..` part (`a//b.mp4`, `../b.mp4`).

### Why a sync runs

`ctx.reason` says what started the sync, for an addon that does more on some than on others:

| `ctx.reason` | The sync runs because |
|---|---|
| `schedule` | `refresh_minutes` passed since the last one that worked |
| `retry` | The last one failed, and its wait passed; or an addon yours `requires` was just signed in or enabled |
| `manual` | A person asked: a sync from the app, enabling the addon, or restoring its deleted titles; and `rex-addons check` |
| `settings` | Its settings changed |
| `install` | It was installed or updated, from a repository, `--bundled` or `--dev`, and hasn't synced since |
| `sign_in` | It was just signed in |

A title a person deleted in a Plex app is left out of what you return from then on, until they
restore the addon's deleted titles in the app. Keep returning it; don't track deletions yourself.

### An episode

```js
{
  id: "pawnshop",
  path: "chaplin-mutuals/the-pawnshop.mp4",
  kind: "episode", season: 1, episode: 6,
  name: "Chaplin's Mutual Comedies",
  display: {
    title: "The Pawnshop", date: "1916-10-02",
    show: { title: "Chaplin's Mutual Comedies", poster: "https://archive.org/services/img/CC_1916_10_02_ThePawnshop" },
  },
  media_info: probe,
}
```

Without `ids`, the episode goes on the show titled with its `name`, one show for every title
of that name, and `display` names the episode and the show. For a show Plex knows, add the
**show's** ids (`ids: { tmdb: 1399 }`, never the episode's): Plex then names both, and `display`
is used only for what Plex doesn't know.

### Probing, and held titles

Probe each title yourself with `ffprobe()` and keep the answer in `storage`: a file doesn't
change, so once is enough. A title that arrives without `media_info` is probed by rex-addons
through your `open()` instead, and that probe is remembered by the title's `id`.

A title that has no probe still reaches rex, but rex **holds** it: it shows nowhere in Plex.
That happens when rex-addons runs without `--ffprobe`, or when the probe failed. A failed probe
is tried again after one `refresh_minutes`, and the wait doubles with each failure after that,
up to a week; a new version of the addon or new settings try them all again at the next sync. So
when a sync works and nothing shows, look at its `held` count ([Trying it](#trying-it)).

### open(id, ctx)

Called when rex reads a title, and when rex-addons probes a title you sent without
`media_info`. Return where it plays from:

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
each by a plain name (`video.m3u8`: at most 64 letters, digits, `-` and `_`, then `.m3u8`; never
`master.m3u8`). Leave `playlists` out when the master names only links:

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
[YouTube addon](https://github.com/debridmediamanager/rex-community-addons/blob/main/addons/youtube/main.js) writes one for each video, its segments byte ranges
(`EXT-X-BYTERANGE`) of YouTube's files.

## What your code can use

Plain JavaScript, plus these:

| Global | What it does |
|---|---|
| `fetch(url, {method, headers, body})` | Returns `{status, ok, headers, url, text(), json(), bytes()}`, `bytes()` giving a `Uint8Array`. `headers` is a plain object with lowercase names, `url` is where redirects ended. Only `hosts` are reachable, and every redirect is checked again. `body` is text, any typed array (such as a `Uint8Array`) or an `ArrayBuffer`. The body is read only when you call `text()`, `json()` or `bytes()`, once, so a `Range: bytes=0-0` check costs nothing. Fetches run one at a time: `Promise.all` over several waits for each in turn ([more](#more-on-fetch)) |
| `ffprobe(url, {headers})` | ffprobe's JSON of a video (`format`, `streams`, `chapters`), for `media_info`. rex-addons reads the link itself, for ffprobe, so the same `hosts` rule applies as for `fetch`; an HLS playlist works too. `ffprobe({playlist, playlists})` probes [playlists you wrote](#playlists-you-write), checked as `open()`'s are (`"host": "0.4"`). It fails with a clear message when rex-addons runs without ffprobe. Up to 2 minutes, which don't count as JavaScript time |
| `console.log`, `console.info`, `console.debug`, `console.warn`, `console.error` | Into rex-addons' log, tagged `[addon <id>]`, with your secret settings' values (6 characters or more) shown as `********`. `warn` and `error` lines start with `WARN` and `ERROR` |
| `storage.get(key)`, `storage.set(key, value)`, `storage.delete(key)`, `storage.keys(prefix)` | Your addon's own saved values, any JSON. They survive restarts and updates. A key never set gives `undefined`, and setting `undefined` deletes the key. `keys()` lists every key, sorted; `keys("probe:")` only those starting with `probe:`, so you can delete what's gone |
| `atob(text)`, `btoa(text)` | Base64, as in browsers: text whose characters are bytes (0 to 255) |
| `new URL(link, base)`, `URLSearchParams` | Links, as in browsers but smaller: `new URL(link, base)` resolves a relative link, a URL has `href`, `protocol`, `username`, `password`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`, `origin` and `searchParams`, and `URLSearchParams` takes a query string, an object or a list of pairs, has `get`, `getAll`, `has`, `set`, `append`, `delete`, `toString`, `entries`, `keys` and `values`, and works in `for...of`. Changing `searchParams` changes the URL |
| `hash(algorithm, data)`, `hmac(algorithm, key, data)` | A digest in lowercase hex, for signing an API request. `data` and `key` are text (taken as UTF-8), a `Uint8Array` or an `ArrayBuffer`. `algorithm` is `md5`, `sha1`, `sha256` or `sha512` |
| `new TextEncoder().encode(text)`, `new TextDecoder().decode(bytes)` | Text to UTF-8 bytes and back, as in browsers. UTF-8 only: `new TextDecoder("latin1")` throws |
| `crypto.getRandomValues(array)`, `crypto.randomUUID()` | Random numbers for a nonce or a PKCE verifier, as in browsers: `getRandomValues` fills an integer typed array, at most 65,536 bytes a call, and returns it. There's no `crypto.subtle` |
| `await sleep(ms)` | Waits, for backing off when a website answers 429. The wait doesn't count as JavaScript time, and a sleep past the end of the call fails at once |
| `spotify.user()`, `await spotify.get(path)`, `await spotify.post(path, body)` | Only with `"sign_in": {"kind": "spotify"}`: the person's Spotify library ([below](#signing-in-with-spotify)) |
| `await addons.call(id, name, ...args)` | Only with `requires`: another addon's exported function ([below](#calling-another-addon)) |

There's nothing else: no timers (`setTimeout`, use `sleep`), no `require` or `import`, no file
access. Each call starts fresh, so keep anything you want to remember across calls in `storage`.

### More on fetch

- `method` is `GET` (the default), `HEAD`, `POST`, `PUT`, `PATCH` or `DELETE`, in any case.
- It waits up to 10 s to connect. After that, only the call's own time limit (below) stops it.
- It follows up to 9 redirects, each one checked against your `hosts`.
- It sends `User-Agent: rex-addons/<version>` unless you set your own.
- At most 32 responses can wait with their body unread. Past that the oldest is closed, and
  reading it fails. Read each body, or don't keep the response.

### Errors you throw

The app shows the first line of what you throw; the log gets all of it, stack included. So put
the useful part first: `archive.org answered 503`, not `Error in step 3`.

### Limits, per call

| | |
|---|---|
| Memory | 64 MB |
| JavaScript time | 30 s for `items()`, 10 s for `open()`. Time spent waiting on `fetch`, `ffprobe` or `sleep` doesn't count |
| Whole call | 20 minutes for `items()`, 90 s for `open()`. A `fetch` or body read still waiting then fails |
| One sync's pages | 10,000 pages, 200,000 titles, 64 MB of ids and paths, and an hour of `items()` calls, waiting included |
| A fetched body | 16 MB |
| `storage` | 16 MB in all, written to disk when the call ends |
| `crypto.getRandomValues` | 65,536 bytes a call |

`signIn()`, `signInPoll()`, `signOut()` and functions other addons call get `open()`'s limits.

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

## A large library

One `items()` call has 30 s of JavaScript and 64 MB, which is about 11,000 titles that carry
their `media_info`. Past that, or when listing every title every time costs too much, return them
in pages, or only what changed, or both. Each needs `"host": "0.8"`.

### Pages

Return `{items: [...], next}` instead of a list. While an answer has a `next` (any JSON but
`null`), rex-addons calls `items()` again, a fresh call with limits of its own, with `ctx.cursor`
set to that `next`. The first call has no `ctx.cursor`.

```js
async items(ctx) {
  const answer = await fetch(`https://api.example.com/videos?limit=2000&after=${ctx.cursor ?? ""}`);
  if (!answer.ok) throw new Error(`api.example.com answered ${answer.status}`);
  const page = await answer.json();
  return { items: page.videos.map(toItem), next: page.next_cursor ?? null };
}
```

All the pages together are every title, as one list would be: a title no page names leaves rex
once the last page is in, the half guard counts every page, and an entry whose id or path an
earlier page had is left out. Each page reaches rex as soon as it is checked, so rex-addons holds
one page at a time. A page that throws fails the whole sync, and rex removes nothing (it keeps the
titles of the pages before it, added or updated).

### Changes only

Every `items()` call gets `ctx.full` and `ctx.last_sync`. When `ctx.full` is `false`, you may
return only what changed:

```js
return { partial: true, items: [/* new or changed titles, whole */], removed: ["id", /* ... */] };
```

`items` are the titles that are new or changed, each whole, as a full list has it, and probed as
usual; `removed` the ids that are gone. A partial answer may have a `next` too; then every page
is partial. A plain list, or `{items}` without `partial`, is always every title, also when
`ctx.full` is `false`.

`ctx.last_sync` is when the last sync that worked began (Unix seconds), or `null`: ask the website
for what changed since then. When `ctx.full` is `false`, **everything your last `items()`
returned reached rex**, so a list in `storage` of what you returned is what rex has.

`ctx.full` is `true`, and a partial answer fails the sync, when:

- the addon has never synced, or was installed or updated since;
- its settings changed, it was just signed in, a person restored its deleted titles, or a sync
  was forced;
- the last sync failed after `items()` had answered, or rex-addons stopped during one;
- the last sync that got every title is a day old, so rex catches up even if it lost something;
- a title rex holds is due for another probe ([held titles](#probing-and-held-titles));
- rex-addons was just updated from a version before 0.8.

The rest:

- `removed` is applied once the last page is in. An id that is in `items` too stays.
- `removed` can't take more than half of the addon's titles: nothing is removed and the sync
  fails, `error_kind` `would_remove`. A forced sync asks for every title instead, and meets
  rex's own guard.
- A title whose `path` another title already has is left out, as rex would refuse it: move or
  remove the other one in an earlier sync.
- A title a person deleted in a Plex app stays left out.

### Costly work per title

Pages and changes take care of how many titles there are. A lookup or a probe for each title is
still slow, and a website won't answer a thousand of them at once. So do the costly part for a few
new titles each sync and keep what it found in `storage` by the title's id; the rest arrive with
later syncs. Forget what left the list, so `storage` stays under its 16 MB. Or leave
`media_info` out: rex-addons then probes each title itself and keeps the probe, outside your
`storage`.

The [YouTube addon](https://github.com/debridmediamanager/rex-community-addons/blob/main/addons/youtube/main.js) checks at most 30 new videos a sync, 1.5 s apart,
returns only videos it has checked, and reads its playlists again only every six hours, keeping
the listing in `storage` in between. The [Spotify addon](https://github.com/debridmediamanager/rex-community-addons/blob/main/addons/spotify/main.js) looks up 30
new songs a sync (a setting). The [internet-archive addon](../addons/internet-archive/main.js)
lists every film and probes for 10 minutes at most, sending the rest without `media_info` for
rex-addons to probe.

The same, small: 200 films from archive.org, ten new ones looked up and probed each sync. The
search is sorted, because unsorted archive.org lists different films from one search to the
next. Its `hosts` are `archive.org` and `*.archive.org`, and `refresh_minutes` is 60.

```js
const PER_SYNC = 10;

export default {
  async items(ctx) {
    const search = await fetch("https://archive.org/advancedsearch.php?q=collection:feature_films&fl[]=identifier&fl[]=title&sort[]=identifier+asc&rows=200&output=json");
    if (!search.ok) throw new Error(`archive.org search answered ${search.status}`);
    const films = (await search.json()).response.docs;

    let looked = 0;
    for (const film of films) {
      if (storage.get(`film:${film.identifier}`) || looked >= PER_SYNC) continue;
      looked++;
      try {
        const answer = await fetch(`https://archive.org/metadata/${film.identifier}`);
        if (!answer.ok) throw new Error(`archive.org answered ${answer.status}`);
        const file = (await answer.json()).files?.find((one) => /^[^/]+\.mp4$/.test(one.name));
        if (!file) throw new Error("no MP4");
        const url = `https://archive.org/download/${film.identifier}/${encodeURIComponent(file.name)}`;
        storage.set(`film:${film.identifier}`, { url, probe: await ffprobe(url) });
      } catch (error) {
        console.warn(`${film.identifier}: ${error.message}`);
        storage.set(`film:${film.identifier}`, { skip: true });
      }
    }

    // Forget films that left the list.
    const listed = films.map((film) => film.identifier);
    for (const old of storage.get("listed") ?? []) if (!listed.includes(old)) storage.delete(`film:${old}`);
    storage.set("listed", listed);

    return films.filter((film) => storage.get(`film:${film.identifier}`)?.url).map((film) => ({
      id: film.identifier,
      path: `archive-films/${film.identifier}/film.mp4`,
      display: { title: film.title },
      media_info: storage.get(`film:${film.identifier}`).probe,
    }));
  },

  async open(id) {
    const film = storage.get(`film:${id}`);
    if (!film?.url) throw new Error(`no film ${id}`);
    return { url: film.url };
  },
};
```

## A server on your home network

An addon for the person's own server (Jellyfin, Navidrome, a NAS over WebDAV) can't list its
address in `hosts`: you don't know it. Give it a `host` setting instead, and the person types
the address in the app: `192.168.1.20:8096`, `http://nas.local:8096/jellyfin` or `https://[fd00::5]:8920`.
That host, on that port only, is then reachable as if your manifest listed it exactly:
`fetch()`, each redirect, `open()` links, playlists and `ffprobe()`, until the person types
another address. Your code gets the text as typed, in `ctx.config`, and builds its links from it.

| Typed | Reachable |
|---|---|
| `192.168.1.20:8096`, `http://192.168.1.20:8096/jellyfin` | `192.168.1.20` on port 8096 |
| `http://nas.local`, `https://nas.local` | `nas.local` on port 80, or 443 |
| `nas.local`, with no scheme and no port | `nas.local` on port 80 and 443, as your code may put either scheme before it |
| `[fd00::5]:8920` | `fd00::5` on port 8920 |

A link or redirect to another port of that host is refused, naming the port: so a server that
redirects from `:8096` to `:8920` needs both typed, in two settings.

```json
{
  "id": "home-videos",
  "name": "Home videos",
  "version": "1.0.0",
  "host": "0.7",
  "config": [
    {"key": "server", "label": "Server", "type": "host", "required": true, "help": "Such as 192.168.1.20:8080"}
  ]
}
```

```js
// A plain file server whose /videos.json lists [{"id": "beach", "title": "Beach 2024", "file": "beach.mp4"}].
const base = (ctx) => (/^https?:\/\//.test(ctx.config.server) ? ctx.config.server : `http://${ctx.config.server}`).replace(/\/$/, "");

export default {
  async items(ctx) {
    const answer = await fetch(`${base(ctx)}/videos.json`);
    if (!answer.ok) throw new Error(`the server answered ${answer.status}`);
    return (await answer.json()).map((video) => ({
      id: video.id,
      path: `home-videos/${video.id}/${video.file}`,
      display: { title: video.title },
    }));
  },

  async open(id, ctx) {
    const video = (await (await fetch(`${base(ctx)}/videos.json`)).json()).find((one) => one.id === id);
    if (!video) throw new Error(`no video ${id}`);
    return { url: `${base(ctx)}/${encodeURIComponent(video.file)}` };
  },
};
```

- Saving the setting refuses anything that isn't one `http(s)` address: no `*`, no user name or
  password in it, and never this device (`127.0.0.1`, `localhost`, `[::1]`). A server on the same
  device is typed by its network address. A name that turns out to lead to this device is still
  refused, when the addon connects.
- Only what the person typed counts: a `host` setting has no `default`, and an update that turns
  another setting into a `host` setting clears it, so it is typed again.
- Several `host` settings each add their own host and port. One that's empty adds nothing; with
  `required: true` the sync then fails with `set Server first`.
- On the device rex runs on, only the port typed is reached, so a server there doesn't hand the
  addon rex or rex-addons. But an address typed with rex's own port (`192.168.1.20:32400`) gives
  the addon rex's feed on that address, which takes any program on the network: titles in,
  titles out. Type the server's own port.
- Another addon calling yours through `addons.call` reaches only its own hosts, and a link you
  return to it must be on them.

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

## Signing in with Spotify

A Spotify addon lists the person's Spotify library (playlists, liked songs, saved albums) in
rex. Nobody types anything and no Spotify developer app is needed: rex-addons pairs with the
Spotify app the way a speaker does. The person opens Spotify on a phone on the same network,
taps the devices button and picks this device (`rex`, or whatever `--device-name` says). rex-addons
keeps the sign-in to itself; your code never sees it.

```json
"host": "0.5",
"hosts": ["*.googlevideo.com"],
"sign_in": {"kind": "spotify", "label": "Connect Spotify"}
```

`hosts` here is where the songs play from, not Spotify: the `spotify` global below reaches
Spotify without them. This one plays them from YouTube.

There's no `signIn()` or `signInPoll()` to write. Signing out forgets the sign-in and calls your
`signOut()` if you export one. **Spotify's audio is out of reach**: Spotify no longer hands it to
clients like this one for many accounts, and rex-addons doesn't try. Play what you list from
somewhere else, such as YouTube ([calling another addon](#calling-another-addon)).

Your code gets a `spotify` global, talking to Spotify's own API (`spclient`) as Spotify's
desktop app does, with the person's session. It is Spotify's own, undocumented API: paths can
change without notice, so throw (and keep what rex has) when an answer isn't what you expect.
The paths that list a library, and how to turn a uri into the id they take, are in
[Listing a Spotify library](spotify.md).

| Call | What it does |
|---|---|
| `spotify.user()` | The account's Spotify username, which those paths take |
| `await spotify.get(path)` | `GET` the path, answered as JSON |
| `await spotify.post(path, body)` | `POST` `body` as JSON, answered as JSON |

`path` starts with `/` and is on Spotify's host only. A failure throws: `Spotify answered 404 to
/metadata/4/track/...`, or, when Spotify no longer takes the sign-in, an error starting with
`SIGN_IN_REQUIRED`, which signs the addon out if you let it through. librespot, which talks to
Spotify for rex-addons, waits out a `429` by itself when Spotify asks for 10 seconds or less.

## Calling another addon

An addon can let others call some of its functions, so a Spotify addon can ask the YouTube addon
to find and play a song with the person's YouTube sign-in, instead of signing in to YouTube
itself. The one that offers lists them:

```json
"host": "0.5",
"exports": ["findSong", "songAudio"]
```

and the one that calls says whose it needs:

```json
"host": "0.5",
"requires": ["youtube"]
```

```js
const found = await addons.call("youtube", "findSong", { title: "...", artists: ["..."], duration_ms: 213000 });
```

- The function runs in **the other addon's own sandbox**: its `hosts`, settings, `storage` and
  sign-in, with `open()`'s limits, and `ctx` as its last argument as usual. Arguments and the
  result are JSON. Its time counts as waiting for yours, not as your JavaScript time.
- What it throws, you get thrown: `YouTube: no match for ...`. So does a call that cannot be
  made: `the addon youtube is not installed: install it and sign in to it first`,
  `YouTube is disabled: enable it first`, `sign in to YouTube first`, or `YouTube exports no findSong()`.
  Show these to the person. When the other addon's website ended its session it is signed out,
  and you get `sign in to YouTube again: ...`; you are not signed out.
- One level: an addon called by another cannot call a third.
- A link it returns that your `open()` returns must be on **your** `hosts`, as always.

Installing checks that `main.js` exports each function `exports` names. What the YouTube addon
exports, and what each function returns: [The YouTube addon's exports](youtube.md).

## A repository of addons

A repository is any public git repository people can reach over HTTPS: GitHub, GitLab, Codeberg
or a server of your own. Every folder in it holding a `manifest.json` and a `main.js` is an
addon, up to three folders down; folders whose names start with `.` are skipped. For example:

```
repository.json                      optional: {"name": "...", "description": "..."}
addons/internet-archive/manifest.json
addons/internet-archive/main.js
addons/my-addon/manifest.json
addons/my-addon/main.js
```

Without `repository.json` the repository is named after its address. People add it by that
address (`https://codeberg.org/you/my-addons`, or `...#branch` for another branch), pick your
addons from its list and install them. rex-addons fetches only the newest commit, and only what
it needs: at most 2,000 files and 64 MB, never a symbolic link or submodule. To ship a change,
raise the addon's `version` and push; people see the update when they refresh the repository.
To move an addon to another repository, take it out of this one and give it a higher `version`
there: once this one no longer offers it, the other one updates it, settings and titles kept.

## Trying it

`rex-addons check` tries an addon folder once, without rex: it parses the manifest, does what
installing checks, runs `items()`, every page of it with `ctx.full` `true`, and tells you what came
back. `--since <unix seconds>` runs it as a sync that may answer with changes only: `ctx.full`
`false`, `ctx.last_sync` that time.

```bash
rex-addons check ~/my-addons/my-films --ffprobe "$(command -v ffprobe)"
rex-addons check ~/my-addons/my-films --config '{"pawnshop": false}' --open his-girl-friday
```

It prints a line for each step, `ok`, `FAIL` or `note`: how many titles `items()` returned, over
how many pages and how long it took (for changes only, also how many ids it removed), every entry left out and why, every required setting not given, and the first title
as JSON. With `--open <id>` it runs `open()` for that title too, prints what it returned and
whether the link is on your `hosts`. What your addon prints is there too. It exits with 1 when a
step failed.

- `--config` takes the settings as the app sends them; the rest take their defaults.
- Without `--ffprobe`, `ffprobe()` fails as it does in rex-addons without one.
- `storage` starts empty and is thrown away. `--data <dir>` names a rex-addons data directory
  instead: the addon's storage there is used, and `addons.call` reaches the addons installed there.
- `check` can't sign in. An addon with `sign_in` runs with what its storage holds, so give it
  `--data` where it is signed in, or it fails where it needs the account.

Then run it in rex-addons, beside rex: start rex-addons with `--dev` naming a folder that holds your
addon's folder. It's installed on start, and installed again, then synced, each time you save its
`manifest.json` or `main.js`, without raising its `version`. rex-addons talks to rex at `--rex`
(default `http://127.0.0.1:32400`) and syncs nothing until it answers.

```bash
rex-addons --data ~/rex-addons-dev --ffprobe "$(command -v ffprobe)" --dev ~/my-addons
curl http://127.0.0.1:32410/api/status
curl http://127.0.0.1:32410/api/addons/my-films/log
curl -X POST http://127.0.0.1:32410/api/addons/my-films/sync
```

A change it can't install (a manifest that breaks a rule, a `main.js` that doesn't load) is a
`WARN [host] dev <folder>: ...` line in the log, and the last version that installed stays. A sync
runs once rex answers. The log shows what your addon printed and why a sync failed; `/log` answers
the addon's last lines of it. `/api/status` shows each addon's `items` (in rex), `held`
([held titles](#probing-and-held-titles)) and `warnings` (entries left out). rex itself shows what
it has and what it holds:

```bash
curl http://127.0.0.1:32400/rex/feed/addon-my-films/items
curl http://127.0.0.1:32400/rex/feed/addon-my-films/held
```

`--bundled` works as `--dev` does, but installs again only on a start after you raise `version`,
and one it refuses is a `WARN [host] bundled <id>: ...` line; it's how an app ships addons.

To sign in an addon with `sign_in`, start it and read the code to enter:

```bash
curl -X POST http://127.0.0.1:32410/api/addons/my-addon/sign-in
curl http://127.0.0.1:32410/api/addons/my-addon
```

`sign_in.pending` holds the `code` and the `url` to enter it at; read the addon again every few
seconds until `sign_in.account` is set, and it syncs. When `REX_ADDONS_TOKEN` is set, every
`/api/` request needs `-H "Authorization: Bearer $REX_ADDONS_TOKEN"`. The rest of the API is in
[api.md](api.md).

Once it works, push it to your repository and add that in the app.

## Editor help

Two files in [`types/`](../types) give VS Code autocomplete and checking, still with no npm and no
build step:

- **manifest.json**: add this as its first field. VS Code then completes the fields and
  underlines one that breaks a rule installing checks. rex-addons ignores it, older ones too.

  ```json
  "$schema": "https://raw.githubusercontent.com/debridmediamanager/rex-addons/main/types/manifest.schema.json",
  ```

- **main.js**: put [`rex-addons.d.ts`](../types/rex-addons.d.ts) beside it, start `main.js` with
  the two lines below, and put `/** @type {Addon} */` on the line above `export default {`.

  ```bash
  curl -O https://raw.githubusercontent.com/debridmediamanager/rex-addons/main/types/rex-addons.d.ts
  ```

  ```js
  // @ts-check
  /// <reference path="rex-addons.d.ts" />
  ```

  VS Code then completes `fetch`, `storage`, `ctx.config` and the rest, and underlines what
  isn't there (`setTimeout`, `document`), a call with the wrong arguments, an item without a
  `path` and an `open()` that returns neither `{ url }` nor playlists.

The types are rex-addons 0.8's, and they keep a browser's globals out even where a
`jsconfig.json` asks for them. The [bundled addons](../addons) use both files (the YouTube addon
only the schema).

## When it doesn't work

| What you see | Why, and what to do |
|---|---|
| A sync worked, nothing shows in Plex | The titles are held: they have no probe. Check `held` in `/api/status`, then whether rex-addons runs with `--ffprobe` and why probes failed (`<id>: probe failed: ...` in the log) ([held titles](#probing-and-held-titles)) |
| Nothing syncs, no error | rex isn't answering at `--rex`. `/api/status` says `"reachable": false` |
| `rex at http://127.0.0.1:32400 is not reachable: ...` | The same, for a sync you started yourself |
| Some titles are missing, the sync worked | Entries left out: `warnings` in `/api/status` counts them and quotes the first few, and the log has each as `items(): ...; left out` ([items()](#itemsctx)) |
| `items() must return a list` | `items()` returned something else, or nothing |
| `set <label> first` | A `required` setting is empty |
| `<label>: must be one of 1080, 720` | A `select` was given a value that isn't one of its options |
| `the website asked to wait 3600 s` | `items()` threw `RETRY_AFTER 3600`: the next try is an hour later |
| `sign in first` | An addon with `sign_in` isn't signed in |
| `rex kept everything: this sync would remove 80 of the 100 items it has from this addon, more than half. If that is right, sync with force` | `items()` returned under half of what it did before. If a website gave a short answer, throw instead. If it's right, sync with `{"force": true}` |
| `items() ran JavaScript for longer than 30 s` | Too much work in one call. Return pages, or do less each sync ([a large library](#a-large-library)) |
| `items() returned {items: [...]}, which needs "host": "0.8" in the manifest` | Pages and changes only need `"host": "0.8"` |
| `items() returned partial: true, but this sync needs every title (ctx.full is true)` | Return every title when `ctx.full` is `true` ([changes only](#changes-only)) |
| `items(): a sync's pages are all partial, or none is` | One page said `partial: true` and another didn't |
| `items(): removed needs partial: true`, `items(): removed must be a list of ids` | `removed` goes with `partial: true`, as a list of id strings |
| `items() must return a list, or {items: [...]}` | The answer was an object without an `items` list |
| `rex-addons removed nothing: items() removed 80 of the 100 titles the addon has, more than half. If that is right, sync with force` | A partial answer's `removed` took more than half. A forced sync asks for every title |
| `items() returned 10000 pages and still a next: at most 10000 a sync` | The last page needs `next: null`, or none. Return bigger pages |
| `items() ran for longer than 3600 s over all its pages` | The pages took over an hour together. Return changes only ([a large library](#a-large-library)) |
| `items() returned more than 200000 titles: at most 200000 a sync`, `... more than 64 MB of ids and paths ...` | Too many titles for one addon, or very long ids and paths |
| `items(): x has the path "a/b.mp4", which y has; left out` | A partial answer gave a title the path another title has ([changes only](#changes-only)) |
| `rex refused the snapshot (409): no such snapshot session at this part: start again from part 0` | rex restarted during a sync of several pages. The next try, 5 minutes later, starts again |
| `items() took longer than 1200 s` | The whole call ran past 20 minutes, waiting included |
| `items() never finished: it waits on something that never comes (there are no timers)` | A promise nothing resolves, such as one waiting on `setTimeout`. Use `await sleep(ms)` |
| `out of memory` | The call went past 64 MB |
| `fetch: example.com is not in the addon's hosts` | Add the host to `hosts`. A redirect to another host needs that one too |
| `fetch: nas.lan: ...: nas.lan is on this device or a private network` | Listed through a wildcard, or on this device. List a private address exactly ([`hosts`](#manifestjson)), or let the person type it ([a server on your home network](#a-server-on-your-home-network)) |
| `fetch: 192.168.1.20 is not in the addon's hosts`, from an addon with a `host` setting | The setting is empty or names another address, or your code builds the link from something else |
| `fetch: 192.168.1.20 is not in the addon's hosts: its settings allow 192.168.1.20 on port 8096 only, not 8920` | The link, or a redirect, goes to another port of the address typed. Have the person type that one too, in a setting of its own. Also in `open()` and playlist errors |
| `Server: that is this device, which addons never reach` | The person typed `127.0.0.1`, `localhost` or the like: a server on the same device needs its network address, such as `192.168.1.20` |
| `Server: give an address such as 192.168.1.20:8096 or http://nas.local:8096` | What was typed into a `host` setting isn't one address: no `*`, no spaces, an IPv6 address in `[ ]` |
| `fetch: method OPTIONS is not allowed` | Only `GET`, `HEAD`, `POST`, `PUT`, `PATCH` and `DELETE` |
| `the body was already read` | `text()`, `json()` and `bytes()` read the body once. Keep what the first one gave |
| `the body was already read, or closed after 32 newer unread ones` | A response kept unread while 32 more were fetched ([more on fetch](#more-on-fetch)) |
| `the body is over 16 MB` | Ask for less: a `Range`, a smaller page |
| `storage is limited to 16 MB` | Keep less in `storage`, and delete what's gone (`storage.keys()` lists it) |
| `sleep(60000) would outlast the call: a call may take 90 s in all` | The sleep is longer than the call has left |
| `ffprobe: ffprobe() is not available: rex-addons runs without --ffprobe` | Start rex-addons with `--ffprobe` |
| `open() returned https://..., which is not on the addon's hosts` | The link's host isn't in `hosts`, or it isn't `http(s)` |
| `open() returned no url` | `open()` returned something without `url` (or `playlist`) |
| `open() returned playlists, which need "host": "0.4" in the manifest` | Set `"host": "0.4"` or higher |
| `main.js: main.js has no default export object` | `main.js` needs `export default { ... }` |
| `main.js: main.js exports no open()` | The default export lacks that function. Installing needs `items` and `open`, plus `signIn` and `signInPoll` with `sign_in`, and each name in `exports` |
| `manifest: ...` | The manifest breaks a rule: the message says which ([manifest.json](#manifestjson)) |
| `this addon needs rex-addons 0.5 or newer` | Its `host` is newer than this rex-addons |
| `manifest: a select setting needs "host": "0.6" or newer` | Set `"host": "0.6"` or higher |
| `manifest: a host setting needs "host": "0.7" or newer` | Set `"host": "0.7"` or higher |
| `hash: crc32 is not md5, sha1, sha256 or sha512` | Only those four |
| `TextDecoder: latin1 is not supported, only utf-8` | Decode other encodings yourself, from `bytes()` |
| `WARN [host] dev <folder>: ...` | `--dev` couldn't install what you saved: the rest of the line says why. The last version that installed stays |
