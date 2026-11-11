#!/usr/bin/env python3
"""YouTube's answers for tests/e2e-youtube.sh with FAKE_YOUTUBE=1, the same routes src/youtube_tests.rs
serves: the recorded playlist pages in tests/fixtures/youtube, the TV player's answer (fabricated, in
the measured shape) whose file links answer only once their n (and signature) are transformed as the
TV player script f5321ed3 transforms them, that script and the file from MEDIA_DIR (made by the test),
on this machine's network address, as the sandbox never fetches loopback. Every InnerTube call must
carry the test's token.

    python3 tests/e2e-youtube-fake.py <port> <media dir>     # prints its base URL, then serves
"""
import json
import os
import socket
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures", "youtube")
PORT, MEDIA = int(sys.argv[1]), sys.argv[2]
PLAYER = "/s/player/f5321ed3/tv-player-ias.vflset/tv-player-ias.js"
# yt-dlp-ejs 0.8.0's answers for player f5321ed3, as in src/youtube_tests.rs.
N = {"ZdZIqFPQK-Ty8wId": "vbisBPXSXnof", "abcdefghijklmnop": "k15_hiWZkeX-"}
SIG = ("0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz",
       "456789abcdefghij1lmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789azcdebghijklmnopqrstuvwxyf")
# Each video with a file: its n challenge, and whether its link comes as a signatureCipher.
FILES = {"l5OZu-IrXpw": ("ZdZIqFPQK-Ty8wId", False), "u9lj-c29dxI": ("abcdefghijklmnop", True)}


def lan_ip():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.connect(("192.0.2.1", 9))
        return probe.getsockname()[0]


BASE = "http://%s:%d" % (lan_ip(), PORT)


def load(name):
    with open(os.path.join(FIXTURES, name)) as handle:
        return json.load(handle)


PAGE1 = load("tv-browse-PL6B3937A5D230E335.json")
TOKEN = PAGE1["contents"]["tvBrowseRenderer"]["content"]["tvSurfaceContentRenderer"]["content"]["twoColumnRenderer"][
    "rightColumn"]["playlistVideoListRenderer"]["continuations"][0]["nextContinuationData"]["continuation"]


def player(video):
    if video not in FILES:
        return None
    challenge, ciphered = FILES[video]
    with open(os.path.join(FIXTURES, "tv-player-l5OZu-IrXpw.FABRICATED.json")) as handle:
        text = handle.read().replace("{BASE}", BASE).replace("{N}", challenge).replace("o-FABRICATED", "o-" + video)
    answer = json.loads(text)
    answer["videoDetails"]["videoId"] = video
    file = answer["streamingData"]["formats"][0]
    if ciphered:
        url = file.pop("url").replace("&sig=FABRICATED", "")
        file["signatureCipher"] = urllib.parse.urlencode({"s": SIG[0], "sp": "sig", "url": url})
    return answer


class Handler(BaseHTTPRequestHandler):
    def send(self, status, body, kind="application/json", headers=()):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        path = self.path.split("?")[0]
        if path.startswith("/youtubei/") and self.headers.get("Authorization") != "Bearer fake-access":
            return self.send(401, {"error": {"code": 401}})
        if path == "/youtubei/v1/browse":
            if body.get("browseId") == "VLPL6B3937A5D230E335":
                return self.send(200, PAGE1)
            if body.get("continuation") == TOKEN:
                return self.send(200, load("tv-browse-PL6B3937A5D230E335-page2.json"))
            return self.send(404, {})
        if path == "/youtubei/v1/player":
            answer = player(body.get("videoId", ""))
            return self.send(200, answer) if answer else self.send(404, {})
        if path == "/o/oauth2/device/code":
            return self.send(200, {"device_code": "DEVICE-1", "user_code": "ABC-DEF-GHI", "expires_in": 1800, "interval": 5,
                                   "verification_url": "https://www.google.com/device"})
        if path == "/o/oauth2/token":
            if body.get("grant_type") == "refresh_token" and body.get("refresh_token") == "fake-refresh":
                return self.send(200, {"access_token": "fake-access", "expires_in": 3600, "token_type": "Bearer"})
            return self.send(200, {"error": "authorization_pending"})
        if path == "/o/oauth2/revoke":
            return self.send(200, {})
        self.send(404, {})

    def do_GET(self):
        path, _, query = self.path.partition("?")
        if path == "/tv":
            page = '<script>ytcfg.set({"WEB_PLAYER_CONTEXT_CONFIGS":{"WEB_PLAYER_CONTEXT_CONFIG_ID_LIVING_ROOM_WATCH":{"jsUrl":"%s"}}});</script>' % PLAYER
            return self.send(200, page.encode(), "text/html")
        if path == PLAYER:
            with open(os.path.join(MEDIA, "player.js"), "rb") as handle:
                return self.send(200, handle.read(), "text/javascript")
        if path == "/videoplayback":
            params = dict(urllib.parse.parse_qsl(query))
            video = params.get("id", "").removeprefix("o-")
            challenge, ciphered = FILES.get(video, (None, False))
            if not challenge or params.get("n") != N[challenge] or (ciphered and params.get("sig") != SIG[1]):
                return self.send(403, b"", "text/plain")
            with open(os.path.join(MEDIA, "video.mp4"), "rb") as handle:
                data = handle.read()
            asked = self.headers.get("Range", "")
            if not asked.startswith("bytes="):
                return self.send(200, data, "video/mp4", [("Accept-Ranges", "bytes")])
            start, _, end = asked[6:].partition("-")
            start, end = int(start or 0), min(int(end) if end else len(data) - 1, len(data) - 1)
            return self.send(206, data[start:end + 1], "video/mp4", [("Content-Range", "bytes %d-%d/%d" % (start, end, len(data)))])
        self.send(404, {})

    do_HEAD = do_GET

    def log_message(self, *args):
        sys.stderr.write("fake youtube: %s\n" % (args[0] % args[1:]))


server = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
print(BASE, flush=True)
server.serve_forever()
