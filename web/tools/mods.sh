#!/bin/sh
# Copies the mods' own files (images, scripts, sounds, DLLs) from a Fangame.ModLoader
# folder into web/public/mods/<mod>/, where the page fetches them (src/mods.mjs):
#   web/tools/mods.sh path/to/Fangame.ModLoader
# The mods' .NET assemblies and editor leftovers (.pdb, .import) are not needed.
set -e
src=${1:?usage: mods.sh <Fangame.ModLoader folder>}
here=$(cd "$(dirname "$0")/.." && pwd)
dst="$here/public/mods"
mkdir -p "$dst"
for dir in "$src"/Mods/*/; do
  mod=$(basename "$dir")
  rm -rf "${dst:?}/$mod"
  mkdir -p "$dst/$mod"
  (cd "$dir" && find . -type f ! -name "$mod.dll" ! -name '*.pdb' ! -name '*.import' ! -name 'Config.json' -exec cp --parents {} "$dst/$mod/" \;)
  echo "$mod: $(find "$dst/$mod" -type f | wc -l) files"
done
