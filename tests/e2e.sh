#!/usr/bin/env bash
# End to end against a real rex, a real addon repository and live archive.org: this repository,
# given as --default-repo, is fetched on the first start; the internet-archive addon is installed
# from it; its films, probed by the addon's own ffprobe() calls, reach rex's feed; a Range GET of
# one through rex's part path returns bytes; a repository with no addons is refused; and removing
# the addon removes its source from rex.
#
#   REX_BIN=/path/to/rex tests/e2e.sh
#   REX_BIN=/path/to/rex REX_ADDONS_BIN=/path/to/rex-addons tests/e2e.sh   # no cargo needed
#   REPO_URL='https://github.com/debridmediamanager/rex-addons#a-branch' ...  # another commit
#
# rex runs unclaimed on scratch ports with no state, no GDM, no plex.tv and no rex-direct; it
# serves Plex clients only with --token, which this script sets. Needs ffprobe and python3.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${REX_BIN:?Set REX_BIN to a rex binary (cargo build --release in the rex repository)}"
ffprobe=${FFPROBE:-$(command -v ffprobe || true)}
[ -x "$ffprobe" ] || { echo "ffprobe not found: set FFPROBE" >&2; exit 1; }
rex_port=${REX_PORT:-32700}
host_port=${HOST_PORT:-32710}
count=${COUNT:-4}
repo_url=${REPO_URL:-https://github.com/debridmediamanager/rex-addons}
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
    for log in host rex; do echo "--- $log log" >&2; tail -40 "$work/$log.log" >&2 || true; done
    exit 1
}

# REX_ADDONS_BIN runs a binary built elsewhere (a host without cargo); otherwise this tree is built.
bin=${REX_ADDONS_BIN:-}
if [ -z "$bin" ]; then
    cargo build --release --quiet
    bin=${CARGO_TARGET_DIR:-target}/release/rex-addons
fi
token="e2e-$RANDOM$RANDOM"
rex="http://127.0.0.1:$rex_port"
api="http://127.0.0.1:$host_port/api"

REX_DIRECT_URL=off REX_PLEXTV_URL=http://127.0.0.1:9 REX_TRANSCODER=off REX_ARTWORK_ANALYSIS=off \
    "$REX_BIN" --port "$rex_port" --catalog "$work/rex.db" --no-state --gdm off --token "$token" >"$work/rex.log" 2>&1 &
pids+=($!)
for _ in $(seq 60); do curl -fsS "$rex/identity" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "$rex/identity" >/dev/null || fail "rex did not start"

mkdir -p "$work/data/addons/internet-archive"
# A few films, so the probes are quick; the rest of the addon's settings are its defaults.
echo "{\"count\": $count}" >"$work/data/addons/internet-archive/config.json"
"$bin" --listen "127.0.0.1:$host_port" --rex "$rex" --data "$work/data" --ffprobe "$ffprobe" --default-repo "$repo_url" >"$work/host.log" 2>&1 &
pids+=($!)

for _ in $(seq 60); do
    repos=$(curl -fsS "$api/repos" 2>/dev/null || true)
    commit=$(printf '%s' "$repos" | json 'd[0]["commit"] or ""' 2>/dev/null || true)
    [ -n "$commit" ] && break
    sleep 1
done
[ -n "$commit" ] || fail "the default repository was not fetched: $repos"
repo_id=$(printf '%s' "$repos" | json 'd[0]["id"]')
echo "repository $repo_id at $commit: $(printf '%s' "$repos" | json '", ".join(a["id"] + " " + a["version"] for a in d[0]["addons"])')"
code=$(curl -sS -o "$work/installed.json" -w '%{http_code}' -X POST "$api/addons" -d "{\"repo\": \"$repo_id\", \"addon\": \"internet-archive\"}")
echo "install from the repository: $code $(json 'd["id"] + " " + d["version"] + " repo=" + str(d["repo"])' <"$work/installed.json")"
[ "$code" = 201 ] || fail "install answered $code: $(cat "$work/installed.json")"

echo "waiting for the first sync..."
for _ in $(seq 120); do
    status=$(curl -fsS "$api/status" 2>/dev/null || true)
    state=$(printf '%s' "$status" | json 'd["addons"][0]["state"] if d.get("addons") else ""' 2>/dev/null || true)
    synced=$(printf '%s' "$status" | json 'd["addons"][0]["last_sync"] or ""' 2>/dev/null || true)
    [ "$state" = error ] && fail "sync: $(printf '%s' "$status" | json 'd["addons"][0]["error"]')"
    [ -n "$synced" ] && break
    sleep 5
done
[ -n "$synced" ] || fail "no sync within 10 minutes"
echo "status: $(printf '%s' "$status" | json 'json.dumps(d["addons"][0])')"
printf '%s' "$status" | json 'd["rex"]["reachable"]' | grep -q True || fail "status says rex is not reachable"

items=$(curl -fsS "$rex/rex/feed/addon-internet-archive/items")
shown=$(printf '%s' "$items" | json 'len([i for i in d.get("items", []) if not i.get("deleted")])')
echo "rex shows $shown items from addon-internet-archive"
[ "$shown" -ge 1 ] || fail "rex shows no items: $items"
key=$(printf '%s' "$items" | json '[i for i in d["items"] if not i.get("deleted")][0]["ratingKey"]')
id=$(printf '%s' "$items" | json '[i for i in d["items"] if not i.get("deleted")][0]["id"]')

part=$(curl -fsS -H "X-Plex-Token: $token" "$rex/library/metadata/$key" |
    python3 -c 'import re,sys; print(re.search(r"<Part [^>]*key=\"([^\"]+)\"", sys.stdin.read()).group(1))')
echo "item $id is ratingKey $key, part $part"
code=$(curl -sS -o "$work/range.bin" -w '%{http_code}' -H "X-Plex-Token: $token" -H "Range: bytes=0-1023" "$rex$part")
size=$(wc -c <"$work/range.bin" | tr -d ' ')
echo "Range GET through rex: $code, $size bytes, starting $(head -c 12 "$work/range.bin" | od -An -tx1 | tr -d ' \n')"
[ "$code" = 206 ] && [ "$size" = 1024 ] || fail "expected 206 with 1024 bytes"

# The addon probed its films itself (ffprobe()), so this host probed none.
own=$(json 'len([k for k in d if k.startswith("p:")])' <"$work/data/addons/internet-archive/storage.json")
echo "probes the addon took itself: $own"
[ "$own" -ge "$shown" ] || fail "the addon probed $own of $shown films"
grep -q "probe failed" "$work/host.log" && fail "the host had to probe"

code=$(curl -sS -o "$work/refused.json" -w '%{http_code}' -X POST "$api/repos" -d '{"url": "https://codeberg.org/Codeberg/Community"}')
echo "a repository with no addons: $code $(json 'd["error"]' <"$work/refused.json")"
[ "$code" = 400 ] || fail "expected 400"

curl -fsS -X DELETE "$api/addons/internet-archive" | tee "$work/removed.json"; echo
curl -s -o /dev/null -w '%{http_code}' "$rex/rex/sources/addon-internet-archive" | grep -q 404 || fail "rex still has the source"
curl -fsS -X DELETE "$api/repos/$repo_id" >/dev/null
[ "$(curl -fsS "$api/repos")" = "[]" ] || fail "the repository is still listed"
echo "PASS"
