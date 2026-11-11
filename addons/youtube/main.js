// Your YouTube playlists in rex: Liked videos, Watch later, your own playlists and any playlist
// you link. Each YouTube playlist is a rex playlist in YouTube's order; each video is one episode
// of its channel's show in the YouTube library, and plays from YouTube's own HLS stream. Nothing is
// downloaded: rex converts (copies) the stream as it plays.
//
// Everything goes through YouTube's own app API (InnerTube), as YouTube's apps do:
// - listing, as the TV app (TVHTML5): signed in, its OAuth token is accepted only from the TV app;
//   a public playlist by link is listed the same way, signed out (measured 2026-11-11).
// - playing, as the iOS app, signed out: its answer has an HLS master playlist of muxed H.264 + AAC
//   variants up to 1080p whose segments need no token. The best variant within the resolution
//   limit is what rex reads, and what was probed.

const YT = "https://www.youtube.com";
const OAUTH = {
  // YouTube on TV's own client: its device flow is the one YouTube accepts for the TV app's API.
  client_id: "861556708454-d6dlm3lh05idd8npek18k6be8ba3oc68.apps.googleusercontent.com",
  client_secret: "SboVhoG9s0rNafixCSGGKXAT",
  scope: "http://gdata.youtube.com https://www.googleapis.com/auth/youtube",
};
const CLIENTS = {
  tv: { id: 7, ua: "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version", context: { clientName: "TVHTML5", clientVersion: "7.20260311.12.00" } },
  ios: {
    id: 5, ua: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
    context: { clientName: "IOS", clientVersion: "20.10.4", deviceMake: "Apple", deviceModel: "iPhone16,2", osName: "iPhone", osVersion: "18.3.2.22D82" },
  },
  // Only for a video's upload date: the iOS answer has none, the web player's microformat does.
  web: {
    id: 1, ua: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
    context: { clientName: "WEB", clientVersion: "2.20260205.01.00" },
  },
};
// Any video's id: YouTube lists the account's playlists in its "save to playlist" answer for one.
const ANY_VIDEO = "jNQXAC9IVRw";

// A video is checked (asked about once, its stream probed once) before it reaches rex: a player
// call, its master playlist and one segment read by ffprobe, a few MB. A sync checks this many at
// most, within this time, pausing between them, and the rest arrive over the next hourly syncs;
// what a check learns is kept for as long as the video stays in a playlist.
const CHECKS_PER_SYNC = 30;
const CHECK_BUDGET_MS = 5 * 60 * 1000;
const CHECK_PAUSE_MS = 1500;
// The playlists are listed again at most this often (and whenever the settings change); the
// syncs between only check videos still waiting.
const LISTING_MS = 6 * 60 * 60 * 1000;
// A video that could not be checked (private, removed, no stream) is asked about again after this.
const RETRY_MS = 24 * 60 * 60 * 1000;

const cut = (text, most) => (text && text.length > most ? `${text.slice(0, most - 3)}...` : text || undefined);
const textOf = (node) => node?.simpleText ?? (node?.runs ?? []).map((run) => run.text).join("") ?? "";

/** Every node under `node` with one of `keys`, in document order, looking inside neither them nor
 * the `skip` keys. */
function find(node, keys, skip = [], out = []) {
  if (Array.isArray(node)) node.forEach((inner) => find(inner, keys, skip, out));
  else if (node && typeof node === "object") {
    for (const [key, value] of Object.entries(node)) {
      if (keys.includes(key)) out.push([key, value]);
      else if (!skip.includes(key)) find(value, keys, skip, out);
    }
  }
  return out;
}
const VIDEOS = ["tileRenderer", "playlistVideoRenderer"];

// ---------------------------------------------------------------- signing in

async function oauth(path, fields) {
  const answer = await fetch(`${YT}/o/oauth2/${path}`, {
    method: "POST", headers: { "Content-Type": "application/json", "User-Agent": CLIENTS.tv.ua }, body: JSON.stringify(fields),
  });
  if (answer.status >= 500) throw new Error(`YouTube's sign-in answered ${answer.status}`);
  return { status: answer.status, ...(await answer.json()) };
}

