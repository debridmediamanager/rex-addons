#!/usr/bin/env bash
# The spotify addon end to end, against a real rex with FFmpeg: the Spotify addon signs in with a code
# through the host (live, the script prints the code and waits while a person enters it at
# spotify.com/pair; with the fake, it approves it itself), the YouTube addon starts signed in (the
# test seeds its tokens, or live without YT_TOKEN_FILE signs it in with a code as well); the library's counts are shown (and checked against EXPECT_LIKED,
# EXPECT_ALBUMS and EXPECT_PLAYLISTS when set, within 10%), and no Spotify token is in any log; the first MATCHES
# tracks of the library are found on YouTube and reach rex's music library called Spotify, under their
# albums and artists, and every playlist is in rex in Spotify's order; one track's sound is read
# through rex's Part path with Range (its start and middle) and is AAC of the track's length; it plays
# as Plexamp plays it, and Plex for Android's music decision for it is asked; removing the addon
# removes its music from rex.
#
#   tests/e2e-spotify.sh                  # live: signs YouTube in with a code too
#   YT_TOKEN_FILE=token.json tests/e2e-spotify.sh
#   YT_TOKEN_FILE=token.json EXPECT_LIKED=96 EXPECT_ALBUMS=16 EXPECT_PLAYLISTS=24 tests/e2e-spotify.sh
#   REX_BIN=/path/to/rex ... tests/e2e-spotify.sh
#   FAKE_SPOTIFY=1 tests/e2e-spotify.sh   # Spotify (sign-in, apresolve, spclient) from
#                                         # tests/e2e-spotify-fake.py, YouTube from tests/e2e-youtube-fake.py,
#                                         # and sound FFmpeg makes
#
# The token file is YouTube's answer to the TV sign-in ({access_token, refresh_token, expires_in}). The
# script copies it only into its own work directory, mode 600, which it deletes, as it does the Spotify
# tokens the sign-in leaves in the addon's storage; run it with TMPDIR pointing where such a copy may
# go. It prints nothing from the account but the code to enter: counts, lengths and codecs.
#
# Live, it lists the account's Spotify library (a few dozen requests) and looks for MATCHES tracks on
# YouTube (15 by default: a search and a player call or two each), plus one player call per play;
# run it rarely from one address. rex runs unclaimed on scratch ports with no state, no GDM, no
# plex.tv and no rex-direct. Needs ffmpeg, ffprobe, python3 and, without REX_BIN, gh.
set -euo pipefail
cd "$(dirname "$0")/.."
ffprobe=${FFPROBE:-$(command -v ffprobe || true)}
ffmpeg=${FFMPEG:-$(command -v ffmpeg || true)}
[ -x "$ffprobe" ] && [ -x "$ffmpeg" ] || { echo "ffmpeg and ffprobe are needed: set FFMPEG and FFPROBE" >&2; exit 1; }
fake_mode=${FAKE_SPOTIFY:-}
if [ "$fake_mode" != 1 ]; then
    [ -z "${YT_TOKEN_FILE:-}" ] || [ -r "$YT_TOKEN_FILE" ] || { echo "YT_TOKEN_FILE $YT_TOKEN_FILE cannot be read" >&2; exit 1; }
fi
rex_port=${REX_PORT:-32760}
host_port=${HOST_PORT:-32770}
fake_port=${FAKE_PORT:-32780}
spotify_port=${SPOTIFY_PORT:-32790}
matches=${MATCHES:-15}
work=$(mktemp -d "${TMPDIR:-/tmp}/e2e-spotify.XXXXXX")
chmod 700 "$work"
pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    if [ "${KEEP:-}" = 1 ]; then echo "kept $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
# A name from the account is shown by its length only.
shown() { if [ "$fake_mode" = 1 ]; then printf '"%s"' "$1"; else printf '(%s characters)' "${#1}"; fi; }
fail() {
    echo "FAIL: $*" >&2
    for log in host rex fake spotify; do [ -f "$work/$log.log" ] && { echo "--- $log log" >&2; tail -30 "$work/$log.log" | grep -v 'youtubei/search' >&2; }; done
    exit 1
}

