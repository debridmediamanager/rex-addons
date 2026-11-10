# Writing an addon

An addon is a folder with two files: `manifest.json`, which says what it is, and `main.js`, which
says what it has and where each title plays from. rex-addons does the rest: it registers the
addon with rex, checks each title once with ffprobe, sends rex the list, serves the bytes and
keeps it all up to date.

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
| `version` | Numbers and dots. A bundled addon updates when its version is higher |
| `hosts` | The websites `fetch()` may reach, and that `open()` links may point at: `example.com` exactly, `*.example.com` for anything under it, or `*` for any public website. Addresses on the device or the home network are never reachable through `*` |
| `config` | The settings the app shows. Each has a `key`, a `label`, a `type` (`text`, `secret`, `bool`, `number` or `list`, a list of text), and optionally a `default` and `required: true`. A `secret` is shown masked |
| `refresh_minutes` | How often `items()` runs. Default 360, at least 5 |

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

The rest (`release`, `subtitles`, `collections`, `channel` for live TV, music `track`s and so on)
is in rex's [feed API](https://github.com/debridmediamanager/rex/blob/main/docs/feed-api.md#an-item).
Leave out `media_info`: rex-addons probes every new title itself and remembers it by `id`.

### open(id, ctx)

Called when rex reads a title, and once before that when it's checked. Return where it plays from:

- `{ url }`: rex is sent straight there.
- `{ url, headers }`: for a link that needs headers (a `Referer`, a token). rex-addons then
  passes the bytes through itself, `Range` and all.

The link must be on one of your `hosts`. An answer is reused for 10 minutes, so `open()` isn't
called for every seek.

## What your code can use

Plain JavaScript, plus three things:

| Global | What it does |
|---|---|
| `fetch(url, {method, headers, body})` | Returns `{status, ok, headers, url, text(), json()}`. `headers` is a plain object with lowercase names, `url` is where redirects ended. Only `hosts` are reachable, and every redirect is checked again. `body` is text. The body is read only when you call `text()` or `json()`, so a `Range: bytes=0-0` check costs nothing |
| `console.log`, `console.warn`, `console.error` | Into rex-addons' log, tagged `[addon <id>]` |
| `storage.get(key)`, `storage.set(key, value)`, `storage.delete(key)` | Your addon's own saved values, any JSON. They survive restarts and updates |

There's nothing else: no timers (`setTimeout`), no `require` or `import`, no `URL`, no
`TextEncoder`, no file access. Each call starts fresh, so keep anything you want to remember
across calls in `storage`.

Limits, per call:

| | |
|---|---|
| Memory | 64 MB |
| JavaScript time | 30 s for `items()`, 10 s for `open()`. Time spent waiting on `fetch` doesn't count |
| Whole call | 20 minutes for `items()`, 90 s for `open()` |
| A fetched body | 16 MB |
| `storage` | 8 MB in all |

## A complete example

Two public domain films from the Internet Archive. One has an IMDb id, so Plex names it; the
other is named by `display`. The manifest is the one above.

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

    return FILMS.filter((film) => ctx.config.pawnshop || film.id !== "the-pawnshop").map((film) => ({
      id: film.id,
      path: `my-films/${film.id}/${film.file}`,
      ...(film.imdb ? { kind: "movie", ids: { imdb: film.imdb } } : {}),
      display: { thumb: `https://archive.org/services/img/${film.item}`, ...film.display },
    }));
  },

  async open(id) {
    const film = FILMS.find((one) => one.id === id);
    if (!film) throw new Error(`no film ${id}`);
    storage.set("last played", id);
    return { url: `https://archive.org/download/${film.item}/${film.file}` };
  },
};
```

## Trying it

Serve the folder and install it by its link:

```bash
cd my-films && python3 -m http.server 8000 &
curl -X POST http://127.0.0.1:32410/api/addons -d '{"url": "http://127.0.0.1:8000/manifest.json"}'
curl http://127.0.0.1:32410/api/status
```

`main.js` is fetched from beside `manifest.json`, so any web host that serves both works for
sharing an addon. Installing checks that `main.js` loads and exports `items` and `open`;
installing again from the same link updates it and keeps its settings and storage. A sync runs
right away. The log shows what your addon printed and why a sync failed, and
`curl http://127.0.0.1:32400/rex/feed/addon-my-films/items` shows what rex holds.