function keep(answer) {
  const before = storage.get("tokens") ?? {};
  storage.set("tokens", {
    access: answer.access_token,
    refresh: answer.refresh_token ?? before.refresh,
    expires_at: Date.now() + (Number(answer.expires_in) || 3600) * 1000,
  });
}

/** The access token, refreshed when it is about to expire (or `force`); null when signed out. */
async function accessToken(force = false) {
  const tokens = storage.get("tokens");
  if (!tokens?.refresh) return null;
  if (!force && tokens.access && tokens.expires_at > Date.now() + 60 * 1000) return tokens.access;
  const answer = await oauth("token", {
    client_id: OAUTH.client_id, client_secret: OAUTH.client_secret, refresh_token: tokens.refresh, grant_type: "refresh_token",
  });
  if (answer.error || !answer.access_token) {
    throw new Error(`SIGN_IN_REQUIRED: YouTube refused the sign-in (${answer.error_description ?? answer.error ?? answer.status})`);
  }
  keep(answer);
  return answer.access_token;
}

// ---------------------------------------------------------------- InnerTube

/** One InnerTube call as `client`; signed in when `token` is given. The request's shape (never the
 * token) is logged, so a changed answer can be told from a changed request. */
async function innertube(endpoint, client, body, token = null) {
  const { id, ua, context } = CLIENTS[client];
  console.debug(`youtubei/${endpoint} as ${context.clientName}${token ? ", signed in" : ""}: ${JSON.stringify(body).slice(0, 120)}`);
  const headers = {
    "Content-Type": "application/json", "User-Agent": ua,
    "X-YouTube-Client-Name": String(id), "X-YouTube-Client-Version": context.clientVersion,
  };
  if (token) headers.Authorization = `Bearer ${token}`;
  const answer = await fetch(`${YT}/youtubei/v1/${endpoint}?prettyPrint=false`, {
    method: "POST", headers, body: JSON.stringify({ context: { client: { ...context, hl: "en", gl: "US" } }, ...body }),
  });
  if (answer.status === 429) throw new Error("YouTube answered 429 (too many requests from this address); the next sync tries again");
  if (answer.status === 401 || answer.status === 403) return { unauthorized: answer.status };
  if (!answer.ok) throw new Error(`YouTube's ${endpoint} answered ${answer.status}`);
  return await answer.json();
}

/** An InnerTube call as the signed-in TV app, the token refreshed once when YouTube refuses it. */
async function signedIn(endpoint, body) {
  let answer = await innertube(endpoint, "tv", body, await accessToken());
  if (answer.unauthorized) answer = await innertube(endpoint, "tv", body, await accessToken(true));
  if (answer.unauthorized) throw new Error(`SIGN_IN_REQUIRED: YouTube answered ${answer.unauthorized} to the signed-in account`);
  return answer;
}

/** The account's own playlists, as its "save to playlist" answer lists them: [{id, title}]. */
async function ownPlaylists() {
  const answer = await signedIn("playlist/get_add_to_playlist", { videoIds: [ANY_VIDEO] });
  const found = find(answer, ["playlistAddToOptionRenderer"]).map(([, option]) => ({ id: option.playlistId, title: textOf(option.title) }));
  if (!find(answer, ["addToPlaylistRenderer"]).length) throw new Error("YouTube did not list the account's playlists");
  return found.filter((one) => one.id && !["WL", "LL"].includes(one.id));
}

/** The account's name, if YouTube says it. */
async function accountName() {
  try {
    const answer = await signedIn("account/accounts_list", {});
    const name = find(answer, ["accountName"]).map(([, node]) => textOf(node)).find(Boolean);
    return name || "YouTube";
  } catch (error) {
    console.warn(`the account's name: ${error.message}`);
    return "YouTube";
  }
}

