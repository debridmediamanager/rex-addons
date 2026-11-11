// Your Spotify library in rex, as music: liked songs, your playlists and saved albums, each track
// with Spotify's own title, artists, album, numbers, date and cover. Each Spotify playlist, and
// Liked Songs, is a rex playlist in Spotify's order. Spotify's audio is out of reach (rex-addons
// pairs with Spotify only to list the library), so each track plays as its recording on YouTube,
// found and played through the YouTube addon with its own sign-in.
//
// A track reaches rex once YouTube has a recording of it at its length: a few are looked for each
// sync (the setting), each one once for good; a track YouTube has none of is looked for again after
// a week. What rex is told about the file (AAC from YouTube, its length, size and rate) comes from
// that one look, so no track is probed.
//
// The library is read through Spotify's own API (the `spotify` global, spclient): the playlists
// list (`rootlist`), each playlist again only when its revision changed, liked songs and saved
// albums (`collection`), each in pages, and each track's details once.

const B62 = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
/** A Spotify id (base 62, the end of a `spotify:track:` uri) as the hex `gid` metadata takes. */
function gid(id) {
  let n = 0n;
  for (const c of id) {
    const digit = B62.indexOf(c);
    if (digit < 0) throw new Error(`${id} is not a Spotify id`);
    n = n * 62n + BigInt(digit);
  }
  return n.toString(16).padStart(32, "0");
}
/** A hex `gid` as a Spotify id. */
function idOf(hex) {
  let n = BigInt(`0x${hex}`), out = "";
  for (; n > 0n; n /= 62n) out = B62[Number(n % 62n)] + out;
  return out.padStart(22, "0");
}

// Spotify's pages: a playlist or the playlists list is read in windows of this many once Spotify
// says an answer holds only part of it (a 400-track playlist came whole, 2026-11-11), the
// collection this many a request.
const PAGE = 100;
const COLLECTION_PAGE = 300;
// Between two Spotify lookups, and between two YouTube searches.
const SPOTIFY_PAUSE_MS = 250;
const MATCH_PAUSE_MS = 1500;
// A track YouTube has no recording of is looked for again after this.
const RETRY_MS = 7 * 24 * 60 * 60 * 1000;
const LIKED = "Liked Songs";
// A playlist Spotify refuses is asked about again after this.
const GONE_MS = 24 * 60 * 60 * 1000;

const sp = async (path) => {
  await sleep(SPOTIFY_PAUSE_MS);
  return await spotify.get(path);
};

/** Every entry of a playlist (or of the playlists list, `rootlist`) at `path`, in order, following
 * Spotify's windows while it says the answer is cut short: {head, items, metaItems}. */
async function windows(path) {
  const head = await sp(`${path}${path.includes("?") ? "&" : "?"}decorate=revision,length,attributes,timestamp,owner`);
  if (!head?.contents) throw new Error(`Spotify's answer to ${path.split("?")[0]} has no contents`);
  // An empty playlist's contents have no items at all (measured 2026-11-11).
  const items = [...(head.contents.items ?? [])], metaItems = [...(head.contents.metaItems ?? [])];
  for (let truncated = head.contents.truncated; truncated;) {
    if (items.length > 20000) throw new Error(`${path.split("?")[0]}: more than 20,000 entries`);
    const page = await sp(`${path}${path.includes("?") ? "&" : "?"}decorate=revision,length,attributes,timestamp,owner&from=${items.length}&length=${PAGE}`);
    const got = page?.contents?.items ?? [];
    if (page?.contents?.pos !== items.length || !got.length) throw new Error(`Spotify answered ${path.split("?")[0]} from ${items.length} with nothing`);
    items.push(...got);
    metaItems.push(...(page.contents.metaItems ?? []));
    truncated = page.contents.truncated;
  }
  if (typeof head.length === "number" && items.length < head.length) {
    throw new Error(`${path.split("?")[0]}: Spotify listed ${items.length} of its ${head.length} entries`);
  }
  return { head, items, metaItems };
}

