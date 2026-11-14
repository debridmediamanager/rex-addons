# rex-addons

Small JavaScript addons that bring things into [rex](https://rex.debridmediamanager.com), and so
into every Plex app: public domain films from the Internet Archive, or anything you write yourself. They run in
[rex for Android](https://notes.debridmediamanager.com/rex/android/), free, and in
[zurg+rex](https://notes.debridmediamanager.com/zurg/guides/android/), a Plex server on your phone for sponsors that also
connects your debrid and Usenet library to rex, or beside rex on a computer. Nothing is downloaded. When you press play, the addon
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

Repositories are checked for updates once a day (or refresh one yourself); an addon with a newer version says so, and installing
it again updates it with your settings kept.

## Addons in this repository

| Addon | What it brings |
|---|---|
| Internet Archive | Public domain feature films from archive.org: silents, classics, noir, B-movies. No setup, and it comes with the app |

More addons, for your Stremio addons' catalogs, your YouTube playlists and your Spotify library, are
in [rex-community-addons](https://github.com/debridmediamanager/rex-community-addons): add that
repository in the app.

## Running it yourself

Next to rex, with ffprobe (part of FFmpeg) installed:

```bash
cargo build --release
target/release/rex-addons --data ~/rex-addons --ffprobe "$(command -v ffprobe)"
curl -X POST http://127.0.0.1:32410/api/repos -d '{"url": "https://github.com/debridmediamanager/rex-community-addons"}'
curl -X POST http://127.0.0.1:32410/api/addons -d '{"repo": "github-com-debridmediamanager-rex-community-addons", "addon": "stremio"}'
curl -X PUT http://127.0.0.1:32410/api/addons/stremio/config \
  -d '{"addons": ["https://v3-cinemeta.strem.io/manifest.json", "<your stream addon'"'"'s manifest link>"]}'
```

It talks to rex at `http://127.0.0.1:32400` unless you give `--rex`. rex shows nothing it hasn't
been told the format of, so every title is checked once with ffprobe.

## Good to know

- Each addon is its own source in rex (`addon-<id>`). Removing an addon removes its titles from rex.
- Removing a repository keeps the addons you installed from it; they just get no more updates.
- Deleting a title in a Plex app hides it; the addon won't bring it back unless you restore its
  deleted titles in the app.
- If a sync would take away more than half of an addon's titles, rex keeps them all and the addon
  shows an error. That guards against a website having a bad day. If you meant it, sync with force.
  Changing an addon's settings does that for you.

The tests read rex-community-addons' addons from the `community` submodule: clone with
`--recurse-submodules`, or run `git submodule update --init`, before `cargo test`.

Writing your own addon, and making a repository of them: [docs/writing-addons.md](docs/writing-addons.md).
The app's API: [docs/api.md](docs/api.md).

## License

MIT
