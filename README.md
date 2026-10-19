# gmrecomp

Runs Windows GameMaker games (GameMaker 8.0/8.1, GameMaker: Studio 1.4 VM and YYC) in
desktop browsers by recompiling their original executables. The x86 code of the
runner, the game (YYC) and every DLL the game ships is translated to WebAssembly; the
Windows APIs it calls (kernel32, user32, gdi32, Direct3D 8/9, DirectSound,
DirectMusic, MCI, XInput, ...) are implemented in Rust. Nothing is re-implemented
from GameMaker itself, so games behave as they do on Windows, x87 rounding included.

## Requirements

- Rust (stable for the native tools; nightly for the browser build:
  `rustup toolchain install nightly`, `rustup target add wasm32-unknown-unknown --toolchain nightly`)
- Node.js 22+ (dev server, tests), a Chromium-based browser or Firefox
- `7z` (importing IExpress/CAB and RAR archives), `objcopy` (optional, strips the
  system DLLs it supplies), `gcc` with 32-bit support (the CPU differential tests
  run a small native reference program)
- Wine's 32-bit PE DLLs (`/usr/lib64/wine-wow64/wine/i386-windows`, used by the
  importer for the few runtime DLLs games need but do not ship)
- Optional: a General MIDI SoundFont as `web/soundfont.sf2` (e.g. a link to
  `/usr/share/soundfonts/default.sf2`), for games with MIDI music

## Using it

```sh
# import games (folders, .zip, .rar, .exe) into web/public/games/<slug>/
cargo run --release -p importer -- import path/to/game.zip --out web/public/games

# build the browser runtime (without DWARF: 3.4 MB instead of 21; the name section,
# which profiles and stack traces use, stays)
CARGO_PROFILE_RELEASE_DEBUG=0 rustup run nightly cargo build --release --target wasm32-unknown-unknown -p web --target-dir target/wasm

# serve it (cross-origin isolation headers included) and open http://localhost:8766/
node web/dev/server.mjs
```

The page lists the imported games; a game plays at its own speed with keyboard,
mouse and game controllers, sound starts with the first click or key press, and
saves are kept in the browser (export/import from the game page).

Games can also be imported in the browser, from the archive they are stored as:
"Import a game…" on the page, or `?zip=<url>` (for example a link to the archive in
object storage), which imports and starts it. A zip, 7z or RAR works, with an
IExpress package inside unwrapped (NSIS installers still need the native importer). The game is kept in the browser and plays with `?game=<slug>&local`.
This needs two extra builds:

```sh
# the importer for the page (served at /import/)
CARGO_PROFILE_RELEASE_DEBUG=0 rustup run nightly cargo build --release --target wasm32-unknown-unknown -p webimport --target-dir target/wasm-import
# the system DLLs it may add (served at /system32/; from Wine's PE builds)
web/tools/system32.sh
```

