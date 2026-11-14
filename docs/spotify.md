# Listing a Spotify library

What the `spotify` global can ask Spotify for, for an addon with `"sign_in": {"kind": "spotify"}`.
How that sign-in works and what the global does is in
[Signing in with Spotify](writing-addons.md#signing-in-with-spotify). The
[Spotify addon](https://github.com/debridmediamanager/rex-community-addons/blob/main/addons/spotify/main.js) uses all of this.

These are Spotify's own, undocumented paths (`spclient`), as Spotify's desktop app uses them.
They can change without notice, so throw (and keep what rex has) when an answer isn't what you
expect.

## Paths

What lists a library (measured 2026-11-11 with a Premium account; each answered in 40 to 260 ms
and 20 track lookups in a row at 4 a second were all answered):

| Path | Gives |
|---|---|
| `GET /playlist/v2/user/{user}/rootlist?decorate=revision,length,attributes,timestamp,owner` | The person's playlists: `contents.items[].uri` (`spotify:playlist:<id>`, plus folder markers), and in `contents.metaItems[]` each one's `attributes.name`, `attributes.picture` and `length` |
| `GET /playlist/v2/playlist/{id}?decorate=length,attributes` | One playlist: `attributes.name`, `length`, and `contents.items[].uri` (tracks, episodes) with `attributes.timestamp`, when it was added. `contents.truncated` says when it holds only part (not seen with 82 tracks; untested past that) |
| `POST /collection/v2/paging` `{"username": user, "set": "collection", "limit": 300}` | Liked songs (`spotify:track:` uris) and saved albums (`spotify:album:` uris) together, with `added_at`, and a `sync_token`. Past `limit` the answer should carry a `next_page_token` to send back as `pagination_token` (librespot's protocol says so; untested). `"set": "artist"` lists followed artists |
| `GET /metadata/4/track/{gid}?market=from_token` | A track: `name`, `artist[].name`, `album` (`name`, `artist`, `date.year`, `cover_group.image[].file_id`), `number`, `disc_number`, `duration` (ms), `external_id` (`{"type": "isrc", "id": ...}`) |
| `GET /metadata/4/album/{gid}?market=from_token` | An album: `name`, `artist`, `date`, `cover_group`, and `disc[].track[].gid` |

`{user}` is `spotify.user()`. A cover is `https://i.scdn.co/image/<file_id>`.

## Track ids

`gid` is the id in hex; the uris carry it in base 62:

```js
const B62 = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const gid = (uri) => {
  let n = 0n;
  for (const c of uri.split(":").pop()) n = n * 62n + BigInt(B62.indexOf(c));
  return n.toString(16).padStart(32, "0");
};

export default {
  async items(ctx) {
    const user = spotify.user();
    const library = await spotify.post("/collection/v2/paging", { username: user, set: "collection", limit: 300 });
    const items = [];
    for (const liked of library.items.filter((item) => item.uri.startsWith("spotify:track:"))) {
      // A track doesn't change: look it up once.
      let track = storage.get(liked.uri);
      if (!track) {
        track = await spotify.get(`/metadata/4/track/${gid(liked.uri)}?market=from_token`);
        storage.set(liked.uri, { name: track.name, artist: track.artist[0].name, album: track.album.name, duration: track.duration });
        track = storage.get(liked.uri);
      }
      items.push(/* a rex music track, played from wherever you find it */);
    }
    return items;
  },

  async open(id) { /* the song's link, from wherever it plays */ },
};
```

## Not offered: the Web API

Spotify's public Web API (`api.spotify.com`) is not offered: it wants a developer app of the
person's own, and the session's tokens don't work there (measured: the old token service answers
`Invalid client`, and a login5 token gets `429` from its first call).