/** One video's id from a playlist page, or null for what is not a playable video (private, deleted, live,
 * upcoming, a short's shelf). The TV app lists tiles; the web shape is read too. */
function entry([kind, node]) {
  if (kind === "tileRenderer") {
    if (node.contentType && node.contentType !== "TILE_CONTENT_TYPE_VIDEO") return null;
    const id = node.onSelectCommand?.watchEndpoint?.videoId ?? node.contentId;
    const overlays = (node.header?.tileHeaderRenderer?.thumbnailOverlays ?? []).map((o) => o.thumbnailOverlayTimeStatusRenderer).filter(Boolean);
    const length = overlays.find((o) => !["LIVE", "UPCOMING"].includes(o.style))?.text;
    // A private or deleted video's tile has no length; a live stream's says LIVE.
    if (!id || !length || overlays.some((o) => ["LIVE", "UPCOMING"].includes(o.style))) return null;
    return id;
  }
  return node.isPlayable === false || !node.lengthSeconds ? null : node.videoId ?? null;
}

/** The first `count` videos of a playlist, in its order, following its continuations, and its
 * title. Throws rather than answer with part of it. */
async function listPlaylist(id, count, signed) {
  const browse = (body) => (signed ? signedIn("browse", body) : innertube("browse", "tv", body));
  let answer = await browse({ browseId: `VL${id}` });
  if (answer.unauthorized) throw new Error(`playlist ${id}: YouTube answered ${answer.unauthorized}; is it private?`);
  const title = find(answer.contents ?? {}, ["entityMetadataRenderer", "playlistHeaderRenderer"], VIDEOS)
    .map(([, node]) => textOf(node.title)).find(Boolean);
  const videos = [];
  for (let page = 0; ; page++) {
    const entries = find(answer.contents ?? answer.continuationContents ?? answer.onResponseReceivedActions ?? {}, VIDEOS);
    const next = find(answer, ["nextContinuationData", "reloadContinuationData", "continuationCommand"], VIDEOS)
      .map(([, node]) => node.continuation ?? node.token).find(Boolean);
    if (!page && !title && !entries.length && !next) {
      const alert = find(answer.alerts ?? [], ["text"]).map(([, node]) => textOf(node)).find(Boolean);
      throw new Error(`playlist ${id}: ${alert || "YouTube answered no playlist"}`);
    }
    for (const video of entries.map(entry)) {
      if (video && !videos.includes(video) && videos.length < count) videos.push(video);
    }
    if (videos.length >= count || !next) break;
    if (page > count / 5 + 10) throw new Error(`playlist ${id}: more pages than ${count} videos fill`);
    answer = await browse({ continuation: next });
    if (!answer.continuationContents && !answer.onResponseReceivedActions) {
      throw new Error(`playlist ${id}: YouTube answered page ${page + 2} with nothing`);
    }
  }
  return { title, videos };
}

/** The iOS app's player answer for a video, signed out; signed in only when YouTube asks for an
 * account (an age-restricted video). */
async function player(id) {
  const body = { videoId: id, contentCheckOk: true, racyCheckOk: true };
  let answer = await innertube("player", "ios", body);
  const status = answer.playabilityStatus?.status;
  if (status === "LOGIN_REQUIRED" && !/bot/i.test(answer.playabilityStatus?.reason ?? "")) {
    const token = await accessToken();
    if (token) {
      const again = await innertube("player", "ios", body, token);
      if (!again.unauthorized) answer = again;
    }
  }
  return answer;
}

/** Why a player answer cannot play, or null. `bot` when YouTube wants this address to sign in. */
function refusal(answer) {
  const status = answer.playabilityStatus ?? {};
  if (status.status !== "OK") {
    const why = status.reason || textOf(status.errorScreen?.playerErrorMessageRenderer?.reason) || status.status || "no answer";
    return { why, bot: /not a bot/i.test(why) };
  }
  const details = answer.videoDetails ?? {};
  if (details.isLive || details.isUpcoming || details.isPostLiveDvr || !Number(details.lengthSeconds)) return { why: "a live stream" };
  if (!answer.streamingData?.hlsManifestUrl) return { why: "YouTube offered no HLS stream", nohls: true };
  return null;
}

