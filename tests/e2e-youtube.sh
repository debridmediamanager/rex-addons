#!/usr/bin/env bash
# The youtube addon end to end, against a real rex with FFmpeg and live YouTube, signed in: a copy
# of the addon starts already signed in from a token file (signing in needs a person to enter a
# code, so the test seeds the addon's storage and the host's account with the tokens instead); its
# account's first videos reach rex with their titles, and the account's playlists exist in rex in
# YouTube's order; the sharpest video plays as Plex Web plays it, rex copying its H.264 picture and
# AAC sound into a conversion, from its start and after a seek to its middle, both there in every
# segment; a second copy set to 360 plays one video's 360p file through rex's Part path with Range;
# a sign-in starts against Google through the app's API, stays pending and is cancelled; removing
# the addon removes its titles from rex. With FAKE_YOUTUBE=1 it first runs 1.0.0, the 360p
# version, and updates it: the titles keep their keys and watch state and play at 1080p.
#
#   YT_TOKEN_FILE=token.json tests/e2e-youtube.sh   # the latest rex nightly for this Mac, downloaded
#   REX_BIN=/path/to/rex YT_TOKEN_FILE=token.json tests/e2e-youtube.sh
#   FAKE_YOUTUBE=1 tests/e2e-youtube.sh             # YouTube's answers from tests/e2e-youtube-fake.py,
#                                                   # with a token of its own, and media FFmpeg makes
#
# The token file is YouTube's answer to the TV sign-in ({access_token, refresh_token, expires_in}).
# The script copies it only into its own work directory, which it deletes; run it with TMPDIR
# pointing where such a copy may go. It prints no title from the account, only their lengths.
#
# Live, it asks YouTube for the account's playlists (COUNT videos of each, 5 by default) and
# checks CHECKS of their videos (8 by default: one player call each), plus one public playlist's
# first video for the 360 copy and one player call per play; run it rarely from one address. rex
# runs unclaimed on scratch ports with no state, no GDM, no plex.tv and no rex-direct.
# Needs ffmpeg, ffprobe, python3 and, without REX_BIN, gh. Prints TIMINGS for each play.
set -euo pipefail
cd "$(dirname "$0")/.."
ffprobe=${FFPROBE:-$(command -v ffprobe || true)}
ffmpeg=${FFMPEG:-$(command -v ffmpeg || true)}
[ -x "$ffprobe" ] && [ -x "$ffmpeg" ] || { echo "ffmpeg and ffprobe are needed: set FFMPEG and FFPROBE" >&2; exit 1; }
fake_mode=${FAKE_YOUTUBE:-}
[ "$fake_mode" = 1 ] || [ -r "${YT_TOKEN_FILE:-}" ] || { echo "live, YT_TOKEN_FILE must name a token file" >&2; exit 1; }
rex_port=${REX_PORT:-32720}
host_port=${HOST_PORT:-32730}
fake_port=${FAKE_PORT:-32740}
count=${COUNT:-5}
checks=${CHECKS:-8}
work=$(mktemp -d "${TMPDIR:-/tmp}/e2e-youtube.XXXXXX")
pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    if [ "${KEEP:-}" = 1 ]; then echo "kept $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
# A title from the account is shown by its length only.
shown() { if [ "$fake_mode" = 1 ]; then printf '"%s"' "$1"; else printf '(%s characters)' "${#1}"; fi; }
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

# The addon as it is, CHECKS videos a sync; a copy set to 360 for one public playlist's first
# video; and one never signed in, for the sign-in check.
copies="youtube youtube-sd youtube-signin"
for copy in $copies; do
    mkdir -p "$work/bundled/$copy"
    perl -pe "s/const CHECKS_PER_SYNC = 30;/const CHECKS_PER_SYNC = $checks;/" community/addons/youtube/main.js >"$work/bundled/$copy/main.js"
