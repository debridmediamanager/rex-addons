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

The link must be on one of your `hosts`. An answer is reused for 10 minutes, so `open()` isn't
called for every seek.

## What your code can use

Plain JavaScript, plus these:

| Global | What it does |
|---|---|
| `fetch(url, {method, headers, body})` | Returns `{status, ok, headers, url, text(), json()}`. `headers` is a plain object with lowercase names, `url` is where redirects ended. Only `hosts` are reachable, and every redirect is checked again. `body` is text. The body is read only when you call `text()` or `json()`, so a `Range: bytes=0-0` check costs nothing |
| `ffprobe(url, {headers})` | ffprobe's JSON of a video (`format`, `streams`, `chapters`), for `media_info`. rex-addons reads the link itself, for ffprobe, so the same `hosts` rule applies as for `fetch`; an HLS playlist works too. It fails with a clear message when rex-addons runs without ffprobe. Up to 2 minutes, which don't count as JavaScript time |
| `console.log`, `console.warn`, `console.error` | Into rex-addons' log, tagged `[addon <id>]` |
| `storage.get(key)`, `storage.set(key, value)`, `storage.delete(key)` | Your addon's own saved values, any JSON. They survive restarts and updates |
| `atob(text)`, `btoa(text)` | Base64, as in browsers: text whose characters are bytes (0 to 255) |
| `new URL(link, base)`, `URLSearchParams` | Links, as in browsers but smaller: `new URL(link, base)` resolves a relative link, a URL has `href`, `protocol`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`, `origin` and `searchParams`, and `URLSearchParams` has `get`, `getAll`, `has`, `set`, `append`, `delete`, `toString` and `entries`. Changing `searchParams` changes the URL |
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