/** The variant of an HLS master playlist rex reads: picture and sound together (H.264 + AAC), the
 * highest within `most` lines, else the smallest. Returns {url, height}. */
async function variant(master, most) {
  const answer = await fetch(master);
  if (!answer.ok) throw new Error(`YouTube's HLS playlist answered ${answer.status}`);
  const lines = (await answer.text()).split(/\r?\n/);
  const variants = [];
  lines.forEach((line, at) => {
    if (!line.startsWith("#EXT-X-STREAM-INF:")) return;
    const attributes = Object.fromEntries([...line.matchAll(/([A-Z0-9-]+)=("[^"]*"|[^,]*)/g)].map(([, name, value]) => [name, value.replace(/^"|"$/g, "")]));
    const codecs = attributes.CODECS ?? "";
    const uri = lines.slice(at + 1).find((next) => next.trim() && !next.startsWith("#"));
    // A variant with an AUDIO group has its sound elsewhere: rex reads one playlist.
    if (!uri || attributes.AUDIO || !/avc1/.test(codecs) || !/mp4a/.test(codecs)) return;
    const height = Number((attributes.RESOLUTION ?? "").split("x")[1]) || 0;
    variants.push({ url: new URL(uri.trim(), answer.url).href, height, bandwidth: Number(attributes.BANDWIDTH) || 0 });
  });
  if (!variants.length) throw new Error("YouTube's HLS playlist has no stream with picture and sound together");
  const within = variants.filter((one) => one.height <= most);
  const pick = (within.length ? within : variants.filter((one) => one.height === Math.min(...variants.map((v) => v.height))))
    .reduce((best, one) => (one.height > best.height || (one.height === best.height && one.bandwidth > best.bandwidth) ? one : best));
  return { url: pick.url, height: pick.height };
}

/** The upload date, from the web player's microformat; undefined when it doesn't say. */
async function uploadDate(id) {
  try {
    const answer = await innertube("player", "web", { videoId: id, contentCheckOk: true, racyCheckOk: true });
    const micro = answer.microformat?.playerMicroformatRenderer ?? {};
    const date = String(micro.publishDate ?? micro.uploadDate ?? "").slice(0, 10);
    return /^\d{4}-\d{2}-\d{2}$/.test(date) ? date : undefined;
  } catch (error) {
    console.warn(`${id}: no upload date: ${error.message}`);
    return undefined;
  }
}

function settings(config) {
  const number = (value, fallback, low, high) => Math.max(low, Math.min(high, Math.floor(Number(value) || fallback)));
  const links = (config.playlists ?? []).map((link) => link.trim()).filter(Boolean).map((link) => {
    let id = link;
    if (/^https?:\/\//.test(link)) id = new URL(link).searchParams.get("list") ?? "";
    if (!/^[A-Za-z0-9_-]{2,64}$/.test(id)) throw new Error(`${link}: not a YouTube playlist link`);
    return id;
  });
  return {
    count: number(config.count, 200, 1, 5000), most: number(config.max_height, 1080, 144, 4320),
    liked: config.liked !== false, later: config.watch_later !== false, own: config.own !== false, links,
  };
}

/** Every playlist the settings name, each with its first videos: [{title, videos}]. */
async function listAll(wanted) {
  const signed = Boolean(storage.get("tokens")?.refresh);
  const sources = [];
  if (signed && wanted.liked) sources.push({ id: "LL", title: "Liked videos" });
  if (signed && wanted.later) sources.push({ id: "WL", title: "Watch later" });
  if (signed && wanted.own) sources.push(...(await ownPlaylists()));
  // A playlist by link is anyone's, listed signed out; one the account has is listed as its own.
  sources.push(...wanted.links.filter((id) => !sources.some((one) => one.id === id)).map((id) => ({ id, link: true })));
  const lists = [];
  const titles = new Set();
  for (const source of sources) {
    const listed = await listPlaylist(source.id, wanted.count, signed && !source.link);
    const named = source.title || listed.title;
    if (!named) throw new Error(`playlist ${source.id}: YouTube answered no title`);
    let title = named;
    for (let n = 2; titles.has(title); n++) title = `${named} (${n})`;
    titles.add(title);
    lists.push({ title, videos: listed.videos });
    console.log(`${title}: ${listed.videos.length} videos`);
  }
  return lists;
}

/** What a check of one video learns: its details, the variant within `most` and that variant's probe. */
async function check(id, most) {
  const answer = await player(id);
  const refused = refusal(answer);
  if (refused) return { refused };
  const details = answer.videoDetails;
  const chosen = await variant(answer.streamingData.hlsManifestUrl, most);
  const probe = await ffprobe(chosen.url);
  const thumbs = (details.thumbnail?.thumbnails ?? []).filter((t) => /^https:\/\//.test(t.url ?? ""));
  return {
    video: {
      title: details.title, summary: cut(details.shortDescription, 2000), date: await uploadDate(id),
      thumb: thumbs.reduce((best, t) => (!best || (t.width ?? 0) > (best.width ?? 0) ? t : best), null)?.url,
      channel: details.author, channel_id: details.channelId, height: chosen.height, most, probe,
    },
  };
}

export default {
  async items(ctx) {
    const wanted = settings(ctx.config);
    const signed = Boolean(storage.get("tokens")?.refresh);
    const key = JSON.stringify([wanted.count, wanted.liked, wanted.later, wanted.own, wanted.links, signed]);
    let listing = storage.get("listing");
    if (!listing || listing.key !== key || Date.now() - listing.at > LISTING_MS) {
      listing = { key, at: Date.now(), lists: await listAll(wanted) };
      storage.set("listing", listing);
    }

    // Each video once, with its place in every playlist it is in.
    const places = new Map();
    for (const list of listing.lists) {
      list.videos.forEach((id, at) => {
        if (!places.has(id)) places.set(id, {});
        places.get(id)[list.title] = at + 1;
      });
    }

    // Check what is new (or was checked for another resolution limit), a bounded number a sync.
    const numbers = storage.get("numbers") ?? {};
    const until = Date.now() + CHECK_BUDGET_MS;
    // Three videos in a row without an HLS stream: YouTube offers none to this address for now.
    let checks = 0, noHls = 0, waiting = 0, halted = false;
    for (const id of places.keys()) {
      const known = storage.get(`v:${id}`);
      const skipped = storage.get(`skip:${id}`);
      if ((known && known.most === wanted.most) || (skipped && skipped.until > Date.now())) continue;
      if (checks >= CHECKS_PER_SYNC || Date.now() > until || halted) {
        waiting += 1;
        continue;
      }
      if (checks++) await sleep(CHECK_PAUSE_MS);
      let checked;
      try {
        checked = await check(id, wanted.most);
      } catch (error) {
        if (/^YouTube answered 429|ffprobe\(\) is not available/.test(error.message)) throw error;
        console.warn(`${id}: ${error.message}`);
        storage.set(`skip:${id}`, { until: Date.now() + RETRY_MS, why: error.message });
        continue;
      }
      if (checked.refused) {
        const { why, bot, nohls } = checked.refused;
        if (bot) {
          // Asking on would only make it worse: what is ready goes to rex, the rest waits.
          console.warn(`YouTube asks this address to prove it is not a bot (${why}); checks stop until the next sync`);
          halted = true;
          continue;
        }
        noHls = nohls ? noHls + 1 : 0;
        halted = noHls >= 3;
        console.warn(`${id}: ${why}`);
        storage.set(`skip:${id}`, { until: Date.now() + (nohls ? LISTING_MS : RETRY_MS), why });
        continue;
      }
      noHls = 0;
      const video = checked.video;
      // Episodes are numbered per channel in the order they arrive, so no number ever moves.
      video.episode = known?.episode ?? (numbers[video.channel_id] = (numbers[video.channel_id] ?? 0) + 1);
      storage.set(`v:${id}`, video);
      storage.delete(`skip:${id}`);
    }
    if (waiting) console.log(`${waiting} videos wait to be checked; they arrive over the next syncs`);

    const ready = [...places.keys()].map((id) => [id, storage.get(`v:${id}`)]).filter(([, video]) => video);
    // A show's poster: the picture of its channel's first video here, the same on every push.
    const posters = {};
    for (const [, video] of [...ready].sort(([, a], [, b]) => a.episode - b.episode)) posters[video.channel_id] ??= video.thumb;
    const items = ready.map(([id, video]) => ({
      id: `yt-${id}`,
      path: `youtube/${video.channel_id}/${id}.m3u8`,
      kind: "episode", season: 1, episode: video.episode,
      name: video.channel, release: `youtube:${video.channel_id}`,
      display: {
        title: video.title, summary: video.summary, date: video.date, thumb: video.thumb,
        show: { title: video.channel, poster: posters[video.channel_id] },
      },
      playlists: places.get(id),
      media_info: video.probe,
    }));

    // Forget videos that left every playlist, and channels left with none, so storage stays bounded.
    for (const id of storage.get("known") ?? []) {
      if (!places.has(id)) ["v", "skip"].forEach((kind) => storage.delete(`${kind}:${id}`));
    }
    storage.set("known", [...places.keys()]);
    const channels = new Set(ready.map(([, video]) => video.channel_id));
    storage.set("numbers", Object.fromEntries(Object.entries(numbers).filter(([channel]) => channels.has(channel))));
    console.log(`${items.length} of ${places.size} videos are in rex`);
    return items;
  },

  async open(id, ctx) {
    const vid = id.replace(/^yt-/, "");
    const answer = await player(vid);
    const refused = refusal(answer);
    if (refused) throw new Error(`${vid} does not play: ${refused.why}`);
    // The variant it was probed at, so what plays is what rex was told about.
    const most = storage.get(`v:${vid}`)?.height ?? settings(ctx.config).most;
    return { url: (await variant(answer.streamingData.hlsManifestUrl, most)).url };
  },

  async signIn() {
    const device = Array.from({ length: 32 }, () => Math.floor(Math.random() * 16).toString(16)).join("");
    const answer = await oauth("device/code", { client_id: OAUTH.client_id, scope: OAUTH.scope, device_id: device, device_model: "ytlr::" });
    if (answer.error || !answer.device_code) throw new Error(`YouTube refused to start a sign-in: ${answer.error_description ?? answer.error ?? answer.status}`);
    return {
      code: answer.user_code, url: answer.verification_url ?? answer.verification_uri,
      expires_in: answer.expires_in, interval: answer.interval ?? 5, state: { device_code: answer.device_code },
    };
  },

  async signInPoll(state) {
    const answer = await oauth("token", {
      client_id: OAUTH.client_id, client_secret: OAUTH.client_secret, code: state.device_code, grant_type: "http://oauth.net/grant_type/device/1.0",
    });
    if (answer.error === "authorization_pending" || answer.error === "slow_down") return { done: false };
    if (answer.error === "access_denied") throw new Error("the sign-in was refused on the website");
    if (answer.error === "expired_token") throw new Error("the code expired before it was entered");
    if (answer.error || !answer.access_token || !answer.refresh_token) throw new Error(`YouTube refused the sign-in: ${answer.error_description ?? answer.error ?? answer.status}`);
    keep(answer);
    storage.delete("listing");
    return { done: true, account: await accountName() };
  },

  async signOut() {
    const tokens = storage.get("tokens");
    storage.delete("tokens");
    storage.delete("listing");
    if (tokens?.refresh) {
      try {
        await fetch(`${YT}/o/oauth2/revoke?token=${encodeURIComponent(tokens.refresh)}`, { method: "POST" });
      } catch (error) {
        console.warn(`revoking the sign-in: ${error.message}`);
      }
    }
  },
};
