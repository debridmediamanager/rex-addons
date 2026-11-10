# Running rex-addons and its API

What an app (or a script) needs to run rex-addons and manage its addons. `cargo build --release`
builds it for this machine; `./build-android.sh` builds `target/android/<abi>/librexaddons.so`
for arm64-v8a, armeabi-v7a and x86_64 with the Android NDK (`ABIS=` and `OUT=` narrow and move
that).

## The command

```
rex-addons --listen 127.0.0.1:32410 --rex http://127.0.0.1:32400 --data <dir> [--ffprobe <path>] [--bundled <dir>] [--default-repo <url>]...
```

| Flag | Default | Meaning |
|---|---|---|
| `--listen` | `127.0.0.1:32410` | Where the API and the byte paths answer. This address is also what each addon registers with rex as its byte URL (`0.0.0.0` registers as `127.0.0.1`) |
| `--rex` | `http://127.0.0.1:32400` | rex. rex-addons waits for it: nothing syncs, and nothing counts as failed, until it answers |
| `--data` | `./rex-addons-data` | `<dir>/addons/<id>/` holds each installed addon: `manifest.json`, `main.js`, `config.json`, `storage.json`, and the host's own `state.json`, `probes.json`, `index.json`. `<dir>/repos/<repo id>/` holds each repository's fetched files, `<dir>/repos.json` the list. `<dir>/removed.json` lists bundled addons the person removed |
| `--ffprobe` | none | The ffprobe behind addons' `ffprobe()` and the host's own check of a title that arrives without `media_info`. Without it `ffprobe()` fails, those titles reach rex unprobed, and rex holds unprobed titles without showing them |
| `--bundled` | none | Addon folders installed on the first start, and updated whenever the bundled `version` is higher. A person can remove one; it then stays removed. Installing the same addon from a repository later makes it that repository's |
| `--default-repo` | none | A repository added the first time it is given, and fetched in the background; repeat it for several. One the person removed is never added again |

`--version` and `--help` print and exit. Logs go to stdout (info) and stderr (`WARN`, `ERROR`),
one line each, tagged `[host]`, `[<addon id>]` or `[addon <id>]` for what the addon itself printed.

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
- `/bytes/` and `/relay/` take neither: rex and ffprobe read them.

### `GET /api/status`

```json
{"version": "0.1.0", "rex": {"url": "http://127.0.0.1:32400", "reachable": true},
 "addons": [{"id": "internet-archive", "name": "Internet Archive", "version": "1.1.0",
             "description": "...", "enabled": true, "repo": "github-com-debridmediamanager-rex-addons",
             "hosts": ["archive.org", "*.archive.org"],
             "state": "idle", "items": 100, "held": 0, "last_sync": 1794326938, "error": null,
             "error_kind": null, "update_available": false, "update_hosts": null}]}
```

| Field | Meaning |
|---|---|
| `state` | `syncing`, `error` (the last sync failed: `error` says why), or `idle` |
| `error_kind` | `would_remove` when the last sync failed because rex refused a snapshot that would remove more than half of the addon's titles (offer a forced sync), else `null` |
| `items` | Titles in rex from the last sync, probed |
| `held` | Titles sent without a probe, which rex holds unshown (no ffprobe, or one that failed; retried next sync) |
| `last_sync` | Unix seconds of the last sync that worked, or `null` |
| `repo` | The id of the repository it was installed from, or `null` for a bundled one. It stays when that repository is removed |
| `hosts` | The hosts its installed manifest lets it reach |
| `update_available` | Its repository offers a higher `version` |
| `update_hosts` | When `update_available`, the hosts that version would reach, so the app can ask before an update widens them; `null` otherwise |

### `GET /api/addons/{id}`

The status fields above, plus `manifest` (as installed), `deleted` (how many of its titles a
person deleted in a Plex app, which it no longer pushes) and `config`: every setting with its
saved value or default, a set `secret` answered as `"********"`. A `list` the manifest marks
`"secret": true` (the stremio addon's `addons`, whose links carry account tokens) is answered
entry by entry: an http(s) link as `<scheme>://<host>/********`, anything else as `"********"`.

### `POST /api/addons`

`{"repo": "<repo id>", "addon": "<addon id>"}` installs an addon from a repository added before,
as that repository's last fetch holds it, or updates it when it's installed, keeping its
settings and storage. `main.js` must load and export `items` and `open`. Answers `201` (installed)
or `200` (updated) with the addon as `GET /api/addons/{id}` does, and starts a sync. `404` for a
repository or addon it doesn't know, `409` when an addon of that id is installed from another
repository (remove it first), `400` when its files are refused. A bundled addon installed from a
repository becomes that repository's.

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
doesn't list, or a value of the wrong type, is `400`. Answers `{"config": ...}` as `GET` shows it
and starts a sync, which may remove more than half of the addon's titles (new settings can).

### `POST /api/addons/{id}/sync`

Starts a sync now: `202 {"syncing": true}`, also when one is already running. `409` when the addon
is disabled. Body `{"force": true}` lets this sync remove more than half of the addon's titles,
for when rex refused one that was right (`error_kind` is `would_remove`). Force is for that one
attempt: if it fails for another reason, the automatic retries are not forced.

### `POST /api/addons/{id}/restore`

Brings back every title deleted in a Plex app: empties the addon's deleted list, saves it, and
starts a sync (when the addon is enabled; a disabled one gets them on its next sync). Answers the
addon as `GET` does, with `deleted` at `0`; `404` for an addon it doesn't know.

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
              "installed_version": "1.0.0", "installed_hosts": ["archive.org"], "installed_repo": null,
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
| `addons[].installed_repo` | The repository the installed one came from, or `null` when none is installed or it is bundled. Installing from another repository than this is `409`; installing over a bundled one moves it to this repository, which the app should confirm |
| `addons[].update_available` | Installing it now would update it: it's installed from this repository (or bundled) and this version is higher |

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
`http://<listen>/bytes/<id>`, and pushes its titles over rex's feed API. rex reads a title at
`GET /bytes/<id>/<path>`: rex-addons asks the addon's `open()` and answers `302` to the link, or,
when the link needs headers, relays it with the client's `Range`. rex's `DELETE` on the same path
(a person deleting the title in a Plex app) answers `204`, and the title is never pushed again.
The byte paths answer this device and private networks only, as rex's feed does.

`/relay/<token>/...` is what ffprobe reads while an addon's `ffprobe()` runs: the link fetched
through that addon's sandboxed client. It answers this device only, and only during that probe.
