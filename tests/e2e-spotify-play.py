#!/usr/bin/env python3
"""Plays one track through rex as Plexamp does, for tests/e2e-spotify.sh: Plexamp 4.12.4's own requests,
as rex's tests/fixtures/music/plexamp-play.jsonl recorded them against a real PMS (plexav, 2026-10-31):
a play queue of the track's album, the track's loudness levels, the queue's window, the whole file
read with download=1, a timeline; then Plex for Android's music decision for it, as
droid-music-play.jsonl recorded it. Prints what rex answered; exits 1 when the track did not play.

    python3 tests/e2e-spotify-play.py <rex url> <token> <ratingKey> <ffprobe> <work dir>
"""
import json
import os
import subprocess
import sys
import time
import urllib.parse
import urllib.request

REX, TOKEN, KEY, FFPROBE, WORK = sys.argv[1:6]
PLEXAMP = {"X-Plex-Product": "Plexamp", "X-Plex-Version": "4.12.4", "X-Plex-Client-Identifier": "rex-addons-e2e-plexamp",
           "X-Plex-Platform": "Android", "X-Plex-Platform-Version": "13", "X-Plex-Device": "Android", "X-Plex-Token": TOKEN,
           "X-Plex-Provides": "client,player,pubsub-player", "X-Plex-Features": "external-media", "User-Agent": "TREBLE/2.1",
           "Accept": "application/json"}
failed = []


def check(what, ok, detail=""):
    print("  %s %s%s" % ("ok  " if ok else "FAIL", what, ": " + detail if detail else ""))
    if not ok:
        failed.append(what)


def ask(method, path, query, headers=PLEXAMP):
    url = "%s%s?%s" % (REX, path, urllib.parse.urlencode(query))
    request = urllib.request.Request(url, method=method, headers=headers)
    started = time.time()
    try:
        with urllib.request.urlopen(request, timeout=180) as answer:
            body = answer.read()
            return answer.status, body, dict(answer.headers), time.time() - started
    except urllib.error.HTTPError as error:
        return error.code, error.read(), dict(error.headers), time.time() - started


status, body, _, _ = ask("GET", "/identity", [], {"Accept": "application/json"})
machine = json.loads(body)["MediaContainer"]["machineIdentifier"]
status, body, _, _ = ask("GET", "/library/metadata/%s" % KEY, [])
row = json.loads(body)["MediaContainer"]["Metadata"][0]
part = row["Media"][0]["Part"][0]
stream = next(s for s in part["Stream"] if s.get("streamType") == 2)
print("  track: %s, %s, %d ms, Part %s (%s bytes), its album %s" % (row["type"], row["Media"][0].get("audioCodec"), row.get("duration", 0),
                                                                  part["key"].rsplit("/", 1)[-1], part.get("size"), row.get("parentRatingKey")))

# Plexamp: its queue of the album, the track's levels, the queue's window.
status, body, _, took = ask("POST", "/playQueues", [
    ("includeLoudnessRamps", "1"), ("shuffle", "0"),
    ("uri", "server://%s/com.plexapp.plugins.library/library/metadata/%s/children" % (machine, row["parentRatingKey"])),
    ("playlistID", "undefined"), ("type", "audio"), ("includeFields", "thumbBlurHash"), ("X-Plex-Device-Name", "Android")])
queue = json.loads(body)["MediaContainer"] if status == 200 else {}
check("Plexamp's play queue of the album", status == 200 and queue.get("size", 0) >= 1,
      "%s, %s items, %.2f s" % (status, queue.get("size"), took))
status, _, _, took = ask("GET", "/library/streams/%s/levels" % stream["id"], [("subsample", "128"), ("includeFields", "thumbBlurHash"), ("X-Plex-Device-Name", "Android")])
check("the track's levels: 404, as an unanalysed PMS answers", status == 404, "%s in %.3f s" % (status, took))
status, _, _, took = ask("GET", "/playQueues/%s" % queue.get("playQueueID"), [("window", "30"), ("includeFields", "thumbBlurHash"), ("X-Plex-Device-Name", "Android")])
check("the queue's window", status == 200, "%s in %.2f s" % (status, took))

