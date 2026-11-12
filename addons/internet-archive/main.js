// @ts-check
/// <reference path="../../types/rex-addons.d.ts" />
// Public domain feature films from the Internet Archive.
//
// The films are archive.org's Feature Films collection, only those it lists as public domain
// (a public domain mark or CC0) and dated before 1978, the last year a US film could lose its
// copyright by being published without a notice or never renewed. Anything sexual or sold as
// exploitation is left out, so the default list is fine for anyone.

const SEARCH = "https://archive.org/advancedsearch.php";
const QUERY = [
  "collection:feature_films",
  "mediatype:movies",
  "licenseurl:*publicdomain*",
  "year:[1890 TO 1977]",
  "NOT subject:(sex OR sexploitation OR nudity OR nudist OR erotic OR stripper OR burlesque OR grindhouse OR exploitation)",
  "NOT title:(sex OR nude OR nudist OR naked OR erotic)",
].join(" AND ");

const VIDEO = /\.(mp4|m4v|mkv|webm|ogv|mpeg|mpg|avi|mov)$/i;

function seconds(length) {
  if (!length) return 0;
  const parts = String(length).split(":").map(Number);
  return parts.reduce((total, part) => total * 60 + part, 0) || 0;
}

// The file an item plays from: its best MP4 (what every Plex app plays as it is), else another
// container. An item of several videos is a feature only when its best one is an hour or more;
// otherwise it is shorts or episodes under one title, and it is left out.
function choose(files) {
  const videos = files.filter((file) => VIDEO.test(file.name) && !/\.(ia|thumbs)\b/i.test(file.name));
  const originals = videos.filter((file) => file.source === "original").length;
  const rank = (file) => [
    /\.mp4$/i.test(file.name) ? 3 : /\.(mkv|webm|m4v)$/i.test(file.name) ? 2 : /\.ogv$/i.test(file.name) ? 1 : 0,
    Number(file.height) || 0,
    Number(file.size) || 0,
  ];
  const better = (a, b) => {
    const [x, y] = [rank(a), rank(b)];
    for (let i = 0; i < x.length; i++) if (x[i] !== y[i]) return x[i] > y[i] ? a : b;
    return a;
  };
  const best = videos.reduce((kept, file) => (kept ? better(kept, file) : file), null);
  if (!best) return { skip: "no video file" };
  if (originals > 10 || (originals > 1 && seconds(best.length) < 3600)) return { skip: `${originals} videos, not one film` };
  return { file: best.name };
}

async function fileOf(identifier) {
  const known = storage.get(`f:${identifier}`);
  if (known) return known;
  const response = await fetch(`https://archive.org/metadata/${encodeURIComponent(identifier)}`);
  if (!response.ok) throw new Error(`archive.org answered ${response.status} for ${identifier}`);
  const metadata = await response.json();
  if (metadata.is_dark || !Array.isArray(metadata.files)) return { skip: "not available" };
  const chosen = choose(metadata.files);
  storage.set(`f:${identifier}`, chosen);
  return chosen;
}

function link(identifier, file) {
  return `https://archive.org/download/${encodeURIComponent(identifier)}/${file.split("/").map(encodeURIComponent).join("/")}`;
}

// Probing a film reads a little of it from archive.org, a second or two each. Past this budget a
// sync stops probing, and rex-addons probes the rest itself, several at once.
const PROBE_BUDGET_MS = 10 * 60 * 1000;

function text(html) {
  const plain = String(Array.isArray(html) ? html.join("\n") : html ?? "")
    .replace(/<br\s*\/?>|<\/p>/gi, "\n")
    .replace(/<[^>]+>/g, "")
    .replace(/&nbsp;/g, " ")
    .replace(/&quot;/g, '"')
    .replace(/&#39;|&apos;/g, "'")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&amp;/g, "&")
    .replace(/[ \t]+/g, " ")
    .replace(/\n\s*\n+/g, "\n\n")
    .trim();
  return plain.length > 2000 ? `${plain.slice(0, 1997)}...` : plain;
}

/** @type {Addon} */
export default {
  async items(ctx) {
    const count = Math.max(1, Math.min(1000, Math.floor(ctx.config.count ?? 100)));
    const query = [
      `q=${encodeURIComponent(QUERY)}`,
      ...["identifier", "title", "year", "description", "imdb"].map((field) => `fl[]=${field}`),
      "sort[]=downloads+desc",
      `rows=${count}`,
      "page=1",
      "output=json",
    ].join("&");
    const response = await fetch(`${SEARCH}?${query}`);
    if (!response.ok) throw new Error(`archive.org search answered ${response.status}`);
    const docs = (await response.json()).response?.docs;
    if (!Array.isArray(docs)) throw new Error("archive.org search answered no list");

    const items = [];
    let probeUntil = Date.now() + PROBE_BUDGET_MS;
    for (const doc of docs) {
      const identifier = doc.identifier;
      let chosen;
      try {
        chosen = await fileOf(identifier);
      } catch (error) {
        console.warn(`${identifier}: ${error.message}`);
        continue;
      }
      if (chosen.skip) continue;
      const title = text(Array.isArray(doc.title) ? doc.title[0] : doc.title) || identifier;
      const year = /^\d{4}$/.test(String(doc.year ?? "")) ? String(doc.year) : undefined;
      const extension = chosen.file.split(".").pop().toLowerCase();
      const item = {
        id: `ia-${identifier}`,
        path: `internet-archive/${identifier}/${identifier}.${extension}`,
        name: year ? `${title} (${year})` : title,
        display: {
          title,
          summary: text(doc.description) || undefined,
          date: year,
          thumb: `https://archive.org/services/img/${identifier}`,
        },
      };
      // archive.org names an IMDb title for a few films: Plex then names and pictures them.
      const imdb = String(doc.imdb ?? "").match(/tt\d{5,}/);
      if (imdb) Object.assign(item, { kind: "movie", ids: { imdb: imdb[0] } });
      // rex shows a film once it knows what is in it. A file never changes, so each is probed once.
      let probe = storage.get(`p:${identifier}`);
      if (!probe && Date.now() < probeUntil) {
        try {
          probe = await ffprobe(link(identifier, chosen.file));
          storage.set(`p:${identifier}`, probe);
        } catch (error) {
          console.warn(`${identifier}: ${error.message}`);
          // Without ffprobe on this host, none will work: leave them all to rex-addons.
          if (/not available/.test(error.message)) probeUntil = 0;
        }
      }
      if (probe) item.media_info = probe;
      items.push(item);
    }
    // Forget films the list no longer holds, so storage does not grow forever.
    const listed = new Set(docs.map((doc) => doc.identifier));
    for (const old of storage.get("listed") ?? []) {
      if (!listed.has(old)) ["f", "p"].forEach((kind) => storage.delete(`${kind}:${old}`));
    }
    storage.set("listed", [...listed]);
    console.log(`${items.length} of ${docs.length} films have one playable video`);
    return items;
  },

  async open(id) {
    const identifier = id.replace(/^ia-/, "");
    const chosen = await fileOf(identifier);
    if (!chosen.file) throw new Error(`${identifier}: ${chosen.skip}`);
    return { url: link(identifier, chosen.file) };
  },
};
