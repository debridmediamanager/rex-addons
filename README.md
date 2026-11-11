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

Repositories are checked for updates once a day (or refresh one yourself); an addon with a newer version says so, and installing
it again updates it with your settings kept.

## Addons in this repository

| Addon | What it brings |
|---|---|
| Internet Archive | Public domain feature films from archive.org: silents, classics, noir, B-movies. No setup, and it comes with the app |
| Stremio addons | Films and series from your Stremio addons' catalogs, played from your stream addons' links |
| YouTube (360p) | Your YouTube playlists (Liked videos, Watch later, your own) and any playlist you link, each a playlist in Plex, played from YouTube at 360p |

## YouTube (360p)

Sign in, and your Liked videos, Watch later and your own playlists show up in a library called
YouTube: each channel is a show, each video one of its episodes with YouTube's title, date and
picture. Each YouTube playlist is a playlist in Plex, in YouTube's order. A video in several
playlists is there once. Links to anyone's public or unlisted playlists go in the settings, as do
how many videos to take from each playlist (the first ones in its order, which for Liked videos
are the newest; 200 by default) and which of yours to take.

- **Signing in is required.** The app shows a code; enter it at google.com/device on your phone.
  It is YouTube's TV sign-in, and the addon talks to YouTube as YouTube's TV app does. Signing out
  in the app revokes it.
- **360p.** YouTube now hands out one plain file per video, the 360p MP4 with picture and sound
  together; every sharper version comes only through YouTube's own streaming protocol, which only
  YouTube's apps play. So videos play at 360p. Where YouTube offers an HLS stream instead (rare),
  the best one up to the resolution in the settings plays.
- **Nothing is downloaded.** rex reads the file from YouTube when you press play. To make YouTube's
  links work, the addon fetches YouTube's player script (about 2.5 MB) when YouTube changes it and
  keeps the small part of it that unlocks a link. Each video is checked once (about 0.3 MB) before
  it shows; a sync checks up to 30, an hour apart, so a long list fills in over a few hours.
  Playlists are read again every six hours.
- **Limits:** private, deleted, members-only and live videos are left out. No descriptions: YouTube's
  TV app is not given them. A video's link lasts six hours, so each play asks YouTube for a fresh one.

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
- Deleting a title in a Plex app hides it; the addon won't bring it back unless you restore its
  deleted titles in the app.
- If a sync would take away more than half of an addon's titles, rex keeps them all and the addon
  shows an error. That guards against a website having a bad day. If you meant it, sync with force.
  Changing an addon's settings does that for you.

Writing your own addon, and making a repository of them: [docs/writing-addons.md](docs/writing-addons.md).
The app's API: [docs/api.md](docs/api.md).

## License

MIT
