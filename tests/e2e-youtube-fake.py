#!/usr/bin/env python3
"""YouTube's answers for tests/e2e-youtube.sh with FAKE_YOUTUBE=1: the recorded playlist pages and
player answers in tests/fixtures/youtube, a master playlist in YouTube's shape (fabricated), and
the HLS media in MEDIA_DIR (made by FFmpeg), on this machine's network address, as the sandbox
never fetches loopback. The same routes src/youtube_tests.rs serves.

    python3 tests/e2e-youtube-fake.py <port> <media dir>     # prints its base URL, then serves
"""
import json
import os
import socket
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures", "youtube")
PORT, MEDIA = int(sys.argv[1]), sys.argv[2]


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


class Handler(BaseHTTPRequestHandler):
    def send(self, status, body, kind="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        path = self.path.split("?")[0]
        if path == "/youtubei/v1/browse":
            if body.get("browseId") == "VLPL6B3937A5D230E335":
                return self.send(200, PAGE1)
            if body.get("continuation") == TOKEN:
                return self.send(200, load("tv-browse-PL6B3937A5D230E335-page2.json"))
            return self.send(404, {})
        if path == "/youtubei/v1/player":
            video = body.get("videoId", "")
            if body["context"]["client"]["clientName"] == "WEB":
                return self.send(200, {"playabilityStatus": {"status": "OK"},
                                       "microformat": {"playerMicroformatRenderer": {"publishDate": "2026-05-11T05:00:01-07:00"}}})
            name = os.path.join(FIXTURES, "ios-player-%s.json" % video)
            if not os.path.exists(name):
                return self.send(404, {})
            player = load(os.path.basename(name))
            if video != "SkVqJ1SGeL0":  # fabricated: the HLS master the measured iOS answer carries
                player["streamingData"]["hlsManifestUrl"] = "%s/api/manifest/hls_variant/id/%s/file/index.m3u8" % (BASE, video)
            return self.send(200, player)
        if path == "/o/oauth2/device/code":
            return self.send(200, {"device_code": "DEVICE-1", "user_code": "ABC-DEF-GHI", "expires_in": 1800, "interval": 5,
                                   "verification_url": "https://www.google.com/device"})
        if path == "/o/oauth2/token":
            return self.send(200, {"error": "authorization_pending"})
        if path == "/o/oauth2/revoke":
            return self.send(200, {})
        self.send(404, {})

    def do_GET(self):
        path = self.path.split("?")[0]
        if path.startswith("/api/manifest/hls_variant/id/"):
            video = path.split("/")[5]
            with open(os.path.join(FIXTURES, "hls-master.FABRICATED.m3u8")) as handle:
                master = handle.read().replace("{BASE}", BASE).replace("{VIDEO}", video)
            return self.send(200, master.encode(), "application/vnd.apple.mpegurl")
        if path.startswith("/api/manifest/hls_playlist/"):
            name = os.path.join(MEDIA, os.path.basename(path))
            if os.path.isfile(name):
                with open(name, "rb") as handle:
                    return self.send(200, handle.read(), "application/vnd.apple.mpegurl" if name.endswith(".m3u8") else "video/mp4")
        self.send(404, {})

    do_HEAD = do_GET

    def log_message(self, *args):
        sys.stderr.write("fake youtube: %s\n" % (args[0] % args[1:]))


server = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
print(BASE, flush=True)
server.serve_forever()