done
hosts='["www.youtube.com", "*.googlevideo.com"]'
config="{\"count\": $count}"
public='"https://www.youtube.com/playlist?list=PL6B3937A5D230E335"'
config_sd="{\"count\": 1, \"max_height\": 360, \"liked\": false, \"watch_later\": false, \"own\": false, \"playlists\": [$public]}"
token_file=${YT_TOKEN_FILE:-}
if [ "$fake_mode" = 1 ]; then
    mkdir -p "$work/media"
    # As YouTube's: the 360p file, H.264 + AAC in one MP4; and the H.264 picture (with B-frames, as
    # YouTube's) and the AAC sound apart, each an MP4 of 7 s fragments, its index (sidx) after its
    # header. Six minutes, so a seek to the middle is past what rex makes ahead and inside a fragment.
    "$ffmpeg" -v error -f lavfi -i testsrc=size=640x360:rate=12 -f lavfi -i sine=frequency=440 -t 360 -c:v libx264 -profile:v baseline \
        -pix_fmt yuv420p -preset ultrafast -c:a aac -movflags +faststart "$work/media/muxed.mp4"
    "$ffmpeg" -v error -f lavfi -i testsrc=size=1280x720:rate=12 -t 360 -c:v libx264 -preset veryfast -bf 2 -g 84 -keyint_min 84 -sc_threshold 0 \
        -pix_fmt yuv420p -f mp4 -movflags dash+global_sidx -frag_duration 7000000 "$work/media/dash-video.mp4"
    "$ffmpeg" -v error -f lavfi -i sine=frequency=440 -t 360 -c:a aac -f mp4 -movflags dash+global_sidx -frag_duration 7000000 "$work/media/dash-audio.mp4"
    # YouTube's TV player script, which the addon cuts its n function out of.
    curl -fsS -o "$work/media/player.js" "https://www.youtube.com/s/player/f5321ed3/tv-player-ias.vflset/tv-player-ias.js"
    python3 tests/e2e-youtube-fake.py "$fake_port" "$work/media" >"$work/fake.url" 2>"$work/fake.log" &
    pids+=($!)
    for _ in $(seq 20); do [ -s "$work/fake.url" ] && break; sleep 0.5; done
    fake=$(cat "$work/fake.url")
    hosts="[\"$(printf '%s' "${fake#http://}" | cut -d: -f1)\"]"
    for copy in $copies; do
        perl -pi -e "s#https://www.youtube.com#$fake#; s/const CHECK_PAUSE_MS = 1500;/const CHECK_PAUSE_MS = 0;/" "$work/bundled/$copy/main.js"
    done
    printf '{"access_token": "fake-access", "refresh_token": "fake-refresh", "expires_in": 3600}' >"$work/fake-token.json"
    token_file="$work/fake-token.json"
    config="{\"count\": 2, \"liked\": false, \"watch_later\": false, \"own\": false, \"playlists\": [$public]}"
    echo "fake YouTube at $fake"
fi
python3 - "$work/bundled" "$hosts" <<'EOF'
import json, sys
bundled, hosts = sys.argv[1], json.loads(sys.argv[2])
manifest = json.load(open("community/addons/youtube/manifest.json"))
json.dump(dict(manifest, hosts=hosts), open(bundled + "/youtube/manifest.json", "w"))
json.dump(dict(manifest, hosts=hosts, id="youtube-sd"), open(bundled + "/youtube-sd/manifest.json", "w"))
json.dump(dict(manifest, hosts=hosts, id="youtube-signin", directories=[]), open(bundled + "/youtube-signin/manifest.json", "w"))
EOF
# Test-only: the addon and its 360 copy start signed in, with the test's settings, so their first
# sync (on start) is the test's. Their storage holds the tokens as signInPoll() keeps them, and the
# host's state the account, as a finished sign-in leaves them.
for seeded in "youtube" "youtube-sd"; do
    mkdir -p "$work/data/addons/$seeded"
    seeded_config=$config
    [ "$seeded" = youtube-sd ] && seeded_config=$config_sd
    (umask 077 && python3 - "$token_file" "$work/data/addons/$seeded" "$seeded_config" <<'EOF'
import json, sys, time
token, folder = json.load(open(sys.argv[1])), sys.argv[2]
json.dump(json.loads(sys.argv[3]), open(folder + "/config.json", "w"))
tokens = {"access": token["access_token"], "refresh": token["refresh_token"], "expires_at": int((time.time() + int(token.get("expires_in", 3600)) - 600) * 1000)}
json.dump({"tokens": tokens}, open(folder + "/storage.json", "w"))
json.dump({"enabled": True, "account": "signed in from a token file"}, open(folder + "/state.json", "w"))
EOF
    )
done

token="e2e-$RANDOM$RANDOM"
rex="http://127.0.0.1:$rex_port"
api="http://127.0.0.1:$host_port/api"
REX_DIRECT_URL=off REX_PLEXTV_URL=http://127.0.0.1:9 REX_TRANSCODER="$ffmpeg" REX_ARTWORK_ANALYSIS=off \
    "$REX_BIN" --port "$rex_port" --catalog "$work/rex.db" --no-state --gdm off --token "$token" >"$work/rex.log" 2>&1 &
pids+=($!)
for _ in $(seq 60); do curl -fsS "$rex/identity" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "$rex/identity" >/dev/null || fail "rex did not start"

start_host() {
    "$bin" --listen "127.0.0.1:$host_port" --rex "$rex" --data "$work/data" --ffprobe "$ffprobe" --bundled "$1" >>"$work/host.log" 2>&1 &
    host_pid=$!
    pids+=($host_pid)
    for _ in $(seq 30); do curl -fsS "$api/addons/youtube" >/dev/null 2>&1 && break; sleep 1; done
}
# Waits for a sync of addon $1 that ends with a video in rex; with $2 = again, one it starts now.
synced() {
    local before last
    before=$(curl -fsS "$api/addons/$1" | json 'd["last_sync"] or 0')
    [ "${2:-}" = again ] && curl -fsS -X POST "$api/addons/$1/sync" >/dev/null
    for _ in $(seq 200); do
        addon=$(curl -fsS "$api/addons/$1" 2>/dev/null || true)
        state=$(printf '%s' "$addon" | json 'd["state"]' 2>/dev/null || true)
        shown_items=$(printf '%s' "$addon" | json 'd["items"]' 2>/dev/null || echo 0)
        last=$(printf '%s' "$addon" | json 'd["last_sync"] or 0' 2>/dev/null || echo 0)
        [ "$state" = error ] && fail "$1 sync: $(printf '%s' "$addon" | json 'd["error"]')"
        if [ "$state" = idle ] && [ "$shown_items" -ge 1 ] && { [ "${2:-}" != again ] || [ "$last" != "$before" ]; }; then break; fi
        sleep 3
    done
    [ "$shown_items" -ge 1 ] || fail "rex shows no video of $1 after 10 minutes"
    echo "$1 synced: $(printf '%s' "$addon" | json 'json.dumps({k: d[k] for k in ("version", "items", "held", "error")})')"
}
items_of() { curl -fsS "$rex/rex/feed/addon-$1/items"; }
plex() { curl -fsS -H "X-Plex-Token: $token" -H "Accept: application/json" "$rex$1"; }

if [ "$fake_mode" = 1 ]; then
    # 1.0.0, the 360p version, as people have it: its titles in rex, one of them watched.
    mkdir -p "$work/old/youtube"
    git show 74ec68c:addons/youtube/main.js | perl -pe "s#https://www.youtube.com#$fake#; s/const CHECK_PAUSE_MS = 1500;/const CHECK_PAUSE_MS = 0;/" >"$work/old/youtube/main.js"
    git show 74ec68c:addons/youtube/manifest.json | python3 -c "import json,sys; json.dump(dict(json.load(sys.stdin), hosts=json.loads(sys.argv[1])), sys.stdout)" "$hosts" >"$work/old/youtube/manifest.json"
    start_host "$work/old"
    synced youtube
    old_items=$(items_of youtube)
    printf '%s' "$old_items" | json '" ".join(i["file"] for i in d["items"])' | grep -q '\.mp4' || fail "1.0.0 sent no .mp4"
    watched=$(printf '%s' "$old_items" | json '[i for i in d["items"] if not i.get("deleted")][0]["ratingKey"]')
    curl -fsS -H "X-Plex-Token: $token" "$rex/:/scrobble?key=$watched&identifier=com.plexapp.plugins.library" >/dev/null
    kill "$host_pid"
    wait "$host_pid" 2>/dev/null || true
    echo "1.0.0: $(printf '%s' "$old_items" | json 'len(d["items"])') titles in rex, ratingKey $watched watched; updating"
fi

start_host "$work/bundled"
status=$(curl -fsS "$api/addons/youtube")
name=$(printf '%s' "$status" | json 'd["name"]')
label=$(printf '%s' "$status" | json 'd["sign_in"]["label"]')
echo "$name $(printf '%s' "$status" | json 'd["version"]'), \"$label\", signed in: $(printf '%s' "$status" | json 'd["sign_in"]["account"]')"
[ "$name" = "YouTube" ] && [ "$label" = "Sign in to YouTube" ] || fail "the addon is not named YouTube with its Sign in to YouTube button"
[ "$(printf '%s' "$status" | json 'd["sign_in"]["account"]')" = "signed in from a token file" ] || fail "the update did not keep the sign-in"

echo "waiting for a sync..."
if [ "$fake_mode" = 1 ]; then synced youtube again; else synced youtube; fi
synced youtube-sd
grep -q "probe failed" "$work/host.log" && fail "the host had to probe: the addon sent a video unprobed"
grep -E "cut out|youtubei/player" "$work/host.log" | sed -E 's/.*\[addon youtube\] //' | sed -E 's/"videoId":"[^"]+"/"videoId":…/' | sort | uniq -c

# The videos, titled as YouTube titles them, each an episode of its channel.
items=$(curl -fsS "$rex/rex/feed/addon-youtube/items")
keys=$(printf '%s' "$items" | json '" ".join(str(i["ratingKey"]) for i in d.get("items", []) if not i.get("deleted"))')
for key in $keys; do
    curl -fsS -H "X-Plex-Token: $token" "$rex/library/metadata/$key" >"$work/meta-$key.xml"
    title=$(python3 -c 'import re,sys,html; print(html.unescape(re.search(r"<Video [^>]*\btitle=\"([^\"]*)\"", open(sys.argv[1]).read()).group(1)))' "$work/meta-$key.xml")
    show=$(python3 -c 'import re,sys,html; print(html.unescape(re.search(r"<Video [^>]*\bgrandparentTitle=\"([^\"]*)\"", open(sys.argv[1]).read()).group(1)))' "$work/meta-$key.xml")
    echo "  ratingKey $key: $(shown "$title"), an episode of $(shown "$show")"
    case "$title" in "" | *.mp4 | *.m3u8 | Episode*) fail "item $key is not titled as YouTube titles it";; esac
    grep -q 'librarySectionTitle="YouTube"' "$work/meta-$key.xml" || fail "item $key is not in the library called YouTube"
