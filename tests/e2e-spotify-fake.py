#!/usr/bin/env python3
"""Spotify's own API (spclient) for tests/e2e-spotify.sh with FAKE_SPOTIFY=1, reached through the host's
REX_ADDONS_SPOTIFY_STUB: a FABRICATED library in the structure the owner's account was answered in on
2026-11-11 (rootlist, playlist, collection paging, track, album and artist metadata). Its band's songs
are the ones tests/e2e-youtube-fake.py's search finds; one track YouTube has none of.

    python3 tests/e2e-spotify-fake.py <port>     # prints its base URL, then serves
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
B62 = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
USER = "e2e-user"


def made(n):
    """A made-up Spotify id and its gid."""
    gid = (n * 0x9E3779B97F4A7C15F39CC0605CEDC834) % (1 << 127)
    out, rest = "", gid
    while rest:
        out, rest = B62[rest % 62] + out, rest // 62
    return out.rjust(22, "0"), "%032x" % gid


ALBUMS = {101: ("Dawn", "Rex Band", (2024, 3, 1)), 102: ("Dawn (Remastered)", "Rex Band", (2025, 1, 10)),
          103: ("Singles", "Rex Band", (2025, 6, 6)), 104: ("Quiet", "Nobody", (2020, 2, 2))}
# n: title, artists, album, number, length (ms): the fake YouTube's song is 391,024 ms.
TRACKS = {1: ("First Light", ["Rex Band"], 101, 1, 391000), 2: ("First Light - 2024 Remaster", ["Rex Band"], 102, 1, 391100),
          3: ("First Light (feat. Echo)", ["Rex Band", "Echo"], 103, 2, 390800), 4: ("Night Song", ["Nobody"], 104, 1, 200000)}
PLAYLISTS = {11: ("Morning", [1, 2, 3, 4]), 12: ("Evening", [3, 1])}
LIKED = [(2, "1762800300"), (1, "1762800100")]
SAVED = [(103, "1762800200")]
BY_GID = {made(n)[1]: n for n in list(TRACKS) + list(ALBUMS) + list(PLAYLISTS)}
ARTISTS = {made(500 + i)[1]: name for i, name in enumerate(["Rex Band", "Echo", "Nobody"])}
ARTIST_GID = {name: gid for gid, name in ARTISTS.items()}


def cover(n):
    return {"image": [{"file_id": "ab67616d0000b273%024x" % n, "size": "LARGE", "width": 640, "height": 640},
                      {"file_id": "ab67616d00001e02%024x" % n, "size": "DEFAULT", "width": 300, "height": 300}]}


def track(n):
    title, artists, album, number, ms = TRACKS[n]
    name, artist, (year, month, day) = ALBUMS[album]
    return {"gid": made(n)[1], "name": title, "number": number, "disc_number": 1, "duration": ms, "popularity": 1,
            "album": {"gid": made(album)[1], "name": name, "artist": [{"gid": ARTIST_GID[artist], "name": artist}], "type": "ALBUM",
                      "date": {"year": year, "month": month, "day": day}, "cover_group": cover(album)},
            "artist": [{"gid": ARTIST_GID[a], "name": a} for a in artists],
            "external_id": [{"type": "isrc", "id": "QZREX26%05d" % n}], "canonical_uri": "spotify:track:" + made(n)[0]}


class Handler(BaseHTTPRequestHandler):
    def send(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0]
        parts = path.split("/")
        if path == "/playlist/v2/user/%s/rootlist" % USER:
            items = [{"uri": "spotify:playlist:" + made(n)[0], "attributes": {"timestamp": "1762800000000", "public": False}} for n in PLAYLISTS]
            metas = [{"revision": "r%d" % n, "attributes": {"name": name}, "length": len(tracks), "ownerUsername": USER} for n, (name, tracks) in PLAYLISTS.items()]
            return self.send(200, {"revision": "AAAA", "length": len(items), "contents": {"pos": 0, "truncated": False, "items": items, "metaItems": metas}})
        if path.startswith("/playlist/v2/playlist/"):
            n = next((n for n in PLAYLISTS if made(n)[0] == parts[-1]), None)
            if n is None:
                return self.send(404, {})
            name, tracks = PLAYLISTS[n]
            items = [{"uri": "spotify:track:" + made(t)[0], "attributes": {"timestamp": "1762800000000"}} for t in tracks]
            return self.send(200, {"revision": "r%d" % n, "length": len(items), "attributes": {"name": name}, "ownerUsername": USER,
                                   "contents": {"pos": 0, "truncated": False, "items": items}})
        if path.startswith("/metadata/4/track/") and BY_GID.get(parts[-1]) in TRACKS:
            return self.send(200, track(BY_GID[parts[-1]]))
        if path.startswith("/metadata/4/album/") and BY_GID.get(parts[-1]) in ALBUMS:
            n = BY_GID[parts[-1]]
            return self.send(200, {"gid": parts[-1], "name": ALBUMS[n][0], "cover_group": cover(n),
                                   "disc": [{"number": 1, "track": [{"gid": made(t)[1]} for t, row in TRACKS.items() if row[2] == n]}]})
        if path.startswith("/metadata/4/artist/") and parts[-1] in ARTISTS:
            return self.send(200, {"gid": parts[-1], "name": ARTISTS[parts[-1]], "portrait_group": {"image": [
                {"file_id": ("ab6761610000e5eb" + parts[-1])[:40], "size": "LARGE", "width": 640, "height": 640}]}})
        self.send(404, {})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        if self.path.split("?")[0] == "/collection/v2/paging" and body.get("username") == USER and body.get("set") == "collection":
            items = [{"uri": "spotify:track:" + made(n)[0], "added_at": at} for n, at in LIKED]
            items += [{"uri": "spotify:album:" + made(n)[0], "added_at": at} for n, at in SAVED]
            return self.send(200, {"items": items})
        self.send(404, {})

    def log_message(self, *args):
        sys.stderr.write("fake spotify: %s\n" % (args[0] % args[1:]))


server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
print("http://127.0.0.1:%d" % PORT, flush=True)
server.serve_forever()
