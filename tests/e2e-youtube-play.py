#!/usr/bin/env python3
"""Plays one item through rex as Plex Web does, for tests/e2e-youtube.sh: the decision, start.m3u8,
the session's media playlist, its first segment (a cold start) and one in its middle (a seek, which
restarts rex's FFmpeg there). Each segment must hold the picture at the wanted height and the sound,
the picture starting with the sound. Prints what it measured; exits 1 on a failure.

    python3 tests/e2e-youtube-play.py <rex url> <token> <ratingKey> <height> <ffprobe> <work dir>
"""
import json
import os
import subprocess
import sys
import time
import urllib.parse
import urllib.request

REX, TOKEN, KEY, HEIGHT, FFPROBE, WORK = sys.argv[1:7]
SESSION = "e2e-yt-%s-%d" % (KEY, int(time.time()))
# Plex Web 4.160's own request for a conversion, as rex's tests recorded it from a real PMS.
QUERY = [("hasMDE", "1"), ("path", "/library/metadata/%s" % KEY), ("mediaIndex", "0"), ("partIndex", "0"), ("protocol", "hls"),
         ("fastSeek", "1"), ("directPlay", "0"), ("directStream", "1"), ("subtitleSize", "100"), ("audioBoost", "100"),
         ("location", "lan"), ("addDebugOverlay", "0"), ("autoAdjustQuality", "0"), ("directStreamAudio", "1"),
         ("autoAdjustSubtitle", "1"), ("mediaBufferSize", "102400"), ("session", SESSION), ("subtitles", "burn"),
         ("X-Plex-Session-Identifier", "playback-" + SESSION),
         ("X-Plex-Client-Profile-Extra", "append-transcode-target-codec(type=videoProfile&context=streaming&videoCodec=h264&audioCodec=aac&protocol=hls)"),
         ("X-Plex-Incomplete-Segments", "1"), ("X-Plex-Product", "Plex Web"), ("X-Plex-Version", "4.160.0"),
         ("X-Plex-Client-Identifier", "rex-addons-e2e"), ("X-Plex-Platform", "Chrome"), ("X-Plex-Platform-Version", "151.0"),
         ("X-Plex-Device", "Linux"), ("X-Plex-Device-Name", "Chrome"), ("X-Plex-Token", TOKEN)]
failed = []


def check(what, ok, detail=""):
    print("  %s %s%s" % ("ok  " if ok else "FAIL", what, ": " + detail if detail else ""))
    if not ok:
        failed.append(what)


def get(path, query=QUERY, accept=None):
    url = "%s%s?%s" % (REX, path, urllib.parse.urlencode(query))
    request = urllib.request.Request(url, headers={"Accept": accept} if accept else {})
    try:
        with urllib.request.urlopen(request, timeout=120) as answer:
            return answer.status, answer.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def probe(data, name):
    path = os.path.join(WORK, name)
    with open(path, "wb") as handle:
        handle.write(data)
    out = subprocess.run([FFPROBE, "-v", "error", "-show_streams", "-show_format", "-of", "json", path], capture_output=True, text=True)
    return json.loads(out.stdout or "{}")


def segment(name, data, label):
    info = probe(data, name)
    streams = info.get("streams", [])
    video = next((s for s in streams if s["codec_type"] == "video"), {})
    audio = next((s for s in streams if s["codec_type"] == "audio"), {})
    check("%s holds H.264 at %sp and AAC" % (label, HEIGHT),
          video.get("codec_name") == "h264" and str(video.get("height")) == HEIGHT and audio.get("codec_name") == "aac",
          "%s %sx%s + %s, %d bytes" % (video.get("codec_name"), video.get("width"), video.get("height"), audio.get("codec_name"), len(data)))
    gap = float(video.get("start_time", 99)) - float(audio.get("start_time", 0))
    check("%s: the picture starts with the sound" % label, abs(gap) < 0.5, "picture %+.3f s from the sound" % gap)
    return float(info.get("format", {}).get("start_time", -1))


status, body = get("/video/:/transcode/universal/decision", accept="application/json")
container = json.loads(body).get("MediaContainer", {}) if status == 200 else {}
media = container.get("Metadata", [{}])[0].get("Media", [{}])[0]
decisions = {s.get("streamType"): s.get("decision") for s in media.get("Part", [{}])[0].get("Stream", [])}
check("decision: a conversion that copies picture and sound", container.get("generalDecisionCode") == 1001 and decisions.get(1) == "copy" and decisions.get(2) == "copy",
      "%s %s, %s, picture %s, sound %s" % (status, container.get("generalDecisionCode"), media.get("protocol"), decisions.get(1), decisions.get(2)))

started = time.time()
status, master = get("/video/:/transcode/universal/start.m3u8")
check("start.m3u8", status == 200 and b"session/%s/base/index.m3u8" % SESSION.encode() in master, "%s %r" % (status, master[-80:]))
base = "/video/:/transcode/universal/session/%s/base/" % SESSION
status, playlist = get(base + "index.m3u8", [])
names = [line for line in playlist.decode().splitlines() if line and not line.startswith("#")]
length = next((float(line.split(":")[1]) for line in playlist.decode().splitlines() if line.startswith("#EXT-X-TARGETDURATION")), 0)
check("the session's media playlist", status == 200 and len(names) >= 2, "%d segments of %s s" % (len(names), length))
status, first = get(base + names[0], [])
cold = time.time() - started
check("first segment", status == 200 and first[:1] == b"\x47", "%s, %.2f s from start.m3u8 (cold)" % (status, cold))
start0 = segment("first.ts", first, "first segment")

far = len(names) // 2
started = time.time()
status, middle = get(base + names[far], [])
seek = time.time() - started
check("segment %d (a seek to the middle)" % far, status == 200 and middle[:1] == b"\x47", "%s, %.2f s to its bytes" % (status, seek))
at = segment("middle.ts", middle, "middle segment") - start0
check("the middle segment is at its own time", abs(at - far * length) < 1.6, "at %.2f s, want %.0f s" % (at, far * length))

get("/video/:/transcode/universal/stop", [("session", SESSION), ("X-Plex-Token", TOKEN)])
print("TIMINGS cold %.2f s, seek %.2f s" % (cold, seek))
sys.exit(1 if failed else 0)