done

# Each playlist in rex, in YouTube's order of the videos that reached rex.
curl -fsS -H "X-Plex-Token: $token" -H "Accept: application/json" "$rex/playlists" >"$work/playlists.json"
python3 - "$work/data/addons/youtube/storage.json" "$work/playlists.json" <<'EOF' >"$work/want-playlists.txt"
import json, sys
listing = json.load(open(sys.argv[1]))["listing"]
for at, playlist in enumerate(listing["lists"]):
    print(at, " ".join(v["id"] if isinstance(v, dict) else v for v in playlist["videos"]))
EOF
checked=0
while read -r at videos; do
    want=$(printf '%s' "$items" | python3 -c "
import json, sys
by = {i['id']: str(i['ratingKey']) for i in json.load(sys.stdin)['items'] if not i.get('deleted')}
print(' '.join(by['yt-' + v] for v in '$videos'.split() if 'yt-' + v in by))")
    title=$(python3 -c "import json; print(json.load(open('$work/data/addons/youtube/storage.json'))['listing']['lists'][$at]['title'])")
    # Not the 360 copy's playlist of the same name (with FAKE_YOUTUBE=1 both take the same one).
    sd_keys=$(items_of youtube-sd | json '" ".join(str(i["ratingKey"]) for i in d["items"])')
    pl=$(python3 -c "
import json, sys, urllib.request
found = [p['ratingKey'] for p in json.load(open('$work/playlists.json'))['MediaContainer'].get('Metadata', []) if p['title'] == sys.argv[1]]
def keys(pl):
    request = urllib.request.Request('$rex/playlists/%s/items' % pl, headers={'X-Plex-Token': '$token', 'Accept': 'application/json'})
    return {str(m['ratingKey']) for m in json.load(urllib.request.urlopen(request))['MediaContainer'].get('Metadata', [])}
found = [pl for pl in found if not keys(pl) & set(sys.argv[2].split())]
print(found[0] if found else '')" "$title" "$sd_keys")
    if [ -z "$want" ]; then
        [ -z "$pl" ] || [ "$(curl -fsS -H "X-Plex-Token: $token" -H "Accept: application/json" "$rex/playlists/$pl/items" | json 'len(d["MediaContainer"].get("Metadata", []))')" = 0 ] ||
            fail "playlist $at has items in rex though none of its videos reached rex"
        echo "  playlist $at $(shown "$title"): none of its videos reached rex yet"
        continue
    fi
    [ -n "$pl" ] || fail "rex has no playlist $at $(shown "$title")"
    order=$(curl -fsS -H "X-Plex-Token: $token" -H "Accept: application/json" "$rex/playlists/$pl/items" |
        json '" ".join(str(m["ratingKey"]) for m in d["MediaContainer"].get("Metadata", []))')
    echo "  playlist $at $(shown "$title"): rex $pl holds $order (YouTube's order: $want)"
    [ "$order" = "$want" ] || fail "playlist $at is not in YouTube's order"
    checked=$((checked + 1))
done <"$work/want-playlists.txt"
[ "$checked" -ge 1 ] || fail "no playlist reached rex"

# The sharpest video, played as Plex Web plays it: rex copies its picture and sound, from its start
# and after a seek to its middle.
pick=$(python3 - "$work/data/addons/youtube/storage.json" "$items" <<'EOF'
import json, sys
storage, items = json.load(open(sys.argv[1])), json.loads(sys.argv[2])["items"]
by = {i["id"]: i for i in items if not i.get("deleted")}
# The sharpest, and of those the longest, so a seek to its middle is past what rex made ahead (90 s).
pairs = sorted(((s["height"], float(v["probe"]["format"]["duration"]), k[2:]) for k, v in storage.items()
                if k.startswith("v:") and v.get("kind") == "pair" and "yt-" + k[2:] in by
                for s in v["probe"]["streams"] if s["codec_type"] == "video"), reverse=True)
print("%s %s" % (by["yt-" + pairs[0][2]]["ratingKey"], pairs[0][0]) if pairs else "")
EOF
)
[ -n "$pick" ] || fail "no video plays from a picture and sound pair"
echo "playing ratingKey ${pick% *} (${pick#* } lines) as Plex Web does:"
python3 tests/e2e-youtube-play.py "$rex" "$token" "${pick% *}" "${pick#* }" "$ffprobe" "$work" || fail "the picture and sound pair did not play"
grep -E "TRANSCODE e2e-yt-${pick% *}-[0-9]+ (run|start)" "$work/rex.log" | sed -E 's/^[0-9:]+ INFO +//' || true
grep -qE "TRANSCODE e2e-yt-${pick% *}-[0-9]+ run 2 from segment [1-9]" "$work/rex.log" || fail "the seek did not restart rex's FFmpeg at the middle"
if [ "$fake_mode" = 1 ]; then
    plex "/library/metadata/$watched" >"$work/watched.json"
    python3 - "$work/watched.json" <<'EOF' || fail "the update lost the watched title, or it did not take the new streams"
import json, sys
row = json.load(open(sys.argv[1]))["MediaContainer"]["Metadata"][0]
part = row["Media"][0]["Part"][0]
streams = [(s.get("streamType"), s.get("codec"), s.get("height")) for s in part["Stream"]]
print("after the update, ratingKey %s: viewCount %s, %s, streams %s" % (row["ratingKey"], row.get("viewCount"), part["file"].rsplit("/", 1)[-1], streams))
assert row.get("viewCount") == 1 and part["file"].endswith(".m3u8")
assert (1, "h264", 720) in streams and (2, "aac", None) in streams
EOF
fi

# The 360 copy's video, its 360p file through rex's Part path, as a Plex client reads it: its start
# and its middle, and what ffprobe finds there.
sd=$(items_of youtube-sd | json '[i for i in d["items"] if not i.get("deleted")][0]["ratingKey"]')
plex "/library/metadata/$sd" >"$work/sd.json"
part=$(json 'd["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0]["key"]' <"$work/sd.json")
size=$(json 'd["MediaContainer"]["Metadata"][0]["Media"][0]["Part"][0].get("size", 0)' <"$work/sd.json")
case "$part" in *.mp4) ;; *) fail "the 360 copy's video is not its MP4 file: $part" ;; esac
probed=$("$ffprobe" -v error -show_entries stream=codec_name,height -of csv=p=0 "$rex$part?X-Plex-Token=$token" | tr '\n' ' ')
echo "360 copy: ratingKey $sd, $probed"
case "$probed" in "h264,"*" aac"*) ;; *) fail "the 360p file is not H.264 + AAC through rex" ;; esac
for range in "0-65535" "$((size / 2))-$((size / 2 + 65535))"; do
    out=$(curl -sS -L -o "$work/range.bin" -w '%{http_code} %{size_download} %{speed_download}' -H "X-Plex-Token: $token" -H "Range: bytes=$range" "$rex$part")
    echo "Part $part, bytes $range of $size: $out (status, bytes, bytes/s)"
    [ "${out%% *}" = 206 ] || fail "the Part read of bytes $range answered ${out%% *}"
    [ "$(echo "$out" | cut -d' ' -f2)" = 65536 ] || fail "the Part read of bytes $range did not return 65536 bytes"
    if [ "${range%%-*}" = 0 ]; then
        [ "$(head -c 8 "$work/range.bin" | tail -c 4)" = ftyp ] || fail "the file does not start as an MP4"
    fi
done

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
curl -fsS -X DELETE "$api/addons/youtube-sd" >/dev/null
curl -s -o /dev/null -w '%{http_code}' "$rex/rex/sources/addon-youtube" | grep -q 404 || fail "rex still has the source"
echo "PASS"
