# rex-addons

Small JavaScript addons that bring things into [rex](https://rex.debridmediamanager.com), and so
into every Plex app: public domain films from the Internet Archive, your Stremio addons'
catalogs, or anything you write yourself. Nothing is downloaded. When you press play, the addon
hands rex a link and rex streams from it.

rex-addons is the program that runs them. Each addon runs in its own sandbox: it can only reach
the websites its manifest lists, it gets a memory and time limit, and it can't touch your files.

## Addons

| Addon | What it brings | Install link |
|---|---|---|
| Internet Archive | Public domain feature films from archive.org: silents, classics, noir, B-movies. No setup | Comes with the app |
| Stremio addons | Films and series from your Stremio addons' catalogs, played from your stream addons' links | `https://raw.githubusercontent.com/debridmediamanager/rex-addons/main/addons/stremio/manifest.json` |

## Install an addon

In the app, add an addon by pasting its `manifest.json` link. Its settings show up right after.
Titles appear in your Plex apps as each one is checked.

Running rex-addons yourself, next to rex:

```bash
cargo build --release
target/release/rex-addons --data ~/rex-addons --ffprobe "$(command -v ffprobe)"
curl -X POST http://127.0.0.1:32410/api/addons \
  -d '{"url": "https://raw.githubusercontent.com/debridmediamanager/rex-addons/main/addons/stremio/manifest.json"}'
curl -X PUT http://127.0.0.1:32410/api/addons/stremio/config \
  -d '{"addons": ["https://v3-cinemeta.strem.io/manifest.json", "<your stream addon's manifest link>"]}'
```

It talks to rex at `http://127.0.0.1:32400` unless you give `--rex`. You need ffprobe (part of
FFmpeg): rex shows nothing it hasn't been told the format of, so every title is checked once.

## Good to know

- Each addon is its own source in rex (`addon-<id>`). Removing an addon removes its titles from rex.
- Deleting a title in a Plex app hides it for good; the addon won't bring it back.
- If a sync would take away more than half of an addon's titles, rex keeps them all and the addon
  shows an error. That guards against a website having a bad day. If you meant it, sync with force.
  Changing an addon's settings does that for you.

Writing your own addon: [docs/writing-addons.md](docs/writing-addons.md). The app's API:
[docs/api.md](docs/api.md).

## License

MIT
