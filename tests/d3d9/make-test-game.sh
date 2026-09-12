#!/bin/sh
# Builds shader_test.exe and lays it out as a game (web/public/games/zz-test-d3d9-shaders)
# with Wine's d3dcompiler_43 and the DLLs it needs, as the importer supplies them.
#   tests/d3d9/make-test-game.sh [wine i386-windows dir]
# Then: harness run web/public/games/zz-test-d3d9-shaders --frames 40
#   or: web/tools/run-browser.sh chrome 'game=zz-test-d3d9-shaders&frames=40&nopace=1'
set -e
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
wine=${1:-/usr/lib64/wine-wow64/wine/i386-windows}
out="$repo/web/public/games/zz-test-d3d9-shaders"
rm -rf "$out"
mkdir -p "$out/files/C/Windows/system32"
i686-w64-mingw32-gcc -O2 -o "$out/files/shader_test.exe" "$here/shader_test.c" -ld3d9
for dll in d3dcompiler_43.dll ucrtbase.dll wined3d.dll; do
  objcopy --strip-debug "$wine/$dll" "$out/files/C/Windows/system32/$dll" 2>/dev/null || cp "$wine/$dll" "$out/files/C/Windows/system32/$dll"
done
cd "$out/files"
{
  printf '{"slug": "zz-test-d3d9-shaders", "title": "D3D9 shader test", "entry": "shader_test.exe", "files": ['
  sep=''
  find . -type f | sed 's|^\./||' | sort | while read -r f; do
    printf '%s{"path": "%s", "size": %s}' "$sep" "$f" "$(stat -c %s "$f")"
    sep=', '
  done
  printf ']}\n'
} > "$out/manifest.json"
echo "wrote $out"
