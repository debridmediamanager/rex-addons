# Running rex-addons and its API

What an app (or a script) needs to run rex-addons and manage its addons. `cargo build --release`
builds it for this machine; `./build-android.sh` builds `target/android/<abi>/librexaddons.so`
for arm64-v8a, armeabi-v7a and x86_64 with the Android NDK (`ABIS=` and `OUT=` narrow and move
that).

## The command

```
rex-addons --listen 127.0.0.1:32410 --rex http://127.0.0.1:32400 --data <dir> [--ffprobe <path>] [--bundled <dir>] [--dev <dir>] [--default-repo <url>]...
```

| Flag | Default | Meaning |
|---|---|---|
| `--listen` | `127.0.0.1:32410` | Where the API and the byte paths answer. This address is also what each addon registers with rex as its byte URL (`0.0.0.0` registers as `127.0.0.1`) |
| `--rex` | `http://127.0.0.1:32400` | rex. rex-addons waits for it: nothing syncs, and nothing counts as failed, until it answers |
| `--data` | `./rex-addons-data` | `<dir>/addons/<id>/` holds each installed addon: `manifest.json`, `main.js`, `config.json`, `storage.json`, and the host's own `state.json`, `probes.json`, `probe_failures.json`, `index.json`, `held.json`, and `cache/`, the subtitles it found, fetched and translated (`search/` 12 hours and 16 MB, `files/` 7 days and 64 MB, `translations/` 30 days and 64 MB; emptied when its settings change or it is updated). A `<kind>-credentials.json` an older rex-addons kept there for a sign-in kind 0.9 doesn't have is deleted once the addon is updated, on start, or on sign-out. `<dir>/repos/<repo id>/` holds each repository's fetched files, `<dir>/repos.json` the list. `<dir>/removed.json` lists bundled addons the person removed |
| `--ffprobe` | none | The ffprobe behind addons' `ffprobe()` and the host's own check of a title that arrives without `media_info`. Without it `ffprobe()` fails, those titles reach rex unprobed, and rex holds unprobed titles without showing them |
| `--bundled` | none | Addon folders installed on the first start, and updated whenever the bundled `version` is higher. A person can remove one; it then stays removed. Installing the same addon from a repository later makes it that repository's |
| `--dev` | none | Addon folders installed on every start, whatever their `version`, and installed again, then synced (`ctx.reason` `install`), within a second of their `manifest.json` or `main.js` changing. For writing an addon; one it can't install is a `WARN [host] dev <folder>: ...` line and the last one installed stays |
| `--default-repo` | none | A repository added the first time it is given, and fetched in the background; repeat it for several. One the person removed is never added again |
| `--device-name` | none | Accepted and ignored, so apps that still pass it start. Until 0.9 it named this device for Spotify pairing |

`--version` and `--help` print and exit. Logs go to stdout (info) and stderr (`WARN`, `ERROR`),
one line each, tagged `[host]`, `[<addon id>]` or `[addon <id>]` for what the addon itself printed.

```
rex-addons check <addon folder> [--config '<json>'] [--since <unix seconds>] [--open <item id>] [--ffprobe <path>] [--data <dir>]
```

