#!/usr/bin/env python3
"""Spotify for tests/e2e-spotify.sh with FAKE_SPOTIFY=1, on this machine's network address (the sandbox
never fetches loopback), the addon's links pointed here: its sign-in (the OAuth device flow at
/oauth2/device/authorize and /api/token, approved by `POST /approve`), apresolve (`/?type=spclient`,
naming this server) and its own API (spclient), which wants the bearer token handed out. FABRICATED:
the sign-in answers in the shape measured on 2026-11-14, and a library in the structure the owner's
account was answered in on 2026-11-11 (rootlist, playlist, collection paging, track, album and artist
metadata). Its band's songs are the ones tests/e2e-youtube-fake.py's search finds; one track YouTube
has none of.

    python3 tests/e2e-spotify-fake.py <port>     # prints its base URL, then serves
"""
import json
import socket
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
B62 = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
USER = "e2e-user"
CLIENT_ID = "65b708073fc0480ea92a077233ca87bd"


def lan_ip():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.connect(("192.0.2.1", 9))
        return probe.getsockname()[0]


HOST = "%s:%d" % (lan_ip(), PORT)
# The sign-in: approved once the test posts /approve; the access tokens handed out, the newest valid.
STATE = {"approved": False, "issued": 0}


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

    def signed(self):
        return STATE["issued"] and self.headers.get("Authorization") == "Bearer fake-access-%d" % STATE["issued"]

    def do_GET(self):
        path, _, query = self.path.partition("?")
        parts = path.split("/")
        if path == "/" and query == "type=spclient":
            return self.send(200, {"spclient": [HOST]})
        if not self.signed():
            return self.send(401, {"error": {"status": 401, "message": "Invalid token"}})
        if path == "/user-profile-view/v3/profile/%s" % USER:
            return self.send(200, {"uri": "spotify:user:" + USER, "name": "E2E Listener"})
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
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        path = self.path.split("?")[0]
        if path == "/approve":
            STATE["approved"] = True
            return self.send(200, {})
        if path in ("/oauth2/device/authorize", "/api/token"):
            form = dict(urllib.parse.parse_qsl(raw.decode()))
            if form.get("client_id") != CLIENT_ID:
                return self.send(400, {"error": "invalid_client"})
            if path == "/oauth2/device/authorize":
                return self.send(200, {"device_code": "fake-device", "user_code": "E2E123", "verification_uri": "https://spotify.com/pair",
                                       "verification_uri_complete": "https://spotify.com/pair?code=E2E123", "expires_in": 3599, "interval": 5})
            grant = form.get("grant_type")
            if grant == "urn:ietf:params:oauth:grant-type:device_code" and form.get("device_code") == "fake-device":
                if not STATE["approved"]:
                    return self.send(400, {"error": "authorization_pending"})
                STATE["issued"] += 1
                return self.send(200, {"access_token": "fake-access-%d" % STATE["issued"], "token_type": "Bearer", "expires_in": 3600,
                                       "refresh_token": "fake-refresh", "scope": "user-library-read", "username": USER})
            if grant == "refresh_token" and form.get("refresh_token") == "fake-refresh":
                STATE["issued"] += 1
                return self.send(200, {"access_token": "fake-access-%d" % STATE["issued"], "token_type": "Bearer", "expires_in": 3600})
            return self.send(400, {"error": "invalid_grant"})
        if not self.signed():
            return self.send(401, {"error": {"status": 401, "message": "Invalid token"}})
        body = json.loads(raw or b"{}")
        if self.path.split("?")[0] == "/collection/v2/paging" and body.get("username") == USER and body.get("set") == "collection":
            items = [{"uri": "spotify:track:" + made(n)[0], "added_at": at} for n, at in LIKED]
            items += [{"uri": "spotify:album:" + made(n)[0], "added_at": at} for n, at in SAVED]
            return self.send(200, {"items": items})
        self.send(404, {})

    def log_message(self, *args):
        sys.stderr.write("fake spotify: %s\n" % (args[0] % args[1:]))


server = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
print("http://" + HOST, flush=True)
server.serve_forever()