/** The person's playlists, folders flattened, in the order Spotify shows them: [{id, title, revision}]. */
async function playlists(user) {
  const { items, metaItems } = await windows(`/playlist/v2/user/${encodeURIComponent(user)}/rootlist`);
  const lists = items.filter((item) => item.uri?.startsWith("spotify:playlist:"));
  // Each playlist's details beside it, whether folders' markers have some (`start-group`, `end-group`)
  // or not; when neither lines up, no revision is known and every playlist is read again.
  const metas = metaItems.length === items.length ? items.map((item, at) => [item, metaItems[at]]).filter(([item]) => lists.includes(item)).map(([, meta]) => meta)
    : metaItems.length === lists.length ? metaItems : [];
  return lists.map((item, at) => ({ id: item.uri.split(":")[2], title: metas[at]?.attributes?.name ?? "", revision: metas[at]?.revision ?? null }));
}

/** A playlist's tracks, by Spotify id in its order, local files and podcast episodes left out. Read
 * again only when its revision changed. */
async function playlistTracks(playlist) {
  const kept = storage.get(`pl:${playlist.id}`);
  if (kept?.gone > Date.now()) return null;
  if (kept && !kept.gone && playlist.revision && kept.revision === playlist.revision) return kept;
  let read;
  try {
    read = await windows(`/playlist/v2/playlist/${playlist.id}`);
  } catch (error) {
    // A playlist the person follows that is gone or private now: listed with no details, and
    // answered 403 (measured 2026-11-11). It is left out, as Spotify's app shows none of it.
    if (!/^Spotify answered 40[34] /.test(error.message)) throw error;
    console.warn(`a playlist is left out for a day: ${error.message.split("\n")[0]}`);
    storage.set(`pl:${playlist.id}`, { gone: Date.now() + GONE_MS });
    return null;
  }
  const { head, items } = read;
  const listed = {
    revision: playlist.revision ?? head.revision ?? null, title: head.attributes?.name ?? playlist.title,
    tracks: items.map((item) => item.uri ?? "").filter((uri) => uri.startsWith("spotify:track:")).map((uri) => uri.split(":")[2]),
  };
  storage.set(`pl:${playlist.id}`, listed);
  return listed;
}

/** Liked songs (newest first, as Spotify shows them) and saved albums, every page. */
async function collection(user) {
  const liked = [], albums = [];
  let token = null;
  for (let page = 0; ; page++) {
    if (page > 500) throw new Error("the collection has more pages than Spotify should give");
    await sleep(SPOTIFY_PAUSE_MS);
    const answer = await spotify.post("/collection/v2/paging", { username: user, set: "collection", limit: COLLECTION_PAGE, ...(token ? { pagination_token: token } : {}) });
    if (!Array.isArray(answer?.items)) throw new Error("Spotify's collection answer has no items");
    for (const item of answer.items) {
      const [, kind, id] = (item.uri ?? "").split(":");
      if (kind === "track") liked.push({ id, at: Number(item.added_at) || 0 });
      else if (kind === "album") albums.push(id);
    }
    token = answer.next_page_token;
    if (!token || !answer.items.length) break;
  }
  return { liked: liked.sort((a, b) => b.at - a.at).map((one) => one.id), albums };
}

/** A saved album's tracks, by Spotify id, disc by disc; an album does not change, so once. */
async function albumTracks(id) {
  let kept = storage.get(`al:${id}`);
  if (!kept) {
    const album = await sp(`/metadata/4/album/${gid(id)}?market=from_token`);
    if (!Array.isArray(album?.disc)) throw new Error(`Spotify's answer for album ${id} has no discs`);
    kept = album.disc.flatMap((disc) => (disc.track ?? []).map((track) => idOf(track.gid)));
    storage.set(`al:${id}`, kept);
  }
  return kept;
}

const cover = (group) => {
  const images = group?.image ?? [];
  const best = images.find((image) => image.size === "LARGE") ?? images.sort((a, b) => (b.width ?? 0) - (a.width ?? 0))[0];
  return best?.file_id ? `https://i.scdn.co/image/${best.file_id}` : undefined;
};
const dated = (date) => date?.year ? [date.year, date.month, date.day].filter(Boolean).map((n, i) => String(n).padStart(i ? 2 : 4, "0")).join("-") : undefined;

