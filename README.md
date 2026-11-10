# rex-addons

Small JavaScript addons that bring things into [rex](https://rex.debridmediamanager.com), and so
into every Plex app: public domain films from the Internet Archive, your Stremio addons'
catalogs, or anything you write yourself. Nothing is downloaded. When you press play, the addon
hands rex a link and rex streams from it.

rex-addons is the program that runs them. Each addon runs in its own sandbox: it can only reach
the websites its manifest lists, it gets a memory and time limit, and it can't touch your files.

## Add a repository, install an addon

Addons come from **repositories**: any public git repository that holds them, on GitHub, GitLab,
Codeberg or your own server.

1. In the app, add a repository by pasting its address, such as
   `https://github.com/debridmediamanager/rex-addons`. Add `#branch` to the end for a branch
   other than the default one.
2. Pick the addons you want from its list and install them. Their settings show up right after.
3. Titles appear in your Plex apps as each one is checked.

Refresh a repository to see its updates; an addon with a newer version says so, and installing
it again updates it with your settings kept.

## Addons in this repository

| Addon | What it brings |
|---|---|
| Internet Archive | Public domain feature films from archive.org: silents, classics, noir, B-movies. No setup, and it comes with the app |
| Stremio addons | Films and series from your Stremio addons' catalogs, played from your stream addons' links |

## Running it yourself

Next to rex, with ffprobe (part of FFmpeg) installed:

```bash
cargo build --release
target/release/rex-addons --data ~/rex-addons --ffprobe "$(command -v ffprobe)"
curl -X POST http://127.0.0.1:32410/api/repos -d '{"url": "https://github.com/debridmediamanager/rex-addons"}'
curl -X POST http://127.0.0.1:32410/api/addons -d '{"repo": "github-com-debridmediamanager-rex-addons", "addon": "stremio"}'
curl -X PUT http://127.0.0.1:32410/api/addons/stremio/config \
  -d '{"addons": ["https://v3-cinemeta.strem.io/manifest.json", "<your stream addon'"'"'s manifest link>"]}'
```

It talks to rex at `http://127.0.0.1:32400` unless you give `--rex`. rex shows nothing it hasn't
been told the format of, so every title is checked once with ffprobe.

## Good to know

- Each addon is its own source in rex (`addon-<id>`). Removing an addon removes its titles from rex.
- Removing a repository keeps the addons you installed from it; they just get no more updates.
- Deleting a title in a Plex app hides it for good; the addon won't bring it back.
- If a sync would take away more than half of an addon's titles, rex keeps them all and the addon
  shows an error. That guards against a website having a bad day. If you meant it, sync with force.
  Changing an addon's settings does that for you.

Writing your own addon, and making a repository of them: [docs/writing-addons.md](docs/writing-addons.md).
The app's API: [docs/api.md](docs/api.md).

## License

MIT