# The whole file, as Plexamp reads it before it plays.
status, data, headers, took = ask("GET", part["key"], [
    ("download", "1"), ("X-Plex-Client-Identifier", "rex-addons-e2e-plexamp"), ("X-Plex-Device", "Android"),
    ("X-Plex-Device-Name", "Android"), ("X-Plex-Platform", "Generic"), ("X-Plex-Platform-Version", "13"),
    ("X-Plex-Product", "Plexamp"), ("X-Plex-Token", TOKEN), ("X-Plex-Version", "4.12.4")], {"User-Agent": "TREBLE/2.1"})
download = took
path = os.path.join(WORK, "plexamp-download.m4a")
with open(path, "wb") as handle:
    handle.write(data)
probe = json.loads(subprocess.run([FFPROBE, "-v", "error", "-show_streams", "-show_format", "-of", "json", path], capture_output=True, text=True).stdout or "{}")
audio = next((s for s in probe.get("streams", []) if s.get("codec_type") == "audio"), {})
seconds = float(probe.get("format", {}).get("duration", 0))
check("the whole file with download=1", status == 200 and len(data) == int(part.get("size") or len(data)) and audio.get("codec_name") == "aac",
      "%s, %s, %d bytes in %.2f s (%.1f MB/s), ffprobe: %s %s Hz, %.1f s" % (status, headers.get("Content-Type"), len(data), took,
                                                                            len(data) / max(took, 0.001) / 1e6, audio.get("codec_name"), audio.get("sample_rate"), seconds))
check("its length is the track's", abs(seconds * 1000 - row.get("duration", 0)) < 1500, "%.1f s vs %d ms" % (seconds, row.get("duration", 0)))
status, _, _, _ = ask("GET", "/:/timeline", [("state", "playing"), ("duration", str(row.get("duration", 0))), ("time", "1000"),
                                              ("key", "/library/metadata/%s" % KEY), ("ratingKey", KEY), ("playQueueID", str(queue.get("playQueueID"))),
                                              ("containerKey", "/playQueues/%s" % queue.get("playQueueID")), ("hasMDE", "1"), ("X-Plex-Device-Name", "Android")])
check("a timeline", status == 200, str(status))

# Plex for Android's music decision for the track.
status, body, _, took = ask("GET", "/music/:/transcode/universal/decision", [
    ("directPlay", "1"), ("directStream", "1"), ("directStreamAudio", "1"), ("protocol", "hls"), ("fastSeek", "1"),
    ("path", "/library/metadata/%s" % KEY), ("session", "rex-addons-e2e-%d" % time.time()), ("mediaIndex", "0"), ("partIndex", "0"),
    ("mediaBufferSize", "25000"), ("hasMDE", "1"), ("audioBoost", "100"), ("musicBitrate", "192"), ("location", "lan"),
    ("X-Plex-Client-Profile-Extra", "add-transcode-target(type=musicProfile&context=all&protocol=hls&container=mkv&videoCodec=&audioCodec=aac,pcm,mp3,ac3,dca)"),
    ("X-Plex-Client-Profile-Name", "Generic"), ("X-Plex-Product", "Plex for Android (Mobile)"), ("X-Plex-Platform", "Android"),
    ("X-Plex-Version", "2026.21.0"), ("X-Plex-Client-Identifier", "rex-addons-e2e-android"), ("X-Plex-Token", TOKEN)],
    {"Accept": "application/json"})
container = json.loads(body).get("MediaContainer", {}) if status == 200 else {}
media = (container.get("Metadata") or [{}])[0].get("Media", [{}])[0]
decided = [s.get("decision") for s in (media.get("Part") or [{}])[0].get("Stream", [])]
# A real PMS answers a playable track "mdeDecisionCode 1000, Direct play OK." (droid-music-play.jsonl).
part_decision = (media.get("Part") or [{}])[0].get("decision")
check("Plex for Android's music decision: direct play", status == 200 and container.get("mdeDecisionCode") == 1000,
      "%s: %s %r, Part decision %s, streams %s, in %.2f s" % (status, container.get("mdeDecisionCode"), container.get("mdeDecisionText"),
                                                            part_decision, decided, took))
print("TIMINGS download %.2f s" % download)
sys.exit(1 if failed else 0)