/** A track's details, as Spotify has them, looked up once. */
async function details(id) {
  let kept = storage.get(`t:${id}`);
  if (!kept) {
    const track = await sp(`/metadata/4/track/${gid(id)}?market=from_token`);
    if (!track?.name || !track.album?.gid) throw new Error(`Spotify's answer for track ${id} has no name or album`);
    const albumArtist = track.album.artist?.[0] ?? track.artist?.[0] ?? {};
    kept = {
      title: track.name, artists: (track.artist ?? []).map((artist) => artist.name).filter(Boolean),
      disc: track.disc_number || 1, track: track.number || undefined, duration_ms: track.duration,
      isrc: (track.external_id ?? []).find((one) => one.type === "isrc")?.id,
      album: {
        id: idOf(track.album.gid), title: track.album.name, artist: albumArtist.name ?? "Unknown Artist",
        artist_gid: albumArtist.gid, date: dated(track.album.date), cover: cover(track.album.cover_group),
      },
    };
    storage.set(`t:${id}`, kept);
  }
  return kept;
}

/** An artist's picture, looked up once; "" for one Spotify has none of. */
async function portrait(artistGid) {
  if (!artistGid) return undefined;
  let kept = storage.get(`ar:${artistGid}`);
  if (kept === undefined) {
    try {
      kept = cover((await sp(`/metadata/4/artist/${artistGid}?market=from_token`))?.portrait_group) ?? "";
    } catch (error) {
      if (/^SIGN_IN_REQUIRED/.test(error.message)) throw error;
      console.warn(`an artist's picture: ${error.message}`);
      return undefined;
    }
    storage.set(`ar:${artistGid}`, kept);
  }
  return kept || undefined;
}

/** What ffprobe reads of YouTube's AAC file, from what the YouTube addon read off the player answer. */
function probe(audio) {
  const duration = (audio.duration_ms / 1000).toFixed(6);
  return {
    format: { format_name: "mov,mp4,m4a,3gp,3g2,mj2", nb_streams: 1, duration, size: String(audio.size), bit_rate: String(audio.bitrate) },
    streams: [{
      index: 0, codec_type: "audio", codec_name: "aac", profile: "LC", codec_tag_string: "mp4a",
      sample_rate: String(audio.sample_rate), channels: audio.channels, channel_layout: audio.channels === 1 ? "mono" : "stereo",
      duration, bit_rate: String(audio.bitrate), disposition: { default: 1 },
    }],
  };
}

// What the YouTube addon throws when it cannot be asked at all: the sync fails with it, as it says
// what to do. Anything else stops the looking for this sync.
const UNREACHABLE = /^(the addon youtube is not installed|YouTube is disabled|sign in to YouTube|YouTube exports no)/;

function settings(config) {
  return {
    liked: config.liked !== false, playlists: config.playlists !== false, albums: config.albums !== false,
    budget: Math.max(0, Math.min(500, Math.floor(Number(config.matches_per_sync ?? 30)))),
  };
}

