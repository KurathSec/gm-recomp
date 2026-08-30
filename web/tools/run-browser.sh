#!/bin/sh
# Runs a game page in a headless browser and prints what the page reports.
#   web/tools/run-browser.sh chrome|firefox '<query string>' [capture dir] [port]
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
  chrome) flatpak run com.google.Chrome --headless=new --user-data-dir="$tmp/profile" --no-first-run "$url" > "$tmp/browser.log" 2>&1 & ;;
  firefox) firefox --headless --no-remote --profile "$tmp" "$url" > "$tmp/browser.log" 2>&1 & ;;
esac
browser_pid=$!
wait $server || true
kill $browser_pid 2>/dev/null || true
[ "$browser" = chrome ] && flatpak kill com.google.Chrome 2>/dev/null || true
pkill -f "profile $tmp" 2>/dev/null || true
cat "$tmp/out"
rm -rf "$tmp"