tries one addon folder without rex or a running rex-addons: the manifest, what installing checks,
`items()` (`ctx.reason` `manual`, every page, `ctx.full` `true`, or with `--since` `false` and
`ctx.last_sync` that time) and, with `--open`, one `open()`, each printed as an `ok`,
`FAIL` or `note` line. It exits with 0 when every step worked, 1 when one failed, 2 when asked
wrongly. `--data` names a rex-addons data directory whose storage, and installed addons, the
addon uses; without it storage starts empty and is thrown away. What it prints is in
[Trying it](writing-addons.md#trying-it).

## The API

JSON in and out. It answers only this device: the socket's own peer address must be loopback
(headers are never read for that); anything else gets `403`. Errors are `{"error": "..."}`.

- **Token.** When the environment variable `REX_ADDONS_TOKEN` is set (never a flag: other
  programs can read a process's flags), every `/api/` request must carry
  `Authorization: Bearer <token>`, else `401`. An app starting rex-addons picks a random token and
  keeps it to itself. Without it the API is open to every program on this device, and the start
  logs a `WARN` saying so; that is meant for a desktop.
- **Host.** A request whose `Host` header names anything but `127.0.0.1:<port>`,
  `localhost:<port>`, `[::1]:<port>` or the `--listen` address is `403`, token or not: that is a
  web page reaching this device through DNS rebinding.
- `/bytes/`, `/subtitles/` and `/relay/` take neither: rex and ffprobe read them. `/subtitles/`
  has a Host rule of its own ([subtitles](#subtitles)).

### `GET /api/status`

```json
{"version": "0.1.0", "rex": {"url": "http://127.0.0.1:32400", "reachable": true},
 "addons": [{"id": "internet-archive", "name": "Internet Archive", "version": "1.1.0",
             "description": "...", "enabled": true, "repo": "github-com-debridmediamanager-rex-addons",
             "hosts": ["archive.org", "*.archive.org"], "granted_hosts": [],
             "state": "idle", "items": 100, "held": 0, "last_sync": 1794326938, "error": null,
             "error_kind": null, "warnings": {"count": 0, "first": []},
             "update_available": false, "update_hosts": null, "update_host_settings": null,
             "sign_in": null, "requires": [], "provides": ["content"]}]}
```

| Field | Meaning |
|---|---|
| `state` | `syncing`, `error` (the last sync failed, or the addon can't run until updated: `error` says why), or `idle` |
| `error_kind` | `would_remove` when the last sync failed because it would have removed more than half of the addon's titles, refused by rex's snapshot guard or, for changes only, by rex-addons (offer a forced sync), `sign_in` when it failed because the addon needs signing in (offer that), `update` when its manifest asks for a sign-in kind this rex-addons doesn't have, such as 0.8's `spotify`: it never syncs, plays or signs in, its titles stay in rex, and `error` is `this addon needs an update: update <name> in its repository` (offer the update), else `null` |
| `items` | Titles in rex, probed, after the last sync that worked (one of changes only included) |
| `warnings` | Entries the last sync left out, which show nowhere else: `count`, every one of them, and `first`, the messages of the first five, such as `items(): "a" has the path "/a.mp4", which is not relative; left out` or `rex rejected "a": ...`. `{"count": 0, "first": []}` when none was. The next sync that runs `items()` replaces them |
| `held` | Titles sent without a probe, which rex holds unshown (no ffprobe, or one that failed). A title whose probe failed is probed again after one sync interval (`refresh_minutes`), doubled for each failure after that, at most a week; new settings or a new version of the addon try them all again at the next sync |
| `last_sync` | Unix seconds of the last sync that worked, or `null` |
| `repo` | The id of the repository it was installed from, or `null` for a bundled one. It stays when that repository is removed |
| `hosts` | The hosts its installed manifest lets it reach |
| `granted_hosts` | What the person typed into its `host` settings grants, as `host:port`, each reachable on that port only, private network included: `["192.168.1.20:8096", "nas.local:80", "nas.local:443", "[fd00::5]:8920"]`. `[]` when none is set. What it reaches now is `hosts` and these |
| `update_available` | Its repository offers a higher `version` |
| `update_hosts` | When `update_available`, the hosts that version would reach, so the app can ask before an update widens them; `null` otherwise |
| `update_host_settings` | When `update_available`, the labels of the `host` settings that version adds (a setting it turns into one counts, and its saved value is cleared), such as `["Server"]`; `[]` when none, `null` otherwise. Each is an address the person may be asked for, which the addon would then reach |
| `provides` | What it does, by the functions its `main.js` exports: `content` (`items` and `open`: titles), `subtitles` (`subtitles` and `subtitle`: it finds subtitles) and `translate` (it translates them), in that order, one or more. An addon without `content` never syncs titles (`items` stays `0`); its sync only registers it with rex ([what rex reads](#what-rex-reads)) |
| `requires` | One entry per addon its manifest `requires`: `{"id": "youtube", "ok": true, "reason": null}`, or `ok: false` with `reason` `"not installed"`, `"disabled"` or `"sign in needed"`, for the app to say what to do first. `[]` for an addon that requires none |
| `sign_in` | `null` for an addon that needs no account. Else `{"label": "Sign in to Example", "kind": "code", "account": "name" or null, "pending": {"code": "ABCD-EFGH", "url": "https://example.com/device", "expires_at": 1794330057} or null, "error": "..." or null}`: `label` for the button; `kind` how it signs in, always `code` but for an addon whose `error_kind` is `update` (treat any other kind as that); `account` the name it is signed in as; `pending`, while a sign-in waits, the code to show and the address to type it at, until `expires_at` (Unix seconds); `error` why the last sign-in failed or the website signed it out. An addon with `sign_in` and no `account` doesn't sync (its error is `sign in first`, kind `sign_in`) and isn't retried, and its titles answer `502` to rex. Read the addon every couple of seconds while `pending` is set: it turns `null` with `account` set once the person entered the code, or with `error` set when the sign-in failed or expired |

### `GET /api/addons/{id}`

The status fields above, plus `manifest` (as installed), `deleted` (how many of its titles a
person deleted in a Plex app, which it no longer pushes) and `config`: every setting with its
saved value or default, a set `secret` answered as `"********"`. How to show each setting is in
`manifest.config`, in order: its `key`, `label` and `type`, and as the manifest gives them its
`default`, `required`, `help` (a line to show with it) and, for a `select`, `options`
(`[{"value": "1080", "label": "1080p"}, ...]`: show the `label`s, send the `value`). A `host`
setting is an address the person types, answered as typed: show `granted_hosts`, which the addon
will reach. A `list` the manifest marks
`"secret": true` (the stremio addon's `addons`, whose links carry account tokens) is answered
entry by entry: an http(s) link as `<scheme>://<host>/********`, anything else as `"********"`.

### `POST /api/addons`

`{"repo": "<repo id>", "addon": "<addon id>"}` installs an addon from a repository added before,
as that repository's last fetch holds it, or updates it when it's installed, keeping its
settings and storage. `main.js` must load and export one whole group or more: `items` and `open`,
`subtitles` and `subtitle`, `translate`. Answers `201` (installed)
or `200` (updated) with the addon as `GET /api/addons/{id}` does, and starts a sync. `404` for a
repository or addon it doesn't know, `409` when an addon of that id is installed from another
repository that still offers it (remove it first), `400` when its files are refused. A bundled
addon, or one its repository no longer offers (it moved), installed from a repository becomes that
repository's.

### `DELETE /api/addons/{id}`

Removes the addon and, with rex's two-step source delete, its source `addon-<id>` and every
title it pushed. `{"removed": true, "rex_removed": 100}`; when rex can't be reached the addon is
still removed and the answer carries `rex_error`. A bundled addon removed stays removed.

### `PUT /api/addons/{id}/config`

A JSON object of settings. Each key given replaces its value, `null` puts back the default, a
`secret` sent as `"********"` keeps what's saved, and keys not given are kept. In a secret list,
an entry sent back as it was answered keeps the saved entry it stands for (the first one not yet
kept, so two links on one host both stay, in the order sent); any other entry is taken as typed.
So the app can remove, reorder and add entries without ever seeing the saved ones. A key the manifest
doesn't list, or a value of the wrong type, is `400`, as is a `select` value that isn't one of its
options (`Highest quality: must be one of 1080, 720`). A `host` setting takes text: blank (no
address), `http(s)://name-or-ip[:port][/path]` or `name-or-ip[:port]`, an IPv6 address in `[ ]`.
It grants its host on the port typed; with none, its scheme's (80 for `http://`, 443 for
`https://`), and with neither a scheme nor a port both 80 and 443. Anything else is `400` with one
of, after the label (`Server: ...`):

| Error | For |
|---|---|
| `give an address such as 192.168.1.20:8096 or http://nas.local:8096` | Not one address: `*`, `*.lan`, spaces, `_`, an empty host, port 0 or over 65535 |
| `give an http:// or https:// address` | Another scheme, such as `ftp://` |
| `leave the user name and password out of the address` | `user:password@nas` |
| `that is this device, which addons never reach` | `127.0.0.1` and all of 127/8 however written (`127.1`, `2130706433`), `localhost`, `*.localhost`, `[::1]`, `[::ffff:127.0.0.1]`, `0.0.0.0`, `[::]` |
| `must be an address, as text` | Not text |

A name that is accepted but leads to this device is refused when the addon connects.

Answers `{"config": ...}` as `GET` shows it
and starts a sync, which may remove more than half of the addon's titles (new settings can).

### `GET /api/addons/{id}/log`

The addon's last log lines, oldest first: what it printed (`[addon <id>]`) and the host's lines
about it (`[<id>]`), each exactly as logged, secrets masked. `?lines=N` answers the last `N`; at
most the last 300 are kept, in memory only, each cut to 4 KB.

```json
{"lines": [{"at": 1794326938, "line": "[addon my-films] probed in 3525 ms"},
           {"at": 1794326938, "line": "WARN [my-films] items(): \"a\" repeats an id or a path; left out"}]}
```

`at` is Unix seconds. `400` when `lines` isn't a whole number, `404` for an addon it doesn't know.

### `POST /api/addons/{id}/sync`

Starts a sync now: `202 {"syncing": true}`, also when one is already running. `409` when the addon
is disabled. Body `{"force": true}` lets this sync remove more than half of the addon's titles,
for when rex refused one that was right (`error_kind` is `would_remove`), and asks the addon for
every title. Force is for that one
attempt: if it fails for another reason, the automatic retries are not forced.

### `POST /api/addons/{id}/restore`

Brings back every title deleted in a Plex app: empties the addon's deleted list, saves it, and
starts a sync (when the addon is enabled; a disabled one gets them on its next sync). Answers the
addon as `GET` does, with `deleted` at `0`; `404` for an addon it doesn't know.

### `POST /api/addons/{id}/sign-in`

Starts a sign-in through the addon's `signIn()`, replacing one still waiting, and answers `200`
with the addon as `GET` does, `sign_in.pending` holding the code. rex-addons then checks with the
website by itself; once the person entered the code, `sign_in.account` is set and a sync starts.
`400` for an addon with no `sign_in`, or with the update message for one whose `error_kind` is
`update`; `409` when it is disabled, `502` with the addon's message when `signIn()` failed.
Disabling, removing or signing out the addon stops a sign-in still waiting.

### `DELETE /api/addons/{id}/sign-in`

Signs it out: forgets the account, any sign-in still waiting and any sign-in an older
rex-addons kept for it, and calls the addon's `signOut()` if it has one. Its titles stay in rex and its settings stay. Answers the addon as `GET` does;
`400` for an addon with no `sign_in`.

### `POST /api/addons/{id}/enabled`

`{"enabled": false}` stops syncing it and serving its bytes; its titles stay in rex.
`{"enabled": true}` syncs it right away. Answers the addon as `GET` does.

### `GET /api/repos`

Every repository added, as a list:

```json
[{"id": "github-com-debridmediamanager-rex-addons", "url": "https://github.com/debridmediamanager/rex-addons",
  "name": "rex-addons", "description": "...", "commit": "1dd7a645...", "last_fetch": 1794330057, "error": null,
  "addons": [{"id": "internet-archive", "name": "Internet Archive", "version": "1.1.0", "description": "...",
              "hosts": ["archive.org", "*.archive.org"],
              "installed_version": "1.0.0", "installed_hosts": ["archive.org"], "installed_repo": null, "compatible": true,
              "update_available": true}]}]
```

| Field | Meaning |
|---|---|
| `id` | The address's host and path as a slug |
| `name`, `description` | Its `repository.json`'s, else the last part of its address |
| `commit`, `last_fetch` | What the last fetch that worked got, and when (Unix seconds); `null` before one |
| `error` | Why the last fetch failed, or `no addons in it`; `null` when it worked. A failed fetch keeps what the one before got |
| `addons[].installed_version` | The version installed of that addon id, from wherever, or `null` |
| `addons[].installed_hosts` | The hosts the installed one reaches, or `null` when none is installed. Compare with `hosts` before installing: an update may widen them |
| `addons[].installed_repo` | The repository the installed one came from, or `null` when none is installed or it is bundled. Installing from another repository than this is `409` while that one still offers it; installing over a bundled one, or one its repository no longer offers, moves it to this repository, which the app should confirm |
| `addons[].compatible` | `false` when the addon's manifest names a `host` newer than this rex-addons: installing it answers `400` with the reason |
| `addons[].update_available` | Installing it now would update it: it's installed from this repository (or bundled, or from a repository that no longer offers it) and this version is higher |

### `POST /api/repos`

`{"url": "https://host/owner/name[.git][#branch]"}` fetches the repository (its newest commit on
the default branch, or on `#branch`) and answers `201` with it as `GET /api/repos` lists it. `400`
with a readable reason when the address isn't https, isn't a public git repository, or holds no
addon; nothing is kept then. The same address again refetches it (`200`); another address for
the same host and path is `409`.

A fetch is git's own: a shallow fetch of one commit over smart HTTP (protocol v2, or v0 from an
older server). No login, no SSH. It refuses a repository over 32 MB to download or holding over
2,000 files or 64 MB, and any path that would leave its folder; symbolic links and submodules are
never written.

### `POST /api/repos/{id}/refresh`

Fetches it again and answers it as `GET /api/repos` lists it, with `error` set when that failed.
Installed addons are not updated by it: `update_available` says which have a newer version.

rex-addons also does this by itself, in the background: a repository whose last fetch that
worked is a day old is fetched again, and one whose fetches keep failing is tried once an hour.
That only refreshes the listing; nothing is ever installed or updated without a `POST /api/addons`.

### `GET /api/repos/{id}/addons`

Its addons, as in `GET /api/repos`.

### `DELETE /api/repos/{id}`

Removes it and its files: `{"removed": true}`. Addons installed from it stay, with no more updates.

## What rex reads

Each addon registers with rex as the source `addon-<id>` with the byte URL
`http://<listen>/bytes/<id>`, and pushes its titles over rex's feed API. One that finds or
translates subtitles also says where rex asks it for them, and what it does:

```json
{"url": "http://127.0.0.1:32410/bytes/example-subs",
 "subtitles": {"url": "http://127.0.0.1:32410/subtitles/example-subs", "search": true, "translate": false}}
```

`search` is the `subtitles` group, `translate` the `translate` one. One with no titles registers
and sends rex none, at each sync (`refresh_minutes`), so rex finds it once rex answers.

rex reads a title at `GET /bytes/<id>/<path>`: rex-addons asks the addon's `open()` and answers `302` to the link, or,
when the link needs headers, relays it with the client's `Range`. When `open()` wrote HLS
playlists, the path answers the master playlist, which names each media playlist as
`<file name>/<name>.m3u8`, served at `GET /bytes/<id>/<path>/<name>.m3u8`; their links point
straight at the addon's hosts. rex's `DELETE` on the same path
(a person deleting the title in a Plex app) answers `204`, and the title is never pushed again.
The byte paths answer this device and private networks only, as rex's feed does.

`/relay/<token>/...` is what ffprobe reads while an addon's `ffprobe()` runs: the link fetched
through that addon's sandboxed client, or the playlists the addon wrote (`master.m3u8` and each
by its name), their links pointed through the relay. It answers this device only, and only during
that probe.

### Subtitles

What rex asks an addon that `provides` `subtitles` or `translate`
([subtitle addons](writing-addons.md#subtitle-addons)), at `http://<listen>/subtitles/<id>/...`. These
answer this device and private networks only, as the byte paths do, and never a web page: a
`Host` that is a name rather than an address or `localhost` (DNS rebinding), and a request that
carries `Origin` or a `Sec-Fetch-Site` other than `none` (a page's `<img>` or fetch), are `403`,
as they could spend the person's quota at a website or their translation account. Errors are
`{"error": "..."}`:
`404` for an addon it doesn't know or one without that group (`Example subtitles translates nothing:
its main.js exports no translate()`), `409` for one disabled, not signed in, with a required setting
empty (`set API key first`) or waiting for an update, `400` for a question asked wrongly, `502` when
the addon failed or returned what can't be used (its message), `504` when it ran out of time.

#### `GET /subtitles/{id}/search`

`?type=movie|episode&imdb=tt0032599&tmdb=&season=&episode=&lang=en,pt-BR&hash=&size=&filename=`.
`type` and one of `imdb`, `tmdb`, `hash` or `filename` are needed; an episode needs `season` and
`episode` (its `imdb` and `tmdb` are the show's), a movie has neither. `lang` is language codes,
comma-separated or repeated, best first; `hash` the OpenSubtitles hash, in hex. The addon's
`subtitles()` gets them as `{type, imdb, tmdb, season, episode, hash, size, filename, languages}`,
those given. It has 8 s in all.

```json
{"candidates": [{"id": "3197651", "lang": "pt-BR", "release": "His.Girl.Friday.1940.1080p.BluRay.x264",
                 "provider": "SubDL", "format": "srt", "hi": false, "forced": false, "fps": null, "downloads": 1200}],
 "truncated": false, "left_out": 0}
```

`lang` is a code with its case fixed (`pt-BR`), `format` one of `srt`, `ass`, `ssa`, `vtt`, `sub` or
`null` (not known before it is fetched), `release`, `provider`, `fps` and `downloads` `null` when
the addon didn't say. `truncated` is `true` when the addon returned more than the 200 kept;
`left_out` counts entries that couldn't be used. The same question is answered from the cache for
12 hours.

#### `GET /subtitles/{id}/file/{candidate}`

The candidate's id percent-encoded (a `/` in it as `%2F`); `?season=&episode=` for the episode
wanted from a season's archive and `?lang=` for the language it is in, which picks the older
encoding a file in neither UTF-8 nor UTF-16 is read in (give the candidate's `lang`). Answers `200`
with the subtitle as `text/plain; charset=utf-8`, its format in `X-Subtitle-Format` (`srt`, `ass`,
`ssa`, `vtt` or `sub`), as the file has it, line endings included. The same file is answered from
the cache for 7 days.

#### `POST /subtitles/{id}/translate`

`Content-Type: application/json` (anything else is `415`, so a web page can't send it), at most
2 MB (`413`):

```json
{"from": "en", "to": "my", "title": "His Girl Friday",
 "lines": [{"id": "1", "text": "Walter, I'm getting married.\nTomorrow."}]}
```

`to` is a language code, `from` one or `null`, `title` optional. `lines` are each subtitle's text,
`\n` between its rows, by an id of your own (text, each once, at most 64 bytes): at most 10,000 of
them and 1 MB of text. Answers `202 {"job": "<32 hex>"}`; the same lines already translated answer
`200` at once, as `GET` would answer the job done. The same lines asked for while their job waits or
runs answer that job. Each addon runs one job at a time, the others waiting, at most 16 (`429`).

#### `GET /subtitles/{id}/translate/{job}`

```json
{"job": "9f2c...", "state": "running", "done": 200, "total": 1543}
{"job": "9f2c...", "state": "done", "done": 1543, "total": 1543,
 "lines": [{"id": "1", "text": "..."}], "kept": [{"id": "57", "why": "its translation is not in Myanmar script"}]}
{"job": "9f2c...", "state": "error", "done": 200, "total": 1543, "error": "translate() failed 10 times in a row, last with: the API key was refused"}
```

`state` is `queued`, `running`, `done` or `error`; `done` counts the lines finished, a batch at a
time. `lines` come in the order sent, by the same ids. `kept` lists lines that kept their own text
(left out, or thrown on, 3 times) or the addon's in the wrong script (3 times), with why; one
that would keep more than half is an error instead, and kept nowhere. A job answers for an hour
after it ended, at most 32 an addon; a done one, from disk, for 30 days. Nothing needs the client
to stay connected: ask again whenever. `404` for a job it doesn't know.