export default {
  async items(ctx) {
    const wanted = settings(ctx.config);
    const user = spotify.user();

    // The library: each list's tracks in Spotify's order.
    const lists = [];
    let albums = [];
    if (wanted.liked || wanted.albums) {
      const saved = await collection(user);
      if (wanted.liked) lists.push({ title: LIKED, tracks: saved.liked });
      if (wanted.albums) albums = saved.albums;
    }
    const titles = new Set([LIKED]);
    const listed = wanted.playlists ? await playlists(user) : [];
    if (wanted.playlists) {
      for (const playlist of listed) {
        const listed = await playlistTracks(playlist);
        if (!listed) continue;
        let title = listed.title || "Untitled playlist";
        for (let n = 2; titles.has(title); n++) title = `${listed.title || "Untitled playlist"} (${n})`;
        titles.add(title);
        lists.push({ title, tracks: listed.tracks, id: playlist.id });
      }
    }
    const fromAlbums = [];
    for (const album of albums) fromAlbums.push(...(await albumTracks(album)));

    // Each track once, with its place in every playlist it is in.
    const places = new Map();
    for (const list of lists) {
      list.tracks.forEach((id, at) => {
        if (!places.has(id)) places.set(id, {});
        places.get(id)[list.title] ??= at + 1;
      });
    }
    for (const id of fromAlbums) if (!places.has(id)) places.set(id, {});
    console.log(`library: ${places.size} tracks (${lists.length} playlists with Liked Songs, ${albums.length} saved albums)`);

    // Look for the new ones on YouTube, a bounded number a sync.
    let looked = 0, waiting = 0, stopped = null;
    for (const id of places.keys()) {
      const match = storage.get(`m:${id}`);
      if (match && (!match.none || match.until > Date.now())) continue;
      if (looked >= wanted.budget || stopped) {
        waiting += 1;
        continue;
      }
      if (looked++) await sleep(MATCH_PAUSE_MS);
      let track;
      try {
        track = await details(id);
      } catch (error) {
        // A track Spotify no longer has: as one YouTube has none of.
        if (!/answered 404/.test(error.message)) throw error;
        storage.set(`m:${id}`, { none: true, until: Date.now() + RETRY_MS });
        continue;
      }
      let found;
      try {
        found = await addons.call("youtube", "findSong", {
          title: track.title, artists: track.artists, album: track.album.title, duration_ms: track.duration_ms, isrc: track.isrc,
        });
      } catch (error) {
        if (UNREACHABLE.test(error.message)) throw error;
        console.warn(`looking on YouTube stopped for this sync: ${error.message}`);
        stopped = error.message;
        waiting += 1;
        continue;
      }
      if (!found) {
        storage.set(`m:${id}`, { none: true, until: Date.now() + RETRY_MS });
        continue;
      }
      const { video_id, duration_ms, size, bitrate, sample_rate, channels, kind, score, via } = found;
      storage.set(`m:${id}`, { video_id, duration_ms, size, bitrate, sample_rate, channels, kind, score, via });
    }

    // What reached rex: every matched track.
    const items = [];
    let unmatched = 0;
    for (const [id, playlists] of places) {
      const match = storage.get(`m:${id}`);
      if (!match?.video_id) {
        if (match?.none) unmatched += 1;
        continue;
      }
      const track = await details(id);
      const poster = await portrait(track.album.artist_gid);
      items.push({
        id: `sp-${id}`,
        path: `spotify/${track.album.id}/${id}.m4a`,
        kind: "track", disc: track.disc, ...(track.track ? { track: track.track } : {}),
        size: match.size,
        display: {
          title: track.title, date: track.album.date,
          artist: { title: track.album.artist, ...(poster ? { poster } : {}) },
          album: { title: track.album.title, date: track.album.date, ...(track.album.cover ? { poster: track.album.cover } : {}) },
        },
        playlists,
        media_info: probe(match),
      });
    }
    console.log(`${items.length} matched, ${unmatched} not on YouTube (looked for again after a week), ${waiting} waiting; ${looked} looked for this sync`);

    // Forget what left the library, so storage stays bounded.
    const albumsKept = new Set(albums), playlistsKept = new Set(listed.map((playlist) => playlist.id));
    for (const id of storage.get("known") ?? []) {
      if (!places.has(id)) ["t", "m"].forEach((kind) => storage.delete(`${kind}:${id}`));
    }
    for (const id of storage.get("albums") ?? []) if (!albumsKept.has(id)) storage.delete(`al:${id}`);
    for (const id of storage.get("playlists") ?? []) if (!playlistsKept.has(id)) storage.delete(`pl:${id}`);
    const artists = new Set([...places.keys()].map((id) => storage.get(`t:${id}`)?.album?.artist_gid).filter(Boolean));
    for (const artist of storage.get("artists") ?? []) if (!artists.has(artist)) storage.delete(`ar:${artist}`);
    // The lists as read, in Spotify's order (what a test compares rex's playlists with).
    storage.set("lists", lists.map((list) => ({ title: list.title, tracks: list.tracks })));
    storage.set("known", [...places.keys()]);
    storage.set("albums", [...albumsKept]);
    storage.set("playlists", [...playlistsKept]);
    storage.set("artists", [...artists]);
    return items;
  },

  async open(id) {
    const match = storage.get(`m:${id.replace(/^sp-/, "")}`);
    if (!match?.video_id) throw new Error(`${id} has no recording on YouTube`);
    // A fresh link for each play: YouTube's last six hours.
    const audio = await addons.call("youtube", "songAudio", match.video_id);
    return { url: audio.url };
  },
};
