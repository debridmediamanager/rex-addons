#!/usr/bin/env bash
# The youtube addon end to end, against a real rex and live YouTube, signed in: a copy of the addon
# starts already signed in from a token file (signing in needs a person to enter a code, so the
# test seeds the addon's storage and the host's account with the tokens instead); its account's
# first videos reach rex with their titles, and the account's playlists exist in rex in YouTube's
# order; a video's 360p file is read through rex's Part path with Range, at its start and middle,
# as real MP4 bytes; a sign-in starts against Google through the app's API, stays pending and is
# cancelled; removing the addon removes its titles from rex.
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
# checks CHECKS of their videos (8 by default: one player call each); run it rarely from one
# address. rex runs unclaimed on scratch ports with no state, no GDM, no plex.tv and no rex-direct.
# Needs ffmpeg, ffprobe, python3 and, without REX_BIN, gh.
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

# The addon as it is, CHECKS videos a sync; a second copy, never signed in, for the sign-in check.
mkdir -p "$work/bundled/youtube" "$work/bundled/youtube-signin"
for copy in youtube youtube-signin; do
    perl -pe "s/const CHECKS_PER_SYNC = 30;/const CHECKS_PER_SYNC = $checks;/" addons/youtube/main.js >"$work/bundled/$copy/main.js"
done
hosts='["www.youtube.com", "*.googlevideo.com"]'
config="{\"count\": $count}"
token_file=${YT_TOKEN_FILE:-}
if [ "$fake_mode" = 1 ]; then
    mkdir -p "$work/media"
    # The 360p file, H.264 + AAC in one MP4, as YouTube's.
    "$ffmpeg" -v error -f lavfi -i testsrc=size=640x360:rate=24 -f lavfi -i sine=frequency=440 -t 12 -c:v libx264 -profile:v baseline \
        -pix_fmt yuv420p -preset ultrafast -c:a aac -movflags +faststart "$work/media/video.mp4"
    # YouTube's TV player script, which the addon cuts its n function out of.
    curl -fsS -o "$work/media/player.js" "https://www.youtube.com/s/player/f5321ed3/tv-player-ias.vflset/tv-player-ias.js"
    python3 tests/e2e-youtube-fake.py "$fake_port" "$work/media" >"$work/fake.url" 2>"$work/fake.log" &
    pids+=($!)
    for _ in $(seq 20); do [ -s "$work/fake.url" ] && break; sleep 0.5; done
    fake=$(cat "$work/fake.url")
    hosts="[\"$(printf '%s' "${fake#http://}" | cut -d: -f1)\"]"
    for copy in youtube youtube-signin; do
        perl -pi -e "s#https://www.youtube.com#$fake#; s/const CHECK_PAUSE_MS = 1500;/const CHECK_PAUSE_MS = 0;/" "$work/bundled/$copy/main.js"
    done
    printf '{"access_token": "fake-access", "refresh_token": "fake-refresh", "expires_in": 3600}' >"$work/fake-token.json"
    token_file="$work/fake-token.json"
    config="{\"count\": 2, \"liked\": false, \"watch_later\": false, \"own\": false, \"playlists\": [\"https://www.youtube.com/playlist?list=PL6B3937A5D230E335\"]}"
    echo "fake YouTube at $fake"
fi
python3 - "$work/bundled" "$hosts" <<'EOF'
import json, sys
bundled, hosts = sys.argv[1], json.loads(sys.argv[2])
manifest = json.load(open("addons/youtube/manifest.json"))
json.dump(dict(manifest, hosts=hosts), open(bundled + "/youtube/manifest.json", "w"))
json.dump(dict(manifest, hosts=hosts, id="youtube-signin", directories=[]), open(bundled + "/youtube-signin/manifest.json", "w"))
EOF
# Test-only: the addon starts signed in, with the test's settings, so its first sync (on start) is
# the test's. Its storage holds the tokens as signInPoll() keeps them, and the host's state the
# account, as a finished sign-in leaves them.
mkdir -p "$work/data/addons/youtube"
(umask 077 && python3 - "$token_file" "$work/data/addons/youtube" "$config" <<'EOF'
import json, sys, time
token, folder = json.load(open(sys.argv[1])), sys.argv[2]
json.dump(json.loads(sys.argv[3]), open(folder + "/config.json", "w"))
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
for _ in $(seq 30); do curl -fsS "$api/addons/youtube" >/dev/null 2>&1 && break; sleep 1; done
status=$(curl -fsS "$api/addons/youtube")
name=$(printf '%s' "$status" | json 'd["name"]')
label=$(printf '%s' "$status" | json 'd["sign_in"]["label"]')
echo "$name, \"$label\", signed in: $(printf '%s' "$status" | json 'd["sign_in"]["account"]')"
[ "$name" = "YouTube (360p)" ] && [ "$label" = "Sign in to YouTube" ] || fail "the addon is not named YouTube (360p) with its Sign in to YouTube button"

echo "waiting for a sync..."
for _ in $(seq 120); do
    addon=$(curl -fsS "$api/addons/youtube" 2>/dev/null || true)
    state=$(printf '%s' "$addon" | json 'd["state"]' 2>/dev/null || true)
    shown_items=$(printf '%s' "$addon" | json 'd["items"]' 2>/dev/null || echo 0)
    [ "$state" = error ] && fail "sync: $(printf '%s' "$addon" | json 'd["error"]')"
    [ "$state" = idle ] && [ "$shown_items" -ge 1 ] && break
    sleep 5
done
[ "$shown_items" -ge 1 ] || fail "rex shows no video after 10 minutes"
echo "status: $(printf '%s' "$addon" | json 'json.dumps({k: d[k] for k in ("state", "items", "held", "error")})')"
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
    pl=$(python3 -c "
import json, sys
found = [p['ratingKey'] for p in json.load(open('$work/playlists.json'))['MediaContainer'].get('Metadata', []) if p['title'] == sys.argv[1]]
print(found[0] if found else '')" "$title")
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

# A video's file through rex's Part path, as a Plex client reads it: its start and its middle.
first=${keys%% *}
part=$(python3 -c 'import re,sys; print(re.search(r"<Part [^>]*key=\"([^\"]+)\"", open(sys.argv[1]).read()).group(1))' "$work/meta-$first.xml")
size=$(python3 -c 'import re,sys; m = re.search(r"<Part [^>]*\bsize=\"(\d+)\"", open(sys.argv[1]).read()); print(m.group(1) if m else 0)' "$work/meta-$first.xml")
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
curl -s -o /dev/null -w '%{http_code}' "$rex/rex/sources/addon-youtube" | grep -q 404 || fail "rex still has the source"
echo "PASS"
