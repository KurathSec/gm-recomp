#!/bin/sh
# Runs a game page in a headless browser and prints what the page reports.
#   web/tools/run-browser.sh chrome|firefox '<query string>' [capture dir] [port]
# FFPREFS='name=value ...' sets Firefox preferences.
# PROFILE=<dir> keeps the browser profile (and with it the games' saves) across runs;
# flatpak Chrome only sees directories it may access, e.g. ~/.var/app/com.google.Chrome/.
set -e
browser=$1
query=$2
capdir=${3:-}
port=${4:-8770}
here=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
node "$here/web/dev/server.mjs" --port "$port" --report --timeout 900 ${capdir:+--capture-dir "$capdir"} > "$tmp/out" 2> "$tmp/err" &
server=$!
sleep 1
url="http://127.0.0.1:$port/?$query&report=1"
case $browser in
  chrome) flatpak run com.google.Chrome --headless=new --user-data-dir="${PROFILE:-$tmp/profile}" --no-first-run --autoplay-policy=no-user-gesture-required "$url" > "$tmp/browser.log" 2>&1 & ;;
  firefox)
    ff=${PROFILE:-$tmp}
    mkdir -p "$ff"
    echo 'user_pref("media.autoplay.default", 0); user_pref("media.autoplay.blocking_policy", 0);' > "$ff/user.js"
    # FFPREFS='name=value ...' adds preferences, e.g. javascript.options.wasm_baselinejit=false
    for p in ${FFPREFS:-}; do echo "user_pref(\"${p%%=*}\", ${p#*=});" >> "$ff/user.js"; done
    firefox --headless --no-remote --profile "$ff" "$url" > "$tmp/browser.log" 2>&1 & ;;
esac
browser_pid=$!
wait $server || true
kill $browser_pid 2>/dev/null || true
# Only this run's browser (its profile is in the command line), so runs can overlap.
# Chrome may ignore SIGTERM while the page is busy; the page has stored its saves
# before reporting, so it is killed after a grace period.
case $browser in
  chrome) pat="^/app/extra/chrome .*user-data-dir=${PROFILE:-$tmp/profile}" ;;
  *) pat="profile ${PROFILE:-$tmp}" ;;
esac
pkill -f "$pat" 2>/dev/null || true
n=0
while pgrep -f "$pat" > /dev/null && [ $n -lt 5 ]; do sleep 1; n=$((n + 1)); done
pkill -KILL -f "$pat" 2>/dev/null || true
cat "$tmp/out"
rm -rf "$tmp"
