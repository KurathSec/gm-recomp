#!/bin/sh
# Prepares web/public/system32: the system DLLs a game imported in the browser may
# need that it does not ship and the runtime does not emulate (redistributable C/C++
# runtimes, the shader compiler, D3DX 9 for GM 8.2's gm82dx9, and what they import in
# turn), from Wine's i386 PE builds with their debug sections stripped. The page's
# importer (src/import.mjs) fetches them from /system32/<name>, as the native importer
# copies them.
#   web/tools/system32.sh [wine i386-windows dir]
set -e
src=${1:-/usr/lib64/wine-wow64/wine/i386-windows}
here=$(cd "$(dirname "$0")/.." && pwd)
dst=$here/public/system32
mkdir -p "$dst"
# emulated by the runtime (runtime::hle::kernel32 CORE_DLLS and HLE_DLLS)
emulated=" kernel32 kernelbase user32 gdi32 ntdll advapi32 ole32 oleaut32 shell32 comctl32 comdlg32 winmm version imm32 msimg32 uxtheme dwmapi ws2_32 wsock32 mpr winspool.drv crypt32 psapi shlwapi setupapi usp10 wininet dbghelp olepro32 imagehlp urlmon gdiplus windowscodecs hid opengl32 d3d8 d3d9 ddraw dsound dinput8 dinput dplayx xinput1_3 xinput1_4 xinput9_1_0 msvcrt "
queue=$(cd "$src" && ls | grep -iE '^(msvcr|msvcp|vcruntime|concrt|ucrtbase|d3dcompiler_|d3dx9_43|msacm32|rpcrt4)' | grep -iE '\.dll$')
done_list=""
while [ -n "$queue" ]; do
  set -- $queue
  name=$1
  shift
  queue="$*"
  case " $done_list " in *" $name "*) continue ;; esac
  done_list="$done_list $name"
  [ -f "$src/$name" ] || continue
  cp "$src/$name" "$dst/$name"
  objcopy --strip-debug "$dst/$name" 2>/dev/null || true
  # objcopy stamps the PE header with the time and a new checksum: the original's
  # keep the copies (and games imported with them) the same from run to run
  python3 -I -c '
import sys
a, b = open(sys.argv[1], "rb").read(), bytearray(open(sys.argv[2], "rb").read())
pe = int.from_bytes(b[0x3c:0x40], "little")
for o in (pe + 8, pe + 24 + 64):
    b[o:o + 4] = a[o:o + 4]
open(sys.argv[2], "wb").write(b)' "$src/$name" "$dst/$name"
  for dep in $(objdump -p "$dst/$name" 2>/dev/null | awk '/DLL Name:/ {print tolower($3)}'); do
    stem=${dep%.dll}
    case "$emulated" in *" $stem "*) continue ;; esac
    case "$dep" in api-ms-win-*) continue ;; esac
    queue="$queue $dep"
  done
done
ls "$dst" | wc -l
du -sh "$dst"