if [ -z "${REX_BIN:-}" ]; then
    tag=$(gh release list -R debridmediamanager/rex --limit 20 --json tagName --jq '[.[] | select(.tagName | endswith("nightly"))][0].tagName')
    gh release download "$tag" -R debridmediamanager/rex -p '*darwin-arm64.tar.gz' -D "$work/rex-release" >/dev/null
    tar xzf "$work"/rex-release/*.tar.gz -C "$work/rex-release"
    REX_BIN=$(find "$work/rex-release" -type f -name rex -perm -u+x | head -1)
    echo "rex $tag"
fi
bin=${REX_ADDONS_BIN:-}
if [ -z "$bin" ]; then
    cargo build --release --quiet
    bin=${CARGO_TARGET_DIR:-target}/release/rex-addons
fi

# Both addons as they are, the Spotify one looking for MATCHES tracks a sync.
mkdir -p "$work/bundled/youtube" "$work/bundled/spotify"
cp community/addons/youtube/main.js "$work/bundled/youtube/main.js"
perl -pe 's/\{"key": "matches_per_sync"(.*)"default": 30\}/{"key": "matches_per_sync"$1"default": '"$matches"'}/' community/addons/spotify/manifest.json >"$work/bundled/spotify/manifest.json"
cp community/addons/spotify/main.js "$work/bundled/spotify/main.js"
cp community/addons/youtube/manifest.json "$work/bundled/youtube/manifest.json"
token_file=${YT_TOKEN_FILE:-}
stub=
if [ "$fake_mode" = 1 ]; then
    mkdir -p "$work/media"
    # As YouTube's: the sound (AAC, 391 s, the fake player answer's length) and a picture, each an MP4
    # with its index (sidx) after its header, which the fake's player answer is cut from.
    "$ffmpeg" -v error -f lavfi -i sine=frequency=440 -t 391.024 -c:a aac -b:a 128k -f mp4 -movflags dash+global_sidx -frag_duration 7000000 "$work/media/dash-audio.mp4"
    "$ffmpeg" -v error -f lavfi -i testsrc=size=320x180:rate=12 -t 4 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -f mp4 -movflags dash+global_sidx -frag_duration 2000000 "$work/media/dash-video.mp4"
    cp "$work/media/dash-audio.mp4" "$work/media/muxed.mp4"
    curl -fsS -o "$work/media/player.js" "https://www.youtube.com/s/player/f5321ed3/tv-player-ias.vflset/tv-player-ias.js"
    python3 tests/e2e-youtube-fake.py "$fake_port" "$work/media" >"$work/fake.url" 2>"$work/fake.log" &
    pids+=($!)
    python3 tests/e2e-spotify-fake.py "$spotify_port" >"$work/spotify.url" 2>"$work/spotify.log" &
    pids+=($!)
    for _ in $(seq 20); do [ -s "$work/fake.url" ] && [ -s "$work/spotify.url" ] && break; sleep 0.5; done
    fake=$(cat "$work/fake.url")
    stub=$(cat "$work/spotify.url")
    fake_host=$(printf '%s' "${fake#http://}" | cut -d: -f1)
    perl -pi -e "s#https://www.youtube.com#$fake#; s/const CHECK_PAUSE_MS = 1500;/const CHECK_PAUSE_MS = 0;/" "$work/bundled/youtube/main.js"
    perl -pi -e 's#https://(accounts|apresolve)\.spotify\.com#'"$stub"'#; s#`https://\$\{host\}#`http://\$\{host\}#; s/const MATCH_PAUSE_MS = 1500;/const MATCH_PAUSE_MS = 0;/' "$work/bundled/spotify/main.js"
    grep -q "const ACCOUNTS = \"$stub\"" "$work/bundled/spotify/main.js" && grep -q '`http://${host}' "$work/bundled/spotify/main.js" || fail "the Spotify addon's links were not pointed at the fake"
    python3 - "$work/bundled" "$fake_host" <<'EOF'
import json, sys
for addon in ("youtube", "spotify"):
    path = "%s/%s/manifest.json" % (sys.argv[1], addon)
    manifest = json.load(open(path))
    json.dump(dict(manifest, hosts=[sys.argv[2]]), open(path, "w"))
EOF
    printf '{"access_token": "fake-access", "refresh_token": "fake-refresh", "expires_in": 3600}' >"$work/fake-token.json"
    token_file="$work/fake-token.json"
    echo "fake YouTube at $fake, fake Spotify at $stub"
fi
# Test-only: YouTube starts signed in from a token file, as a finished sign-in leaves it (without
# one it signs in with a code below). It lists none of its own playlists, so all it does here is what
# the Spotify addon asks of it.
(umask 077
 mkdir -p "$work/data/addons/youtube"
 python3 - "$token_file" "$work/data/addons/youtube" <<'EOF'
import json, sys, time
file, folder = sys.argv[1], sys.argv[2]
json.dump({"liked": False, "watch_later": False, "own": False, "playlists": []}, open(folder + "/config.json", "w"))
if file:
    token = json.load(open(file))
    tokens = {"access": token["access_token"], "refresh": token["refresh_token"], "expires_at": int((time.time() + int(token.get("expires_in", 3600)) - 600) * 1000)}
    json.dump({"tokens": tokens}, open(folder + "/storage.json", "w"))
    json.dump({"enabled": True, "account": "signed in from a token file"}, open(folder + "/state.json", "w"))
EOF
)

token="e2e-$RANDOM$RANDOM"
rex="http://127.0.0.1:$rex_port"
api="http://127.0.0.1:$host_port/api"
REX_DIRECT_URL=off REX_PLEXTV_URL=http://127.0.0.1:9 REX_TRANSCODER="$ffmpeg" REX_ARTWORK_ANALYSIS=off \
    "$REX_BIN" --port "$rex_port" --catalog "$work/rex.db" --no-state --gdm off --token "$token" >"$work/rex.log" 2>&1 &
pids+=($!)
for _ in $(seq 60); do curl -fsS "$rex/identity" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "$rex/identity" >/dev/null || fail "rex did not start"
"$bin" --listen "127.0.0.1:$host_port" --rex "$rex" --data "$work/data" --ffprobe "$ffprobe" --bundled "$work/bundled" >"$work/host.log" 2>&1 &
pids+=($!)
for _ in $(seq 30); do curl -fsS "$api/addons/spotify" >/dev/null 2>&1 && break; sleep 1; done

# Signs `$1` in with a code: entered by a person live, approved by the fake (`$2`) otherwise.
sign_in() {
    local pending code addon
    pending=$(curl -fsS -X POST "$api/addons/$1/sign-in")
    code=$(printf '%s' "$pending" | json 'd["sign_in"]["pending"]["code"]')
    echo "SIGN IN to $1: enter $code at $(printf '%s' "$pending" | json 'd["sign_in"]["pending"]["url"]') (the code lives $(( $(printf '%s' "$pending" | json 'd["sign_in"]["pending"]["expires_at"]') - $(date +%s) )) s)"
    [ -z "${2:-}" ] || curl -fsS -X POST "$2" >/dev/null
    for _ in $(seq 400); do
        addon=$(curl -fsS "$api/addons/$1")
        [ "$(printf '%s' "$addon" | json 'd["sign_in"]["account"] is not None')" = True ] && break
        [ "$(printf '%s' "$addon" | json 'd["sign_in"]["pending"] is None')" = True ] && fail "the $1 sign-in ended: $(printf '%s' "$addon" | json 'd["sign_in"]["error"]')"
        sleep 3
    done
    [ "$(printf '%s' "$addon" | json 'd["sign_in"]["account"] is not None')" = True ] || fail "nobody entered the $1 code in 20 minutes"
    echo "$1 signed in as $(shown "$(printf '%s' "$addon" | json 'd["sign_in"]["account"]')")"
}
[ -n "$token_file" ] || sign_in youtube

status=$(curl -fsS "$api/addons/spotify")
echo "$(printf '%s' "$status" | json 'd["name"]') $(printf '%s' "$status" | json 'd["version"]'), \"$(printf '%s' "$status" | json 'd["sign_in"]["label"]')\", requires $(printf '%s' "$status" | json 'json.dumps(d["requires"])')"
[ "$(printf '%s' "$status" | json 'd["sign_in"]["kind"]')" = code ] || fail "the addon does not sign in with a code"
[ "$(printf '%s' "$status" | json 'd["requires"][0]["ok"]')" = True ] || fail "the YouTube addon is not ready for it"

sign_in spotify "${stub:+$stub/approve}"

echo "waiting for a sync..."
started=$(date +%s)
for _ in $(seq 200); do
    addon=$(curl -fsS "$api/addons/spotify" 2>/dev/null || true)
    state=$(printf '%s' "$addon" | json 'd["state"]' 2>/dev/null || true)
    items=$(printf '%s' "$addon" | json 'd["items"]' 2>/dev/null || echo 0)
    [ "$state" = error ] && fail "spotify sync: $(printf '%s' "$addon" | json 'd["error"]')"
    [ "$state" = idle ] && [ "$items" -ge 1 ] && break
    sleep 3
done
[ "$items" -ge 1 ] || fail "rex shows no track after 10 minutes"
echo "synced in $(( $(date +%s) - started )) s: $(printf '%s' "$addon" | json 'json.dumps({k: d[k] for k in ("items", "held", "error")})')"
grep -E "\[addon spotify\] (library|[0-9]+ matched)" "$work/host.log" | sed -E 's/.*\[addon spotify\] //'
# What the library holds, from what the addon kept; and no Spotify token in any log or answer.
python3 - "$work" "$api" <<'EOF' || fail "the library's counts are off, or a Spotify token was logged or shown"
import json, os, sys, urllib.request
work, api = sys.argv[1], sys.argv[2]
storage = json.load(open(work + "/data/addons/spotify/storage.json"))
liked = next((len(l["tracks"]) for l in storage["lists"] if l["title"] == "Liked Songs"), 0)
counts = {"LIKED": liked, "ALBUMS": len(storage["albums"]), "PLAYLISTS": len(storage["playlists"])}
print("  library: %d liked tracks, %d saved albums, %d playlists" % (liked, counts["ALBUMS"], counts["PLAYLISTS"]))
for name, got in counts.items():
    want = os.environ.get("EXPECT_" + name)
    if want and abs(got - int(want)) > max(1, int(want) // 10):
        sys.exit("  %s: %d, expected about %s" % (name.lower(), got, want))
assert liked and counts["PLAYLISTS"], "no liked track or playlist"
tokens = storage["tokens"]
secrets = [s for s in (tokens["access"], tokens["refresh"]) if s]
answers = [urllib.request.urlopen(api + path).read().decode() for path in ("/status", "/addons/spotify", "/addons/spotify/log")]
for name in ("host.log", "rex.log"):
    answers.append(open(os.path.join(work, name), errors="replace").read())
leaks = sum(secret in text for secret in secrets for text in answers)
print("  Spotify tokens in the logs and the API's answers: %d" % leaks)
assert leaks == 0
EOF
echo "YouTube calls: $(grep -c 'youtubei/search' "$work/host.log" || true) searches, $(grep -c 'youtubei/player' "$work/host.log" || true) player calls"
grep -q "probe failed" "$work/host.log" && fail "the host had to probe: a track was sent unprobed"
grep -q "rejected" "$work/host.log" && fail "rex rejected a track: $(grep rejected "$work/host.log" | head -1)"

# The music library: its tracks, albums and artists, titled as Spotify titles them.
plex() { curl -fsS -H "X-Plex-Token: $token" -H "Accept: application/json" "$rex$1"; }
section=$(plex /library/sections | json '[s["key"] for s in d["MediaContainer"]["Directory"] if s["title"] == "Spotify" and s["type"] == "artist"][0]')
counts=$(for type in 8 9 10; do plex "/library/sections/$section/all?type=$type" | json 'd["MediaContainer"]["size"]'; done | tr '\n' ' ')
echo "music library Spotify ($section): artists, albums, tracks = $counts"
read -r artists albums tracks <<<"$counts"
[ "$tracks" = "$items" ] || fail "rex shows $tracks tracks, the addon sent $items"
[ "$albums" -ge 1 ] && [ "$artists" -ge 1 ] || fail "no album or artist"
plex "/library/sections/$section/all?type=10" >"$work/tracks.json"
python3 - "$work/tracks.json" "$work/data/addons/spotify/storage.json" "$fake_mode" <<'EOF' || fail "a track is not titled, numbered or dressed as Spotify has it"
import json, sys
rows, storage, fake = json.load(open(sys.argv[1]))["MediaContainer"]["Metadata"], json.load(open(sys.argv[2])), sys.argv[3] == "1"
spotify = {(t["title"], t["album"]["title"], t["album"]["artist"]): t for k, t in storage.items() if k.startswith("t:")}
named = sum(1 for r in rows if (r["title"], r["parentTitle"], r["grandparentTitle"]) in spotify)
numbered = sum(1 for r in rows if spotify.get((r["title"], r["parentTitle"], r["grandparentTitle"]), {}).get("track") == r.get("index"))
pictured = sum(1 for r in rows if r.get("thumb") and r.get("grandparentThumb"))
print("  %d of %d tracks titled as Spotify, under its album and album artist; %d numbered as there; %d with a cover and an artist picture" % (named, len(rows), numbered, pictured))
if fake:
    print("  " + "; ".join("%s / %s / %s #%s" % (r["grandparentTitle"], r["parentTitle"], r["title"], r.get("index")) for r in rows))
assert named == len(rows) and numbered == len(rows)
EOF

# Every playlist in rex in Spotify's order of the tracks that reached rex.
python3 - "$rex" "$token" "$work/data/addons/spotify/storage.json" "$fake_mode" <<'EOF' || fail "a playlist is not in Spotify's order"
import json, sys, urllib.request
rex, token, storage, fake = sys.argv[1], sys.argv[2], json.load(open(sys.argv[3])), sys.argv[4] == "1"
def get(path):
    return json.load(urllib.request.urlopen(urllib.request.Request(rex + path, headers={"X-Plex-Token": token, "Accept": "application/json"})))["MediaContainer"]
feed = json.load(urllib.request.urlopen(rex + "/rex/feed/addon-spotify/items"))["items"]
key = {i["id"]: str(i["ratingKey"]) for i in feed if not i.get("deleted")}
playlists = {p["title"]: p["ratingKey"] for p in get("/playlists").get("Metadata", [])}
checked = 0
for at, wanted in enumerate(storage["lists"]):
    want = [key["sp-" + t] for t in dict.fromkeys(wanted["tracks"]) if "sp-" + t in key]
    got = [str(m["ratingKey"]) for m in get("/playlists/%s/items" % playlists[wanted["title"]]).get("Metadata", [])] if wanted["title"] in playlists else []
    name = wanted["title"] if fake else "(%d characters)" % len(wanted["title"])
    print("  playlist %d %s: %d of its %d tracks in rex, %s" % (at, name, len(want), len(wanted["tracks"]), "in Spotify's order" if got == want else "NOT in its order"))
    assert got == want, (got, want)
    checked += bool(want)
assert checked >= 1, "no playlist reached rex"
EOF

# One track's sound through rex's Part path, as a client reads it: its start and middle.
pick=$(python3 - "$work/data/addons/spotify/storage.json" "$rex" <<'EOF'
import json, sys, urllib.request
storage = json.load(open(sys.argv[1]))
feed = json.load(urllib.request.urlopen(sys.argv[2] + "/rex/feed/addon-spotify/items"))["items"]
ranked = sorted((-storage["m:" + i["id"][3:]].get("score", 0), i["ratingKey"]) for i in feed if not i.get("deleted"))
print(ranked[0][1])
EOF
)
plex "/library/metadata/$pick" >"$work/track.json"
part=$(json 'd["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0]["key"]' <"$work/track.json")
size=$(json 'd["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0].get("size", 0)' <"$work/track.json")
length=$(json 'd["MediaContainer"]["Metadata"][0]["duration"]' <"$work/track.json")
case "$part" in *.m4a) ;; *) fail "the track's Part is not its .m4a: $part" ;; esac
probed=$("$ffprobe" -v error -show_entries stream=codec_name,sample_rate,channels:format=duration -of json "$rex$part?X-Plex-Token=$token")
echo "track ratingKey $pick, $length ms, $size bytes: ffprobe through rex: $(printf '%s' "$probed" | json 'json.dumps({"codec": d["streams"][0]["codec_name"], "rate": d["streams"][0]["sample_rate"], "channels": d["streams"][0]["channels"], "seconds": d["format"]["duration"]})')"
printf '%s' "$probed" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['streams'][0]['codec_name'] == 'aac' and abs(float(d['format']['duration']) * 1000 - $length) < 1500" ||
    fail "the sound through rex is not AAC of the track's length"
for range in "0-65535" "$((size / 2))-$((size / 2 + 65535))"; do
    out=$(curl -sS -o "$work/range.bin" -w '%{http_code} %{size_download} %{time_total}' -H "X-Plex-Token: $token" -H "Range: bytes=$range" "$rex$part")
    echo "Part, bytes $range of $size: $out (status, bytes, seconds)"
    [ "${out%% *}" = 206 ] || fail "the Part read of bytes $range answered ${out%% *}"
    [ "$(echo "$out" | cut -d' ' -f2)" = 65536 ] || fail "the Part read of bytes $range did not return 65536 bytes"
done
[ "$(head -c 8 "$work/range.bin" | tail -c 4)" != ftyp ] || fail "the middle read returned the file's start"

echo "playing ratingKey $pick as Plexamp does:"
python3 tests/e2e-spotify-play.py "$rex" "$token" "$pick" "$ffprobe" "$work" || fail "the track did not play as Plexamp plays it"

curl -fsS -X DELETE "$api/addons/spotify" | tee "$work/removed.json"; echo
curl -s -o /dev/null -w '%{http_code}' "$rex/rex/sources/addon-spotify" | grep -q 404 || fail "rex still has the source"
left=$(plex "/library/sections/$section/all?type=10" | json 'd["MediaContainer"]["size"]')
echo "after removing the addon: $left tracks in Spotify"
[ "$left" = 0 ] || fail "its music is still in rex"
echo "PASS"
