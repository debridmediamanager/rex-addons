# Running rex-addons and its API

What an app (or a script) needs to run rex-addons and manage its addons. `cargo build --release`
builds it for this machine; `./build-android.sh` builds `target/android/<abi>/librexaddons.so`
for arm64-v8a, armeabi-v7a and x86_64 with the Android NDK (`ABIS=` and `OUT=` narrow and move
that).

## The command

```
rex-addons --listen 127.0.0.1:32410 --rex http://127.0.0.1:32400 --data <dir> [--ffprobe <path>] [--bundled <dir>] [--dev <dir>] [--default-repo <url>]... [--device-name <name>]
```

| Flag | Default | Meaning |
|---|---|---|
| `--listen` | `127.0.0.1:32410` | Where the API and the byte paths answer. This address is also what each addon registers with rex as its byte URL (`0.0.0.0` registers as `127.0.0.1`) |
| `--rex` | `http://127.0.0.1:32400` | rex. rex-addons waits for it: nothing syncs, and nothing counts as failed, until it answers |
| `--data` | `./rex-addons-data` | `<dir>/addons/<id>/` holds each installed addon: `manifest.json`, `main.js`, `config.json`, `storage.json`, and the host's own `state.json`, `probes.json`, `probe_failures.json`, `index.json`, and for a Spotify addon `spotify-credentials.json` (the sign-in, mode 600, never served or given to the addon). `<dir>/repos/<repo id>/` holds each repository's fetched files, `<dir>/repos.json` the list. `<dir>/removed.json` lists bundled addons the person removed |
| `--ffprobe` | none | The ffprobe behind addons' `ffprobe()` and the host's own check of a title that arrives without `media_info`. Without it `ffprobe()` fails, those titles reach rex unprobed, and rex holds unprobed titles without showing them |
| `--bundled` | none | Addon folders installed on the first start, and updated whenever the bundled `version` is higher. A person can remove one; it then stays removed. Installing the same addon from a repository later makes it that repository's |
| `--dev` | none | Addon folders installed on every start, whatever their `version`, and installed again, then synced (`ctx.reason` `install`), within a second of their `manifest.json` or `main.js` changing. For writing an addon; one it can't install is a `WARN [host] dev <folder>: ...` line and the last one installed stays |
| `--default-repo` | none | A repository added the first time it is given, and fetched in the background; repeat it for several. One the person removed is never added again |
| `--device-name` | `rex` | The name this device shows in the Spotify app while a Spotify addon pairs ([below](#spotify-pairing)) |

`--version` and `--help` print and exit. Logs go to stdout (info) and stderr (`WARN`, `ERROR`),
one line each, tagged `[host]`, `[<addon id>]` or `[addon <id>]` for what the addon itself printed.

```
rex-addons check <addon folder> [--config '<json>'] [--open <item id>] [--ffprobe <path>] [--data <dir>]
```

tries one addon folder without rex or a running rex-addons: the manifest, what installing checks,
one `items()` (`ctx.reason` `manual`) and, with `--open`, one `open()`, each printed as an `ok`,
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
- `/bytes/` and `/relay/` take neither: rex and ffprobe read them.

### `GET /api/status`

```json
{"version": "0.1.0", "rex": {"url": "http://127.0.0.1:32400", "reachable": true},
 "addons": [{"id": "internet-archive", "name": "Internet Archive", "version": "1.1.0",
             "description": "...", "enabled": true, "repo": "github-com-debridmediamanager-rex-addons",
             "hosts": ["archive.org", "*.archive.org"], "granted_hosts": [],
             "state": "idle", "items": 100, "held": 0, "last_sync": 1794326938, "error": null,
             "error_kind": null, "warnings": {"count": 0, "first": []},
             "update_available": false, "update_hosts": null, "update_host_settings": null,
             "sign_in": null, "requires": []}]}
```

| Field | Meaning |
|---|---|
| `state` | `syncing`, `error` (the last sync failed: `error` says why), or `idle` |
| `error_kind` | `would_remove` when the last sync failed because rex refused a snapshot that would remove more than half of the addon's titles (offer a forced sync), `sign_in` when it failed because the addon needs signing in (offer that), else `null` |
| `items` | Titles in rex from the last sync, probed |
| `warnings` | Entries the last sync left out, which show nowhere else: `count`, every one of them, and `first`, the messages of the first five, such as `items(): "a" has the path "/a.mp4", which is not relative; left out` or `rex rejected "a": ...`. `{"count": 0, "first": []}` when none was. The next sync that runs `items()` replaces them |
| `held` | Titles sent without a probe, which rex holds unshown (no ffprobe, or one that failed). A title whose probe failed is probed again after one sync interval (`refresh_minutes`), doubled for each failure after that, at most a week; new settings or a new version of the addon try them all again at the next sync |
| `last_sync` | Unix seconds of the last sync that worked, or `null` |
| `repo` | The id of the repository it was installed from, or `null` for a bundled one. It stays when that repository is removed |
| `hosts` | The hosts its installed manifest lets it reach |
| `granted_hosts` | What the person typed into its `host` settings grants, as `host:port`, each reachable on that port only, private network included: `["192.168.1.20:8096", "nas.local:80", "nas.local:443", "[fd00::5]:8920"]`. `[]` when none is set. What it reaches now is `hosts` and these |
| `update_available` | Its repository offers a higher `version` |
| `update_hosts` | When `update_available`, the hosts that version would reach, so the app can ask before an update widens them; `null` otherwise |
| `update_host_settings` | When `update_available`, the labels of the `host` settings that version adds (a setting it turns into one counts, and its saved value is cleared), such as `["Server"]`; `[]` when none, `null` otherwise. Each is an address the person may be asked for, which the addon would then reach |
| `requires` | One entry per addon its manifest `requires`: `{"id": "youtube", "ok": true, "reason": null}`, or `ok: false` with `reason` `"not installed"`, `"disabled"` or `"sign in needed"`, for the app to say what to do first. `[]` for an addon that requires none |
| `sign_in` | `null` for an addon that needs no account. Else `{"label": "Sign in to Example", "kind": "code", "account": "name" or null, "pending": {"code": "ABCD-EFGH", "url": "https://example.com/device", "expires_at": 1794330057} or null, "error": "..." or null}`: `label` for the button; `kind` how it signs in, `code` or `spotify`; `account` the name it is signed in as (for Spotify the account's display name, else its username); `pending`, while a sign-in waits, the code to show and the address to type it at, until `expires_at` (Unix seconds), or for `spotify` `{"device": "rex", "expires_at": ...}`: the name to pick in the Spotify app ([below](#spotify-pairing)); `error` why the last sign-in failed or the website signed it out. An addon with `sign_in` and no `account` doesn't sync (its error is `sign in first`, kind `sign_in`) and isn't retried, and its titles answer `502` to rex. Read the addon every couple of seconds while `pending` is set: it turns `null` with `account` set once the person entered the code, or with `error` set when the sign-in failed or expired |

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
for when rex refused one that was right (`error_kind` is `would_remove`). Force is for that one
attempt: if it fails for another reason, the automatic retries are not forced.

### `POST /api/addons/{id}/restore`

Brings back every title deleted in a Plex app: empties the addon's deleted list, saves it, and
starts a sync (when the addon is enabled; a disabled one gets them on its next sync). Answers the
addon as `GET` does, with `deleted` at `0`; `404` for an addon it doesn't know.

### `POST /api/addons/{id}/sign-in`

Starts a sign-in through the addon's `signIn()`, replacing one still waiting, and answers `200`
with the addon as `GET` does, `sign_in.pending` holding the code. rex-addons then checks with the
website by itself; once the person entered the code, `sign_in.account` is set and a sync starts.
`400` for an addon with no `sign_in`, `409` when it is disabled, `502` with the addon's message
when `signIn()` failed. Disabling, removing or signing out the addon stops a sign-in still waiting.

For a `spotify` addon there is no code: this device becomes discoverable on the local network for
10 minutes and `sign_in.pending` is `{"device": "<--device-name>", "expires_at": ...}`. Tell the
person to open Spotify on a phone on the same Wi-Fi, tap the devices button (the speaker icon) and
pick that name. Once they do, `pending` turns `null` with `account` set and a sync starts; if
nobody does, `pending` turns `null` with `error` `nobody picked rex in the Spotify app in time`.
`502` when the device cannot be announced on the network.

### `DELETE /api/addons/{id}/sign-in`

Signs it out: forgets the account and any sign-in still waiting (a pairing stops), a Spotify
addon's saved sign-in, and calls the addon's `signOut()` if it has one. Its titles stay in rex and its settings stay. Answers the addon as `GET` does;
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
| `addons[].installed_repo` | The repository the installed one came from, or `null` when none is installed or it is bundled. Installing from another repository than this is `409`; installing over a bundled one moves it to this repository, which the app should confirm |
| `addons[].compatible` | `false` when the addon's manifest names a `host` newer than this rex-addons: installing it answers `400` with the reason |
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

## Spotify pairing

Spotify has no code to type; it signs a device in when the person picks it in the Spotify app,
which is Spotify Connect's zeroconf pairing. While a Spotify addon's sign-in is pending
(at most 10 minutes), rex-addons:

- answers HTTP on **all interfaces, IPv6 and IPv4, on a free TCP port** (`getInfo` and `addUser`
  at `/`, nothing else). This is apart from the API, which stays on `--listen`, this device only.
- announces `_spotify-connect._tcp` over **mDNS with a responder of its own** (UDP 5353, the
  multicast groups 224.0.0.251 and ff02::fb, on every interface), named `--device-name`, its host
  `rex-<12 hex>.local`. The app does not advertise anything itself.
- stops both, and withdraws the record, when the pairing ends, however it ends (a Mac browsing
  with `dns-sd` dropped it within about a second).

It is never a player: it registers no Connect device with Spotify, so once paired it is in no
device list, and a play command has nothing to reach. Its `getInfo` says `accountReq: PREMIUM`,
as librespot's does; whether a free account can pick it is untested.

What an Android app has to do for this:

- Hold a `WifiManager.MulticastLock` (permission `CHANGE_WIFI_MULTICAST_STATE`) from the
  `POST .../sign-in` until `pending` turns `null`. Without one held by some app, Android may
  filter the mDNS queries the Spotify app sends, so the device is announced but its record is
  never answered for again.
- Keep `INTERNET` (already needed). The HTTP endpoint and 5353 need no other permission. The
  mDNS sockets are opened with address and port reuse, which should let them share 5353 with
  Android's own mDNS service.
- Show the device name and say: same Wi-Fi as the phone, open Spotify, tap the devices button,
  pick it. Keep reading the addon every couple of seconds while `pending` is set.
- Measured on 2026-11-11 with the binary run from `adb shell`, not inside an app: on a Pixel 6
  (Android 16, arm64-v8a) and a Fire TV (Android 9, armeabi-v7a) the record was announced, a fresh
  mDNS query from another machine was answered, `getInfo` answered over the network, and
  cancelling withdrew the record within about 2 s and closed the endpoint. Not measured: inside
  the app (its sandbox, with and without the lock), and whether newer Android's local-network
  permission is needed. Android names every device `localhost`, which is why the host name is
  `rex-<12 hex>.local` rather than the system's.

## What rex reads

Each addon registers with rex as the source `addon-<id>` with the byte URL
`http://<listen>/bytes/<id>`, and pushes its titles over rex's feed API. rex reads a title at
`GET /bytes/<id>/<path>`: rex-addons asks the addon's `open()` and answers `302` to the link, or,
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
