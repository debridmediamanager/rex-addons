# The YouTube addon's exports

The [YouTube addon](../addons/youtube/main.js) lets other addons call two of its functions
(`"exports": ["findSong", "songAudio"]`), with the person's YouTube sign-in. The
[Spotify addon](../addons/spotify/main.js) plays its songs with them. How calling another addon
works: [Calling another addon](writing-addons.md#calling-another-addon).

```json
"host": "0.5",
"requires": ["youtube"]
```

| Function | What it does |
|---|---|
| `findSong({title, artists, album, duration_ms, isrc})` | A recording of the song on YouTube, or `null`: `{video_id, title, channel, duration_ms, kind, score, via, size, bitrate, mime, sample_rate, channels}`. `kind` is `song` for YouTube Music's own recording, `video` for an upload; the length, size and rates are its AAC file's, from one player call. A search and one or two player calls; ask for each song once and keep the answer |
| `songAudio(video_id)` | Its AAC file to play now: `{url, mime, bitrate, duration_ms, size}`, a plain file read with `Range`, its link good for six hours. One player call |

`songAudio()`'s link is on `*.googlevideo.com`. Your `open()` may return it only when that is in
**your** `hosts`, which is why the Spotify addon lists it.
