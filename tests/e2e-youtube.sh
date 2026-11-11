#!/usr/bin/env bash
# The youtube addon end to end, against a real rex and live YouTube: a test copy of the addon (no
# sign-in, one small public playlist as its extra link) is bundled into rex-addons; its first
# videos reach rex with their titles and the playlist exists in YouTube's order; one plays through
# rex as a Plex client plays an HLS item (the Part read is sent to a conversion, whose playlist and
# first segment come back as real media); a sign-in starts against Google through the app's API,
# stays pending and is cancelled; removing the addon removes its titles from rex.
#
#   tests/e2e-youtube.sh                       # the latest rex nightly for this Mac, downloaded
#   REX_BIN=/path/to/rex tests/e2e-youtube.sh
#   FAKE_YOUTUBE=1 tests/e2e-youtube.sh        # YouTube's recorded answers (tests/e2e-youtube-fake.py)
#                                              # instead of YouTube, and an HLS stream FFmpeg makes
#
# Live, it asks YouTube for one playlist page and a few videos: run it rarely from one address.
# rex runs unclaimed on scratch ports with no state, no GDM, no plex.tv and no rex-direct, with
# FFmpeg as its transcoder. Needs ffmpeg, ffprobe, python3 and, without REX_BIN, gh.
set -euo pipefail
cd "$(dirname "$0")/.."
ffprobe=${FFPROBE:-$(command -v ffprobe || true)}
ffmpeg=${FFMPEG:-$(command -v ffmpeg || true)}
[ -x "$ffprobe" ] && [ -x "$ffmpeg" ] || { echo "ffmpeg and ffprobe are needed: set FFMPEG and FFPROBE" >&2; exit 1; }
rex_port=${REX_PORT:-32720}
host_port=${HOST_PORT:-32730}
fake_port=${FAKE_PORT:-32740}
playlist=${PLAYLIST:-PL6B3937A5D230E335}   # Official Blender Open Movies
count=${COUNT:-2}
work=$(mktemp -d)
pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    if [ "${KEEP:-}" = 1 ]; then echo "kept $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
fail() {
    echo "FAIL: $*" >&2
    for log in host rex fake; do [ -f "$work/$log.log" ] && { echo "--- $log log" >&2; tail -40 "$work/$log.log" >&2; }; done
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

# The test copy: the addon as it is, without sign_in; a second copy with it, for the sign-in check.
mkdir -p "$work/bundled/youtube" "$work/bundled/youtube-signin"
cp addons/youtube/main.js "$work/bundled/youtube/"
cp addons/youtube/main.js "$work/bundled/youtube-signin/"
hosts='["www.youtube.com", "*.googlevideo.com"]'
if [ "${FAKE_YOUTUBE:-}" = 1 ]; then
    mkdir -p "$work/media"
    # One variant's media, muxed H.264 + AAC in fMP4 segments as YouTube's are.
    (cd "$work/media" && "$ffmpeg" -v error -f lavfi -i testsrc=size=640x360:rate=24 -f lavfi -i sine=frequency=440 -t 12 \
        -c:v libx264 -preset ultrafast -c:a aac -map 1:a -map 0:v -f hls -hls_time 4 -hls_playlist_type vod \
        -hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4 -hls_segment_filename 'seg%03d.m4s' index.m3u8)
    python3 tests/e2e-youtube-fake.py "$fake_port" "$work/media" >"$work/fake.url" 2>"$work/fake.log" &
    pids+=($!)
    for _ in $(seq 20); do [ -s "$work/fake.url" ] && break; sleep 0.5; done
    fake=$(cat "$work/fake.url")
    hosts="[\"$(printf '%s' "${fake#http://}" | cut -d: -f1)\"]"
    for copy in youtube youtube-signin; do
        perl -pi -e "s#https://www.youtube.com#$fake#; s/const CHECK_PAUSE_MS = 1500;/const CHECK_PAUSE_MS = 0;/" "$work/bundled/$copy/main.js"
    done
    echo "fake YouTube at $fake"
fi
python3 - "$work/bundled" "$hosts" <<'EOF'
import json, sys
bundled, hosts = sys.argv[1], json.loads(sys.argv[2])
manifest = json.load(open("addons/youtube/manifest.json"))
test = dict(manifest, hosts=hosts, name="YouTube (test)")
test.pop("sign_in")
json.dump(test, open(bundled + "/youtube/manifest.json", "w"))
json.dump(dict(manifest, hosts=hosts, id="youtube-signin", directories=[]), open(bundled + "/youtube-signin/manifest.json", "w"))
EOF

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
for _ in $(seq 30); do curl -fsS "$api/addons/youtube" >/dev/null 2>&1 && break; sleep 1; done
config="{\"count\": $count, \"playlists\": [\"https://www.youtube.com/playlist?list=$playlist\"]}"
curl -fsS -X PUT "$api/addons/youtube/config" -d "$config" >/dev/null || fail "the settings were refused"

echo "waiting for a sync with $count videos..."
for _ in $(seq 60); do
    addon=$(curl -fsS "$api/addons/youtube" 2>/dev/null || true)
    state=$(printf '%s' "$addon" | json 'd["state"]' 2>/dev/null || true)
    shown=$(printf '%s' "$addon" | json 'd["items"]' 2>/dev/null || echo 0)
    [ "$state" = error ] && fail "sync: $(printf '%s' "$addon" | json 'd["error"]')"
    [ "$state" = idle ] && [ "$shown" -ge "$count" ] && break
    sleep 5
done
[ "$shown" -ge "$count" ] || fail "rex shows $shown of $count videos after 5 minutes"
echo "status: $(printf '%s' "$addon" | json 'json.dumps({k: d[k] for k in ("state", "items", "held", "error")})')"
grep -q "probe failed" "$work/host.log" && fail "the host had to probe: the addon sent a video unprobed"

# The videos, in the playlist's order, titled as YouTube titles them.
items=$(curl -fsS "$rex/rex/feed/addon-youtube/items")
keys=$(printf '%s' "$items" | json '" ".join(str(i["ratingKey"]) for i in d.get("items", []) if not i.get("deleted"))')
listed=$(python3 -c "import json; print(' '.join(json.load(open('$work/data/addons/youtube/storage.json'))['listing']['lists'][0]['videos']))")
echo "the playlist's first $count videos: $listed"
for key in $keys; do
    curl -fsS -H "X-Plex-Token: $token" "$rex/library/metadata/$key" >"$work/meta-$key.xml"
    title=$(python3 -c 'import re,sys; print(re.search(r"<Video [^>]*\btitle=\"([^\"]*)\"", open(sys.argv[1]).read()).group(1))' "$work/meta-$key.xml")
    show=$(python3 -c 'import re,sys; print(re.search(r"<Video [^>]*\bgrandparentTitle=\"([^\"]*)\"", open(sys.argv[1]).read()).group(1))' "$work/meta-$key.xml")
    echo "  ratingKey $key: \"$title\", an episode of \"$show\""
    case "$title" in "" | *.m3u8 | Episode*) fail "item $key is not titled as YouTube titles it";; esac
done
curl -fsS -H "X-Plex-Token: $token" -H "Accept: application/json" "$rex/playlists" >"$work/playlists.json"
pl=$(json '[p["ratingKey"] for p in d["MediaContainer"].get("Metadata", []) if p["title"] == "Official Blender Open Movies"][0]' <"$work/playlists.json") ||
    fail "rex has no playlist Official Blender Open Movies: $(cat "$work/playlists.json")"
order=$(curl -fsS -H "X-Plex-Token: $token" -H "Accept: application/json" "$rex/playlists/$pl/items" |
    json '" ".join(str(m["ratingKey"]) for m in d["MediaContainer"].get("Metadata", []))')
want=$(printf '%s' "$items" | python3 -c "
import json, sys
by = {i['id']: str(i['ratingKey']) for i in json.load(sys.stdin)['items']}
print(' '.join(by['yt-' + v] for v in '$listed'.split() if 'yt-' + v in by))")
echo "playlist $pl holds $order (YouTube's order: $want)"
[ -n "$order" ] && [ "$order" = "$want" ] || fail "the playlist is not in YouTube's order"

# Play the first one as a Plex client plays an HLS Part: its read goes to a conversion.
first=${keys%% *}
part=$(python3 -c 'import re,sys; print(re.search(r"<Part [^>]*key=\"([^\"]+)\"", open(sys.argv[1]).read()).group(1))' "$work/meta-$first.xml")
location=$(curl -sS -o /dev/null -w '%{redirect_url}' "$rex$part?X-Plex-Token=$token")
echo "Part read $part -> ${location:0:80}..."
case "$location" in *transcode/universal/start.m3u8*) ;; *) fail "the Part read was not sent to a conversion: $location";; esac
curl -fsS "$location" >"$work/start.m3u8" || fail "start.m3u8 failed"
base=${location%%start.m3u8*}
media=$(grep -v '^#' "$work/start.m3u8" | head -1)
curl -fsS "$base$media" >"$work/index.m3u8" || fail "the conversion's playlist failed: $(cat "$work/start.m3u8")"
segment=$(grep -v '^#' "$work/index.m3u8" | head -1)
media_dir=${media%/*}
code=$(curl -sS -o "$work/segment.bin" -w '%{http_code}' --max-time 120 "$base$media_dir/$segment")
size=$(wc -c <"$work/segment.bin" | tr -d ' ')
decoded=$("$ffprobe" -v error -show_entries stream=codec_type -of csv=p=0 "$work/segment.bin" | sort -u | tr '\n' ' ')
echo "first segment: $code, $size bytes, streams: $decoded"
[ "$code" = 200 ] && [ "$size" -gt 10000 ] && [[ "$decoded" == *video* ]] || fail "no media came back"

# Sign-in, against Google (or the fake): a code and its address, still waiting a poll later, then cancelled.
started=$(curl -fsS -X POST "$api/addons/youtube-signin/sign-in") || fail "the sign-in did not start"
echo "sign-in: $(printf '%s' "$started" | json 'json.dumps({"code": "*" * len(d["sign_in"]["pending"]["code"]), "url": d["sign_in"]["pending"]["url"]})')"
printf '%s' "$started" | json 'd["sign_in"]["pending"]["url"]' | grep -q '^https://' || fail "no https address to enter the code at"
sleep 7
pending=$(curl -fsS "$api/addons/youtube-signin" | json 'str(d["sign_in"]["pending"] is not None) + " " + str(d["sign_in"]["account"]) + " " + str(d["sign_in"]["error"])')
echo "a poll later: pending, account, error = $pending"
[ "$pending" = "True None None" ] || fail "the sign-in did not stay pending"
cancelled=$(curl -fsS -X DELETE "$api/addons/youtube-signin/sign-in" | json 'str(d["sign_in"]["pending"])')
[ "$cancelled" = None ] || fail "the sign-in is still pending after it was cancelled"
echo "cancelled: pending is $cancelled"

curl -fsS -X DELETE "$api/addons/youtube" | tee "$work/removed.json"; echo
curl -s -o /dev/null -w '%{http_code}' "$rex/rex/sources/addon-youtube" | grep -q 404 || fail "rex still has the source"
echo "PASS"
