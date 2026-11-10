# Running rex-addons and its API

What an app (or a script) needs to run rex-addons and manage its addons. `cargo build --release`
builds it for this machine; `./build-android.sh` builds `target/android/<abi>/librexaddons.so`
for arm64-v8a, armeabi-v7a and x86_64 with the Android NDK (`ABIS=` and `OUT=` narrow and move
that).

## The command

```
rex-addons --listen 127.0.0.1:32410 --rex http://127.0.0.1:32400 --data <dir> [--ffprobe <path>] [--bundled <dir>]
```

| Flag | Default | Meaning |
|---|---|---|
| `--listen` | `127.0.0.1:32410` | Where the API and the byte paths answer. This address is also what each addon registers with rex as its byte URL (`0.0.0.0` registers as `127.0.0.1`) |
| `--rex` | `http://127.0.0.1:32400` | rex. rex-addons waits for it: nothing syncs, and nothing counts as failed, until it answers |
| `--data` | `./rex-addons-data` | `<dir>/addons/<id>/` holds each installed addon: `manifest.json`, `main.js`, `config.json`, `storage.json`, and the host's own `state.json`, `probes.json`, `index.json`. `<dir>/removed.json` lists bundled addons the person removed |
| `--ffprobe` | none | The ffprobe each new title is checked with, once. Without it every title reaches rex unprobed, and rex holds unprobed titles without showing them |
| `--bundled` | none | Addon folders installed on the first start, and updated whenever the bundled `version` is higher. A person can remove one; it then stays removed |

`--version` and `--help` print and exit. Logs go to stdout (info) and stderr (`WARN`, `ERROR`),
one line each, tagged `[host]`, `[<addon id>]` or `[addon <id>]` for what the addon itself printed.

## The API

JSON in and out. It answers only this device: the socket's own peer address must be loopback
(headers are never read for that); anything else gets `403`. Errors are `{"error": "..."}`.

### `GET /api/status`

```json
{"version": "0.1.0", "rex": {"url": "http://127.0.0.1:32400", "reachable": true},
 "addons": [{"id": "internet-archive", "name": "Internet Archive", "version": "1.0.0",
             "description": "...", "enabled": true, "state": "idle", "items": 100, "held": 0,
             "last_sync": 1794326938, "error": null}]}
```

| Field | Meaning |
|---|---|
| `state` | `syncing`, `error` (the last sync failed: `error` says why), or `idle` |
| `items` | Titles in rex from the last sync, probed |
| `held` | Titles sent without a probe, which rex holds unshown (no ffprobe, or one that failed; retried next sync) |
| `last_sync` | Unix seconds of the last sync that worked, or `null` |

### `GET /api/addons/{id}`

The status fields above, plus `manifest` (as installed) and `config`: every setting with its
saved value or default, a set `secret` answered as `"********"`.

### `POST /api/addons`

`{"url": "https://.../manifest.json"}` installs from a link; `main.js` is fetched from beside it.
The manifest is checked, and `main.js` must load and export `items` and `open`. An addon
already installed is updated and keeps its settings and storage. Answers `201` with the addon
as `GET /api/addons/{id}` does, and starts a sync; `400` with why it was refused.

### `DELETE /api/addons/{id}`

Removes the addon and, with rex's two-step source delete, its source `addon-<id>` and every
title it pushed. `{"removed": true, "rex_removed": 100}`; when rex can't be reached the addon is
still removed and the answer carries `rex_error`. A bundled addon removed stays removed.

### `PUT /api/addons/{id}/config`

A JSON object of settings. Each key given replaces its value, `null` puts back the default, a
`secret` sent as `"********"` keeps what's saved, and keys not given are kept. A key the manifest
doesn't list, or a value of the wrong type, is `400`. Answers `{"config": ...}` as `GET` shows it
and starts a sync, which may remove more than half of the addon's titles (new settings can).

### `POST /api/addons/{id}/sync`

Starts a sync now: `202 {"syncing": true}`, also when one is already running. `409` when the addon
is disabled. Body `{"force": true}` lets this sync remove more than half of the addon's titles,
for when rex refused one that was right (the addon's `error` says so).

### `POST /api/addons/{id}/enabled`

`{"enabled": false}` stops syncing it and serving its bytes; its titles stay in rex.
`{"enabled": true}` syncs it right away. Answers the addon as `GET` does.

## What rex reads

Each addon registers with rex as the source `addon-<id>` with the byte URL
`http://<listen>/bytes/<id>`, and pushes its titles over rex's feed API. rex reads a title at
`GET /bytes/<id>/<path>`: rex-addons asks the addon's `open()` and answers `302` to the link, or,
when the link needs headers, relays it with the client's `Range`. rex's `DELETE` on the same path
(a person deleting the title in a Plex app) answers `204`, and the title is never pushed again.
The byte paths answer this device and private networks only, as rex's feed does.