Each game's card (and the bar while it plays) has a **Mods** button: changes made to
the game's files before it starts, only those its engine allows. *Debug mode* turns on
the game's debug switch (GameMaker: Studio, VM or YYC; patcher.rs's patch).
Fangame.ModLoader's mods are reimplemented for GameMaker 8 and GameMaker: Studio 1.4 VM
games (not YYC, not GameMaker Studio 2 yet): GodMode, CrimsonKid, DotKid, DbgHelper
(gm8x_fix's patches; the sleep margin for GameMaker: Studio), AllowFullscreen and
PlayOnline (GameMaker 8 only). A GameMaker: Studio data.win is changed in place where
sizes allow and grows at its end otherwise (`crates/gmmod/src/gms.rs` says how). The
mods' code is `crates/gmmod`; their own files come from a Fangame.ModLoader folder:

```sh
# the mods for the page (served at /gmmod/)
CARGO_PROFILE_RELEASE_DEBUG=0 rustup run nightly cargo build --release --target wasm32-unknown-unknown -p gmmod --target-dir target/wasm-mods
# their files (served at /mods/)
web/tools/mods.sh path/to/Fangame.ModLoader
```

A game's network connections (Winsock) go through a WebSocket relay you run, since
a browser cannot open TCP or UDP itself; PlayOnline's settings name it (`ws://…`):

```sh
node web/tools/relay.mjs --port 8790 --allow iwannastartaparty.com
```

The browser also remembers which code each game needed and translates it right
after the first frame of the next session (`?noprofile` turns that off), so later
sessions do not stall the first time they reach a room or effect. A game's first
session starts from what other games built with the same runner needed, or from the
site's profiles in `web/public/profiles` until there are any. To remake those:

```sh
# per run (any games, recorded sessions with --replay), then merged
node web/tools/run-node.mjs web/public/games/<slug> --frames 1000 --module-profiles runs/<slug>
node web/tools/module-profiles.mjs web/public/profiles runs/*/
```

## How it works

```
game.exe + DLLs ──discovery──► functions ──recomp (x86 → wasm, one module per batch)──┐
                                                                                        ▼
browser worker: runtime.wasm (Rust: engine, HLE APIs, x87 model) + cpu glue + x87 fast paths
   │ guest memory = its own wasm memory      │ D3D draw records ──► WebGL2 (gpu.mjs)
   │ virtual clock, deterministic threads   │ sound mix ──► SharedArrayBuffer ring ──► AudioWorklet
```

- `crates/recomp`: the x86 → wasm translator (lazy flags, register locals, blocks
  laid out so loops are wasm loops and other jumps forward branches, small callees
  inlined, SSE on wasm SIMD, common x87 instructions inline with an exact 80-bit
  model behind them).
- `crates/fpu`: the x87 model (rustc_apfloat; bit-exact in every precision and
  rounding mode).
- `crates/runtime`: PE loader, process state, the HLE layer (`hle/`), the engine
  (dispatcher, SEH, API calls, threads).
- `crates/web`: the browser build of the runtime; `web/`: page, worker, WebGL2
  renderer, audio worklet.
- `crates/harness`: runs the same runtime natively on wasmtime (games headless, state
  hashes, WAV output, traces) and the CPU differential tests against the host CPU.
- `crates/importer`: unpacks games, detects the engine, inventories modules and APIs;
  `crates/webimport`: the same unpacking and detection for the page.

Runs are deterministic: the clock is virtual, threads are scheduled cooperatively,
and the same input gives the same state hash in the harness, Node, Chrome and Firefox.

## Status

| Game | Engine | Verified so far |
|---|---|---|
| Not Another Needle Game | GM 8.1 | mouse menus; New Game starts the game; music (supersound.dll on DirectSound) |
| i wanna uhuhu spike | GM 8.0 | the kid runs and jumps in the hub; first stage; MP3 music (MCI), sound effects (DirectMusic) |
| I wanna run the Marathon | GMS 1.4 VM | file select, hub, the marathon starts; title music from the GMS audio thread; Box2D golf (via a test copy) |
| Crimson Needle 3 | GMS 1.4 VM | title, save select, new game (walk right into the opening); music |
| indigo needle | GMS 1.4 VM + GMFile.dll | title room, the player moves (extension runs on Wine's msvcp100) |
| untitled needle game | GMS 1.4 YYC | intro and tutorial: run, jump; shaders compiled by Wine's d3dcompiler |
| RNG Dungeon | GMS 1.4 YYC | intro, file select, a save chosen; all 22 shaders translate to GLSL |
| I Wanna be the Vandal | GMS 1.4 VM | held out until the end: title, save select, new game, out of the box |
| I wanna Bread | GM 8.1 (Release Candidate runner) | held out until the end: title, data select, the hub room, out of the box |

See `docs/` for each milestone's notes (`r1-cpu.md` CPU and tests, `r2-harness.md`
the native harness, `r3-r5-runtime.md` the browser runtime, audio, threads,
Direct3D 9 and shaders).

## Tests

`tests/d3d9/make-test-game.sh` builds a Direct3D 9 shader test that runs like a game
(`game=zz-test-d3d9-shaders`); `tests/gms/` has tools for GMS `data.win` files (rooms,
a rough GML bytecode lister, test copies with patched bytecode).

```sh
cargo test --release                                   # unit tests
cargo run --release -p harness -- cpu-diff --mode x87fast --count 5000   # CPU vs. host
cargo run --release -p harness -- run web/public/games/<slug> --frames 300
web/tools/run-browser.sh chrome 'game=<slug>&frames=300&epoch=1767225600&nopace=1'
```
