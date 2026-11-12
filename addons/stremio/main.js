// @ts-check
/// <reference path="../../types/rex-addons.d.ts" />
// Your Stremio addons' film and series catalogs in rex, as rex-stremio brings them: each film
// and episode goes to rex with its IMDb id, so Plex names it, and plays from the best stream
// your stream addons give for it at that moment.

const CINEMETA = "https://v3-cinemeta.strem.io";
/** @type {[RegExp, number][]} */
const RESOLUTIONS = [
  [/2160p|\b4k\b|\buhd\b/i, 2160],
  [/1440p/i, 1440],
  [/1080p|\bfhd\b/i, 1080],
  [/720p|\bhd\b/i, 720],
  [/576p|480p|\bsd\b/i, 480],
];

const list = (value) => (Array.isArray(value) ? value : value == null ? [] : [value]);
const slug = (text) => String(text).toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
// Addon links can carry a debrid key: logs name the host only.
const host = (url) => String(url).replace(/^(\w+:\/\/[^/?#]+).*$/, "$1");

function base(url) {
  let link = String(url ?? "").trim();
  if (link.startsWith("stremio://")) link = `https://${link.slice("stremio://".length)}`;
  return link.replace(/\/manifest\.json$/, "").replace(/\/+$/, "");
}

/** Whether an addon answers `resource` for this type and id, by its manifest. */
function serves(manifest, resource, kind, id) {
  if (!manifest || typeof id !== "string") return false;
  return list(manifest.resources).some((entry) => {
    const named = typeof entry === "string" ? { name: entry } : entry;
    if (!named || named.name !== resource) return false;
    const types = list(named.types?.length ? named.types : manifest.types);
    const prefixes = list(named.idPrefixes?.length ? named.idPrefixes : manifest.idPrefixes);
    return types.includes(kind) && (!prefixes.length || prefixes.some((prefix) => id.startsWith(prefix)));
  });
}

/** Film and series catalogs an addon lists without being asked something first (a search, a genre). */
function catalogsOf(manifest) {
  return list(manifest?.catalogs).filter((catalog) => {
    if (!catalog || !["movie", "series"].includes(catalog.type) || typeof catalog.id !== "string" || !catalog.id) return false;
    const required = list(catalog.extra).some((extra) => extra?.isRequired) || list(catalog.extraRequired).length > 0;
    return !required;
  });
}

/** [resolution, bytes] as a stream's labels give them; 0 or null where they do not. */
function quality(stream) {
  const hints = stream.behaviorHints ?? {};
  const text = `${stream.name ?? ""} ${stream.title ?? ""} ${stream.description ?? ""} ${hints.filename ?? ""}`;
  const resolution = RESOLUTIONS.find(([pattern]) => pattern.test(text))?.[1] ?? 0;
  let size = Number(hints.videoSize) || null;
  if (!size) {
    const found = /(\d+(?:\.\d+)?)\s*(GB|MB)\b/i.exec(text);
    if (found) size = Number(found[1]) * (found[2].toUpperCase() === "GB" ? 2 ** 30 : 2 ** 20);
  }
  return [resolution, size > 0 ? size : null];
}

/** What names one stream across asks: its file, else its group, else its link. */
function streamKey(stream) {
  const hints = stream.behaviorHints ?? {};
  return hints.filename || hints.bingeGroup || stream.url;
}

/** The streams rex can read, best first: an http(s) link (a torrent's infoHash is no file), within
 * the limits, highest resolution first, the addons' own order breaking ties. The stream the item
 * was probed from goes first, so what plays is what rex was told about. */
function ranked(streams, config, pinned) {
  const cap = Number(config.max_resolution) || 0;
  const biggest = (Number(config.max_size_gb) || 0) * 2 ** 30;
  return streams
    .map((stream, at) => ({ stream, at, quality: quality(stream) }))
    .filter(({ stream, quality: [resolution, size] }) =>
      typeof stream.url === "string" && /^https?:\/\//.test(stream.url) &&
      !(cap && resolution > cap) && !(biggest && size && size > biggest))
    .sort((a, b) =>
      (pinned ? Number(streamKey(a.stream) !== pinned) - Number(streamKey(b.stream) !== pinned) : 0) ||
      b.quality[0] - a.quality[0] || a.at - b.at)
    .map(({ stream }) => stream);
}

/** The headers a link must be asked with (Stremio's proxyHeaders); without its Referer a host can refuse. */
function requestHeaders(stream) {
  const headers = stream.behaviorHints?.proxyHeaders?.request;
  if (!headers || typeof headers !== "object") return {};
  return Object.fromEntries(Object.entries(headers)
    .filter(([name, value]) => ["string", "number"].includes(typeof value) &&
      !["accept-encoding", "range", "host", "connection", "content-length"].includes(name.toLowerCase()))
    .map(([name, value]) => [name, String(value)]));
}

/** The link a stream ends at when it opens as a video rex can seek in; null otherwise (an expired
 * link, a web page an addon lists as a stream, a file that ignores Range, a tiny error body). */
async function opens(url, headers) {
  try {
    const response = await fetch(url, { headers: { ...headers, Range: "bytes=0-0" } });
    const type = (response.headers["content-type"] ?? "").split(";")[0].trim().toLowerCase();
    if (type === "text/html") return console.log(`${host(url)}: a web page, not a video`), null;
    const playlist = type.includes("mpegurl") || /\.m3u8$/i.test(response.url.split(/[?#]/)[0]);
    if (response.status === 200 && !playlist && Number(response.headers["content-length"] ?? 0) > 1) {
      return console.log(`${host(url)}: answers no Range, so it cannot be seeked`), null;
    }
    const total = /\/(\d+)$/.exec(response.headers["content-range"] ?? "");
    if (total && Number(total[1]) < 2 ** 20 && !playlist) return console.log(`${host(url)}: ${total[1]} bytes, not a video`), null;
    return response.status === 200 || response.status === 206 ? response.url : null;
  } catch (error) {
    console.log(`${host(url)}: ${error.message}`);
    return null;
  }
}

async function getJson(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${host(url)} answered ${response.status}`);
  return response.json();
}

/** Every stream the addons that serve this item give, each asked by the id it serves. */
async function streamsFor(info, config) {
  const manifests = storage.get("manifests") ?? {};
  const found = [];
  for (const root of list(config.addons).map(base)) {
    const named = [...list(info.also), info.sid].find((id) => serves(manifests[root], "stream", info.kind, id));
    if (!named) continue;
    try {
      const answer = await getJson(`${root}/stream/${info.kind}/${encodeURIComponent(named)}.json`);
      found.push(...list(answer?.streams).filter((stream) => stream && typeof stream === "object"));
    } catch (error) {
      console.warn(`${named}: ${error.message}`);
    }
  }
  return found;
}

function prune(value) {
  if (!value || typeof value !== "object") return value;
  const kept = Object.entries(value)
    .map(([key, inner]) => [key, prune(inner)])
    .filter(([, inner]) => inner != null && inner !== "" && !(typeof inner === "object" && !Object.keys(inner).length));
  return Object.fromEntries(kept);
}

const released = (value) => (/^\d{4}-\d{2}-\d{2}/.test(String(value ?? "")) ? String(value).slice(0, 10) : undefined);

/** @type {Addon} */
export default {
  async items(ctx) {
    const config = ctx.config;
    const limit = Math.max(1, Math.floor(Number(config.limit) || 50));
    const wanted = list(config.catalogs).map((entry) => entry.trim()).filter(Boolean);
    const manifests = {};
    for (const root of list(config.addons).map(base)) {
      if (!/^https?:\/\//.test(root)) throw new Error(`${root}: not an http(s) link`);
      // A catalog that cannot be read fails the sync, so rex keeps what it has rather than losing it.
      manifests[root] = await getJson(`${root}/manifest.json`);
    }
    storage.set("manifests", manifests);

    const items = [];
    const info = {};
    for (const [root, manifest] of Object.entries(manifests)) {
      for (const catalog of catalogsOf(manifest)) {
        if (wanted.length && !wanted.includes(`${catalog.type}/${catalog.id}`)) continue;
        const name = slug(`${manifest.id ?? manifest.name}-${catalog.type}-${catalog.id}`);
        const metas = (await getJson(`${root}/catalog/${catalog.type}/${encodeURIComponent(catalog.id)}.json`))?.metas;
        if (!Array.isArray(metas) || !metas.length) {
          throw new Error(`${catalog.name ?? catalog.id}: the catalog answered nothing`);
        }
        let taken = 0;
        for (const meta of metas) {
          if (taken >= limit || !meta || typeof meta !== "object") continue;
          const own = typeof meta.id === "string" ? meta.id : "";
          const imdb = /^tt\d+$/.test(meta.imdb_id ?? "") ? meta.imdb_id : (/(?<![A-Za-z0-9])tt\d{5,}(?!\d)/.exec(own) ?? [""])[0];
          const year = /^\d{4}/.exec(String(meta.year ?? meta.releaseInfo ?? ""))?.[0];
          const show = { title: meta.name, summary: meta.description, poster: meta.poster, art: meta.background };
          if (catalog.type === "movie") {
            if (!imdb && !own) continue;
            const id = `st-${name}-${imdb || own.replace(/[^A-Za-z0-9_-]/g, "-")}`;
            info[id] = prune({ kind: "movie", sid: imdb || own, also: own && own !== imdb ? [own] : undefined });
            items.push(prune({
              id, path: `stremio/${name}/${id}.mkv`, kind: "movie", ids: imdb ? { imdb } : undefined,
              name: meta.name || imdb || own, release: `stremio:${imdb || own}`,
              display: { title: meta.name, summary: meta.description, thumb: meta.poster, art: meta.background, date: released(meta.released) ?? year },
            }));
            taken += 1;
            continue;
          }
          // ponytail: a series only its addon knows (no IMDb id) is left out; rex-stremio asks its own meta.
          if (!imdb) continue;
          const source = Object.entries(manifests).find(([, m]) => serves(m, "meta", "series", imdb))?.[0] ?? CINEMETA;
          const full = (await getJson(`${source}/meta/series/${imdb}.json`))?.meta;
          const episodes = list(full?.videos)
            .map((video) => ({ ...video, season: Number(video?.season), episode: Number(video?.episode ?? video?.number) }))
            .filter((video) => video.season > 0 && video.episode > 0)
            .sort((a, b) => a.season - b.season || a.episode - b.episode);
          for (const video of episodes) {
            if (taken >= limit) break;
            const { season, episode } = video;
            const id = `st-${name}-${imdb}-${season}-${episode}`;
            info[id] = prune({ kind: "series", sid: `${imdb}:${season}:${episode}`, also: own && own !== imdb ? [`${own}:${season}:${episode}`] : undefined });
            items.push(prune({
              id, path: `stremio/${name}/${id}.mkv`, kind: "episode", season, episode, ids: { imdb },
              name: full?.name || meta.name || imdb, release: `stremio:${imdb}`,
              display: {
                title: video.name || video.title, summary: video.overview || video.description, thumb: video.thumbnail,
                date: released(video.released || video.firstAired),
                show: { ...show, title: full?.name || meta.name },
              },
            }));
            taken += 1;
          }
        }
      }
    }
    storage.set("info", info);
    return items;
  },

  async open(id, ctx) {
    const info = (storage.get("info") ?? {})[id];
    if (!info) throw new Error(`${id} is not in the last sync`);
    const pinned = storage.get(`pin:${id}`);
    for (const stream of ranked(await streamsFor(info, ctx.config), ctx.config, pinned)) {
      const headers = requestHeaders(stream);
      const url = await opens(stream.url, headers);
      if (!url) continue;
      const key = streamKey(stream);
      if (pinned && key !== pinned) {
        // rex was told about the pinned stream; this one can differ (codec, container).
        console.warn(`${id}: the stream it was checked with stopped opening; playing the next one`);
      }
      if (key !== pinned) storage.set(`pin:${id}`, key);
      return Object.keys(headers).length ? { url, headers } : { url };
    }
    throw new Error(`${id}: no stream addon has a stream that opens`);
  },
};
